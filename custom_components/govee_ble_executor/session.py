"""A connection to one BLE device, reused across jobs.

Modelled on the `led-ble` library, which is the canonical shape for this in Home
Assistant: a connect lock, an operation lock, and a timer that drops the
connection once it has been idle for a while. We deliberately do not reconnect on
an unexpected disconnect — the next job will reconnect, and holding a connection
we are not using costs one of a proxy's typically three slots.
"""

from __future__ import annotations

import asyncio
import logging
from typing import Any, Final

from bleak.exc import BleakError
from bleak_retry_connector import (
    BleakClientWithServiceCache,
    BleakConnectionError,
    BleakNotFoundError,
    BleakOutOfConnectionSlotsError,
    establish_connection,
)
from homeassistant.components import bluetooth
from homeassistant.core import HomeAssistant

from .const import OUT_OF_SLOTS_RETRY_MS
from .protocol import DelayOp, ErrorKind, JobRequest, Op, QueryOp, WriteOp, notify_result

_LOGGER = logging.getLogger(__name__)

MAX_CONNECT_ATTEMPTS: Final = 2

# bleak's own default is 20s per attempt, which does not fit a job's budget
# twice over: the first attempt would consume it almost entirely and the second
# would be cut off part way, wasting the time without ever getting a fair try.
# A proxy-backed connect that is going to succeed does so in a few seconds.
CONNECT_TIMEOUT: Final = 12.0


class SessionError(Exception):
    """A job failed, with enough context for the add-on to decide what to do."""

    def __init__(
        self,
        kind: ErrorKind,
        message: str,
        retry_after_ms: int | None = None,
    ) -> None:
        super().__init__(message)
        self.kind = kind
        self.message = message
        self.retry_after_ms = retry_after_ms


def _reachability_hint(hass: HomeAssistant, address: str) -> str:
    """Explain why a device cannot be reached, if Home Assistant can tell us.

    Since 2026 this reports per-proxy slot usage, which is exactly what makes an
    "it did not work" log actionable. Guarded because it is a newer API.
    """
    try:
        return bluetooth.async_address_reachability_diagnostics(
            hass, address, bluetooth.BluetoothReachabilityIntent.CONNECTION
        )
    except Exception as err:  # noqa: BLE001 - diagnostics must never break a job
        _LOGGER.debug("reachability diagnostics unavailable: %s", err)
        return ""


