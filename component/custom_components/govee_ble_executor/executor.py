"""Job queue, worker pool and MQTT plumbing.

Home Assistant provides no BLE queue of its own — `establish_connection` does not
wait for a free connection slot, it fails and retries with backoff — so the
throttling has to live here. A small fixed worker pool is the whole mechanism:
with the default of one worker there is never more than one connect in flight,
which is the regime habluetooth's path scorer is happiest in.

Scheduling proper (coalescing, backoff, circuit breaking) belongs to the add-on;
this side only enforces a ceiling and reports what the proxies say about their
slots.
"""

from __future__ import annotations

import asyncio
import itertools
import json
import logging
import time
from typing import Any

from homeassistant.components import mqtt
from homeassistant.core import HomeAssistant, callback

from .const import (
    DEFAULT_IDLE_TIMEOUT,
    DEFAULT_MAX_CONCURRENT,
    DEFAULT_TOPIC_PREFIX,
    TOPIC_REQUEST,
    TOPIC_RESPONSE,
    TOPIC_STATUS,
)
from .protocol import (
    ErrorKind,
    JobRequest,
    JobResponse,
    ProtocolError,
    parse_request,
)
from .session import BleSession, SessionError

_LOGGER = logging.getLogger(__name__)

_PRIORITY_RANK = {"user": 0, "poll": 1}


