"""Wire format between the govee2mqtt add-on and this executor.

Deliberately free of Home Assistant imports so it can be unit tested on its own.

The add-on owns all Govee protocol knowledge and all scheduling; this executor
only knows "connect to a MAC, write these bytes, optionally hand back what the
device says". A request therefore describes a whole session rather than a single
write, so that a burst of commands costs one connection instead of several.
"""

from __future__ import annotations

import base64
from dataclasses import dataclass, field
from enum import StrEnum
from typing import Any


class ProtocolError(ValueError):
    """A request could not be understood."""


class ErrorKind(StrEnum):
    """Why a job failed.

    The add-on's scheduler branches on this: `OUT_OF_SLOTS` means try again
    shortly, `NOT_FOUND` means the device is not reachable right now, and
    `GATT_ERROR` means the link worked but the device refused the operation.
    """

    BAD_REQUEST = "bad_request"
    NOT_FOUND = "not_found"
    OUT_OF_SLOTS = "out_of_slots"
    CONNECT_FAILED = "connect_failed"
    GATT_ERROR = "gatt_error"
    TIMEOUT = "timeout"
    INTERNAL = "internal"


@dataclass(slots=True, frozen=True)
class WriteOp:
    """Write bytes to a characteristic.

    `expect_response` selects a GATT write-with-response. Govee's write
    characteristic is write-without-response, so this is normally False; the
    device acknowledges nothing and the write returns as soon as it is queued.
    """

    char: str
    data: bytes
    expect_response: bool = False


@dataclass(slots=True, frozen=True)
class DelayOp:
    """Wait before continuing.

    Govee devices drop commands sent too closely together; roughly 200ms between
    writes is the empirically established minimum.
    """

    delay_ms: int


@dataclass(slots=True, frozen=True)
class QueryOp:
    """Write a request and wait for the device to notify a reply."""

    write_char: str
    notify_char: str
    data: bytes
    timeout_ms: int = 5000
    optional: bool = False
    """Whether silence is an acceptable answer.

    Some questions are only worth asking speculatively -- "do you have
    segments?" is answered by a device that does, and ignored by one that does
    not. Without this the silence fails the whole job, which took a working
    Bluetooth-only light out of service every poll.
    """


Op = WriteOp | DelayOp | QueryOp


@dataclass(slots=True, frozen=True)
class JobRequest:
    """One session's worth of work against a single device."""

    id: str
    address: str
    ops: tuple[Op, ...]
    keep_open_ms: int = 0
    deadline_ms: int = 20000
    priority: str = "user"


@dataclass(slots=True)
class JobResponse:
    """Outcome of a job, returned on the response topic."""

    id: str
    ok: bool
    results: list[dict[str, Any]] = field(default_factory=list)
    duration_ms: int = 0
    error_kind: ErrorKind | None = None
    error_message: str | None = None
    retry_after_ms: int | None = None

    def to_dict(self) -> dict[str, Any]:
        payload: dict[str, Any] = {
            "id": self.id,
            "ok": self.ok,
            "duration_ms": self.duration_ms,
        }
        if self.ok:
            payload["results"] = self.results
        else:
            error: dict[str, Any] = {
                "kind": str(self.error_kind or ErrorKind.INTERNAL),
                "message": self.error_message or "",
            }
            if self.retry_after_ms is not None:
                error["retry_after_ms"] = self.retry_after_ms
            payload["error"] = error
        return payload


def _require(mapping: Any, key: str, kind: type) -> Any:
    if not isinstance(mapping, dict):
        raise ProtocolError(f"expected an object, got {type(mapping).__name__}")
    if key not in mapping:
        raise ProtocolError(f"missing required field {key!r}")
    value = mapping[key]
    if not isinstance(value, kind):
        raise ProtocolError(f"field {key!r} should be {kind.__name__}, got {type(value).__name__}")
    return value


def _decode_data(raw: str) -> bytes:
    try:
        return base64.b64decode(raw, validate=True)
    except (ValueError, TypeError) as err:
        raise ProtocolError(f"data is not valid base64: {err}") from err


def _parse_op(raw: Any) -> Op:
    if not isinstance(raw, dict):
        raise ProtocolError(f"an op should be an object, got {type(raw).__name__}")

    if "write" in raw:
        spec = raw["write"]
        return WriteOp(
            char=_require(spec, "char", str),
            data=_decode_data(_require(spec, "data", str)),
            expect_response=bool(spec.get("response", False)),
        )

    if "delay_ms" in raw:
        delay = raw["delay_ms"]
        if not isinstance(delay, int) or isinstance(delay, bool) or delay < 0:
            raise ProtocolError("delay_ms should be a non-negative integer")
        return DelayOp(delay_ms=delay)

    if "query" in raw:
        spec = raw["query"]
        return QueryOp(
            write_char=_require(spec, "write_char", str),
            notify_char=_require(spec, "notify_char", str),
            data=_decode_data(_require(spec, "data", str)),
            timeout_ms=int(spec.get("timeout_ms", 5000)),
            optional=bool(spec.get("optional", False)),
        )

    raise ProtocolError(f"unrecognised op: {sorted(raw)}")


def parse_request(payload: Any) -> JobRequest:
    """Turn a decoded JSON payload into a `JobRequest`.

    Raises `ProtocolError` with a message worth sending back to the add-on.
    """

    if not isinstance(payload, dict):
        raise ProtocolError(f"expected an object, got {type(payload).__name__}")

    raw_ops = _require(payload, "ops", list)
    if not raw_ops:
        raise ProtocolError("a job needs at least one op")

    return JobRequest(
        id=_require(payload, "id", str),
        address=_require(payload, "address", str).upper(),
        ops=tuple(_parse_op(op) for op in raw_ops),
        keep_open_ms=int(payload.get("keep_open_ms", 0)),
        deadline_ms=int(payload.get("deadline_ms", 20000)),
        priority=str(payload.get("priority", "user")),
    )


def notify_result(data: bytes) -> dict[str, Any]:
    """Result entry for a query op."""
    return {"kind": "notify", "data": base64.b64encode(data).decode("ascii")}