class BleSession:
    """Serialises access to a single device and owns its connection."""

    def __init__(self, hass: HomeAssistant, address: str, idle_timeout: float) -> None:
        self._hass = hass
        self._address = address
        self._idle_timeout = idle_timeout
        self._client: BleakClientWithServiceCache | None = None
        self._connect_lock = asyncio.Lock()
        self._operation_lock = asyncio.Lock()
        self._disconnect_timer: asyncio.TimerHandle | None = None
        self._expected_disconnect = False

    @property
    def address(self) -> str:
        return self._address

    @property
    def connected(self) -> bool:
        return self._client is not None and self._client.is_connected

    async def run(self, job: JobRequest) -> list[dict[str, Any]]:
        """Execute a job's ops in order, returning one result per op."""
        async with self._operation_lock:
            await self._ensure_connected()
            assert self._client is not None

            results: list[dict[str, Any]] = []
            try:
                for op in job.ops:
                    results.append(await self._run_op(self._client, op))
            except SessionError:
                # The link is suspect; drop it so the next job starts clean.
                await self._disconnect_now()
                raise
            except BleakError as err:
                await self._disconnect_now()
                raise SessionError(ErrorKind.GATT_ERROR, str(err)) from err

            self._schedule_disconnect(job.keep_open_ms)
            return results

    async def _run_op(self, client: BleakClientWithServiceCache, op: Op) -> dict[str, Any]:
        if isinstance(op, DelayOp):
            await asyncio.sleep(op.delay_ms / 1000)
            return {"kind": "delay"}

        if isinstance(op, WriteOp):
            await client.write_gatt_char(op.char, op.data, op.expect_response)
            return {"kind": "write"}

        if isinstance(op, QueryOp):
            return notify_result(await self._query(client, op))

        raise SessionError(ErrorKind.BAD_REQUEST, f"unsupported op {type(op).__name__}")

    async def _query(self, client: BleakClientWithServiceCache, op: QueryOp) -> bytes:
        """Write a request and wait for the device to notify a reply.

        The notification has to be subscribed before the write goes out, and
        unsubscribed afterwards: bleak raises if the same handle is subscribed
        twice, and a stale subscription would leak into the next job.
        """
        loop = asyncio.get_running_loop()
        future: asyncio.Future[bytes] = loop.create_future()

        def _on_notify(_characteristic: Any, data: bytearray) -> None:
            if not future.done():
                future.set_result(bytes(data))

        await client.start_notify(op.notify_char, _on_notify)
        try:
            await client.write_gatt_char(op.write_char, op.data, False)
            async with asyncio.timeout(op.timeout_ms / 1000):
                return await future
        except TimeoutError as err:
            raise SessionError(
                ErrorKind.TIMEOUT,
                f"no notification from {self._address} within {op.timeout_ms}ms",
            ) from err
        finally:
            try:
                await client.stop_notify(op.notify_char)
            except (BleakError, EOFError) as err:
                _LOGGER.debug("[%s] stop_notify failed: %s", self._address, err)

    async def _ensure_connected(self) -> None:
        if self.connected:
            self._cancel_disconnect()
            return

        async with self._connect_lock:
            if self.connected:
                self._cancel_disconnect()
                return

            # Always resolve the device afresh. A cached BLEDevice carries the
            # scanner it was seen by, and bleak-retry-connector's
            # ble_device_callback has been inert for some time, so a stale object
            # simply fails to connect.
            device = bluetooth.async_ble_device_from_address(
                self._hass, self._address, connectable=True
            )
            if device is None:
                hint = _reachability_hint(self._hass, self._address)
                raise SessionError(
                    ErrorKind.NOT_FOUND,
                    f"{self._address} has not been seen by a connectable scanner. {hint}".strip(),
                )

            self._log_signal()

            try:
                self._client = await establish_connection(
                    BleakClientWithServiceCache,
                    device,
                    self._address,
                    self._on_disconnected,
                    max_attempts=MAX_CONNECT_ATTEMPTS,
                    use_services_cache=True,
                    timeout=CONNECT_TIMEOUT,
                )
            except BleakOutOfConnectionSlotsError as err:
                raise SessionError(
                    ErrorKind.OUT_OF_SLOTS,
                    f"{err}. {_reachability_hint(self._hass, self._address)}".strip(),
                    retry_after_ms=OUT_OF_SLOTS_RETRY_MS,
                ) from err
            except BleakNotFoundError as err:
                raise SessionError(
                    ErrorKind.NOT_FOUND,
                    f"{err}. {_reachability_hint(self._hass, self._address)}".strip(),
                ) from err
            except (BleakConnectionError, BleakError, TimeoutError) as err:
                raise SessionError(ErrorKind.CONNECT_FAILED, str(err)) from err

            self._expected_disconnect = False
            _LOGGER.debug("[%s] connected", self._address)

    def _log_signal(self) -> None:
        """Note how well we can hear the device before trying to talk to it.

        A connect that times out looks identical whether the device is barely in
        range or simply refusing, and the signal level is what tells them apart.
        """
        try:
            info = bluetooth.async_last_service_info(self._hass, self._address, connectable=True)
        except Exception as err:  # noqa: BLE001 - diagnostics must never break a job
            _LOGGER.debug("[%s] no service info available: %s", self._address, err)
            return

        if info is None:
            _LOGGER.debug("[%s] no advertisement on record", self._address)
        else:
            _LOGGER.debug(
                "[%s] rssi %s dBm via %s, last seen %s",
                self._address,
                info.rssi,
                info.source,
                info.time,
            )

    def _on_disconnected(self, _client: BleakClientWithServiceCache) -> None:
        if self._expected_disconnect:
            _LOGGER.debug("[%s] disconnected", self._address)
        else:
            _LOGGER.debug("[%s] disconnected unexpectedly", self._address)
        self._client = None

    def _schedule_disconnect(self, keep_open_ms: int) -> None:
        self._cancel_disconnect()
        delay = self._idle_timeout if keep_open_ms <= 0 else keep_open_ms / 1000
        if delay <= 0:
            self._hass.async_create_task(self._disconnect_now())
            return
        self._disconnect_timer = self._hass.loop.call_later(
            delay, lambda: self._hass.async_create_task(self._disconnect_now())
        )

    def _cancel_disconnect(self) -> None:
        if self._disconnect_timer is not None:
            self._disconnect_timer.cancel()
            self._disconnect_timer = None

    async def _disconnect_now(self) -> None:
        self._cancel_disconnect()
        client = self._client
        self._client = None
        if client is None:
            return
        self._expected_disconnect = True
        try:
            await client.disconnect()
        except (BleakError, EOFError) as err:
            _LOGGER.debug("[%s] disconnect failed: %s", self._address, err)

    async def async_stop(self) -> None:
        await self._disconnect_now()