class BleExecutor:
    """Owns the MQTT subscription, the job queue and one session per device."""

    def __init__(
        self,
        hass: HomeAssistant,
        topic_prefix: str = DEFAULT_TOPIC_PREFIX,
        max_concurrent: int = DEFAULT_MAX_CONCURRENT,
        idle_timeout: float = DEFAULT_IDLE_TIMEOUT,
    ) -> None:
        self._hass = hass
        self._prefix = topic_prefix.rstrip("/")
        self._max_concurrent = max(1, max_concurrent)
        self._idle_timeout = idle_timeout
        self._queue: asyncio.PriorityQueue[tuple[int, int, JobRequest, float]] = (
            asyncio.PriorityQueue()
        )
        self._sequence = itertools.count()
        self._sessions: dict[str, BleSession] = {}
        self._workers: list[asyncio.Task[None]] = []
        self._unsubscribe: list[Any] = []

    @property
    def _topic_request(self) -> str:
        return f"{self._prefix}/{TOPIC_REQUEST}"

    @property
    def _topic_response(self) -> str:
        return f"{self._prefix}/{TOPIC_RESPONSE}"

    @property
    def _topic_status(self) -> str:
        return f"{self._prefix}/{TOPIC_STATUS}"

    async def async_start(self) -> None:
        self._unsubscribe.append(
            await mqtt.async_subscribe(self._hass, self._topic_request, self._on_request)
        )
        self._workers = [
            self._hass.async_create_background_task(
                self._worker(index), f"govee_ble_executor worker {index}"
            )
            for index in range(self._max_concurrent)
        ]
        self._subscribe_to_slot_changes()
        await self.async_publish_status(online=True)
        _LOGGER.info("listening on %s with %d worker(s)", self._topic_request, self._max_concurrent)

    async def async_stop(self) -> None:
        for unsubscribe in self._unsubscribe:
            unsubscribe()
        self._unsubscribe.clear()

        for worker in self._workers:
            worker.cancel()
        self._workers.clear()

        await asyncio.gather(
            *(session.async_stop() for session in self._sessions.values()),
            return_exceptions=True,
        )
        self._sessions.clear()
        await self.async_publish_status(online=False)

    @callback
    def _on_request(self, message: mqtt.ReceiveMessage) -> None:
        """Parse an incoming job and queue it.

        A malformed request is answered rather than dropped: the add-on is
        waiting on a correlation id and would otherwise sit out its deadline.
        """
        try:
            payload = json.loads(message.payload)
        except ValueError as err:
            # Without a correlation id there is nobody to answer.
            _LOGGER.warning("discarding request that is not valid JSON: %s", err)
            return

        try:
            job = parse_request(payload)
        except ProtocolError as err:
            _LOGGER.warning("rejecting malformed request: %s", err)
            job_id = str(payload.get("id", "")) if isinstance(payload, dict) else ""
            self._hass.async_create_task(
                self._respond(
                    JobResponse(
                        id=job_id,
                        ok=False,
                        error_kind=ErrorKind.BAD_REQUEST,
                        error_message=str(err),
                    )
                )
            )
            return

        rank = _PRIORITY_RANK.get(job.priority, 1)
        self._queue.put_nowait((rank, next(self._sequence), job, time.monotonic()))

    async def _worker(self, index: int) -> None:
        while True:
            _rank, _seq, job, queued_at = await self._queue.get()
            try:
                await self._process(job, queued_at)
            except asyncio.CancelledError:
                raise
            except Exception:  # a worker must outlive one bad job
                _LOGGER.exception("worker %d failed on job %s", index, job.id)
            finally:
                self._queue.task_done()

    async def _process(self, job: JobRequest, queued_at: float) -> None:
        started = time.monotonic()
        waited_ms = int((started - queued_at) * 1000)

        # Answer honestly rather than starting work the add-on has given up on.
        if job.deadline_ms and waited_ms >= job.deadline_ms:
            await self._respond(
                JobResponse(
                    id=job.id,
                    ok=False,
                    duration_ms=waited_ms,
                    error_kind=ErrorKind.TIMEOUT,
                    error_message=(
                        f"job waited {waited_ms}ms in the queue, past its "
                        f"{job.deadline_ms}ms deadline"
                    ),
                )
            )
            return

        session = self._sessions.get(job.address)
        if session is None:
            session = BleSession(self._hass, job.address, self._idle_timeout)
            self._sessions[job.address] = session

        try:
            results = await session.run(job)
        except SessionError as err:
            _LOGGER.debug("[%s] job %s failed: %s", job.address, job.id, err.message)
            response = JobResponse(
                id=job.id,
                ok=False,
                duration_ms=int((time.monotonic() - started) * 1000),
                error_kind=err.kind,
                error_message=err.message,
                retry_after_ms=err.retry_after_ms,
            )
        else:
            response = JobResponse(
                id=job.id,
                ok=True,
                results=results,
                duration_ms=int((time.monotonic() - started) * 1000),
            )

        await self._respond(response)
        await self.async_publish_status(online=True)

    async def _respond(self, response: JobResponse) -> None:
        await mqtt.async_publish(
            self._hass, self._topic_response, json.dumps(response.to_dict()), 0, False
        )

    def _subscribe_to_slot_changes(self) -> None:
        """Republish status whenever a proxy's slot usage changes."""
        try:
            from habluetooth import get_manager  # noqa: PLC0415
        except ImportError:  # pragma: no cover - habluetooth ships with HA
            return

        try:
            manager = get_manager()
        except RuntimeError:
            return

        @callback
        def _on_allocation(_allocation: Any) -> None:
            self._hass.async_create_task(self.async_publish_status(online=True))

        try:
            self._unsubscribe.append(manager.async_register_allocation_callback(_on_allocation))
        except AttributeError:
            _LOGGER.debug("this Home Assistant does not expose slot allocations")

    def _slot_report(self) -> list[dict[str, Any]]:
        try:
            from habluetooth import get_manager  # noqa: PLC0415

            allocations = get_manager().async_current_allocations() or []
        except (ImportError, RuntimeError, AttributeError):
            return []

        # slots == 0 means "this scanner has not reported yet", not "exhausted",
        # so reporting it would make the add-on throttle for no reason.
        return [
            {
                "source": allocation.source,
                "slots": allocation.slots,
                "free": allocation.free,
                "allocated": list(allocation.allocated),
            }
            for allocation in allocations
            if allocation.slots > 0
        ]

    async def async_publish_status(self, *, online: bool) -> None:
        payload = {
            "online": online,
            "max_concurrent": self._max_concurrent,
            "idle_timeout_s": self._idle_timeout,
            "queue_depth": self._queue.qsize(),
            "proxies": self._slot_report() if online else [],
        }
        await mqtt.async_publish(self._hass, self._topic_status, json.dumps(payload), 0, True)
