"""Tests for the add-on <-> executor wire format."""

from __future__ import annotations

import base64

import pytest
from protocol import (
    DelayOp,
    ErrorKind,
    JobResponse,
    ProtocolError,
    QueryOp,
    WriteOp,
    notify_result,
    parse_request,
)

WRITE_CHAR = "00010203-0405-0607-0809-0a0b0c0d2b11"
NOTIFY_CHAR = "00010203-0405-0607-0809-0a0b0c0d2b10"
POWER_ON = bytes.fromhex("3301010000000000000000000000000000000033")


def _request(**overrides):
    payload = {
        "id": "job-1",
        "address": "60:74:f4:2b:2e:a5",
        "ops": [
            {
                "write": {
                    "char": WRITE_CHAR,
                    "data": base64.b64encode(POWER_ON).decode(),
                }
            }
        ],
    }
    payload.update(overrides)
    return payload


def test_parses_a_write_job():
    job = parse_request(_request())

    assert job.id == "job-1"
    assert job.ops == (WriteOp(char=WRITE_CHAR, data=POWER_ON, expect_response=False),)


def test_normalises_the_address_case():
    # The add-on and Home Assistant disagree on MAC casing; sessions are keyed by
    # address, so a mismatch would silently open two connections to one device.
    assert parse_request(_request()).address == "60:74:F4:2B:2E:A5"


def test_defaults_are_applied():
    job = parse_request(_request())

    assert job.priority == "user"
    assert job.keep_open_ms == 0
    assert job.deadline_ms == 20000


def test_parses_delay_and_query_ops():
    job = parse_request(
        _request(
            ops=[
                {"delay_ms": 200},
                {
                    "query": {
                        "write_char": WRITE_CHAR,
                        "notify_char": NOTIFY_CHAR,
                        "data": base64.b64encode(b"\xaa\x01").decode(),
                        "timeout_ms": 1500,
                    }
                },
            ]
        )
    )

    assert job.ops == (
        DelayOp(delay_ms=200),
        QueryOp(
            write_char=WRITE_CHAR,
            notify_char=NOTIFY_CHAR,
            data=b"\xaa\x01",
            timeout_ms=1500,
        ),
    )


def test_write_response_flag_is_honoured():
    job = parse_request(
        _request(
            ops=[
                {
                    "write": {
                        "char": WRITE_CHAR,
                        "data": base64.b64encode(POWER_ON).decode(),
                        "response": True,
                    }
                }
            ]
        )
    )

    assert job.ops[0].expect_response is True


@pytest.mark.parametrize(
    ("payload", "expected"),
    [
        pytest.param({"address": "AA", "ops": [{"delay_ms": 1}]}, "id", id="missing id"),
        pytest.param({"id": "x", "ops": [{"delay_ms": 1}]}, "address", id="missing address"),
        pytest.param({"id": "x", "address": "AA"}, "ops", id="missing ops"),
        pytest.param({"id": "x", "address": "AA", "ops": []}, "at least one", id="empty ops"),
    ],
)
def test_missing_fields_are_rejected(payload, expected):
    with pytest.raises(ProtocolError, match=expected):
        parse_request(payload)


def test_rejects_an_unknown_op():
    with pytest.raises(ProtocolError, match="unrecognised op"):
        parse_request(_request(ops=[{"teleport": {}}]))


def test_rejects_data_that_is_not_base64():
    with pytest.raises(ProtocolError, match="base64"):
        parse_request(_request(ops=[{"write": {"char": WRITE_CHAR, "data": "not base64!"}}]))


def test_rejects_a_negative_delay():
    with pytest.raises(ProtocolError, match="non-negative"):
        parse_request(_request(ops=[{"delay_ms": -1}]))


def test_rejects_a_boolean_delay():
    # bool is a subclass of int in Python, so this needs an explicit guard.
    with pytest.raises(ProtocolError, match="non-negative"):
        parse_request(_request(ops=[{"delay_ms": True}]))


def test_rejects_a_non_object_payload():
    with pytest.raises(ProtocolError, match="expected an object"):
        parse_request(["not", "an", "object"])


def test_successful_response_carries_results():
    response = JobResponse(
        id="job-1", ok=True, results=[notify_result(b"\xaa\x01\x01")], duration_ms=42
    )

    assert response.to_dict() == {
        "id": "job-1",
        "ok": True,
        "duration_ms": 42,
        "results": [{"kind": "notify", "data": base64.b64encode(b"\xaa\x01\x01").decode()}],
    }


def test_failed_response_carries_a_typed_error():
    response = JobResponse(
        id="job-1",
        ok=False,
        duration_ms=4021,
        error_kind=ErrorKind.OUT_OF_SLOTS,
        error_message="no free slot",
        retry_after_ms=4000,
    )

    assert response.to_dict() == {
        "id": "job-1",
        "ok": False,
        "duration_ms": 4021,
        "error": {
            "kind": "out_of_slots",
            "message": "no free slot",
            "retry_after_ms": 4000,
        },
    }


def test_failed_response_omits_the_retry_hint_when_there_is_none():
    response = JobResponse(
        id="job-1", ok=False, error_kind=ErrorKind.GATT_ERROR, error_message="nope"
    )

    assert "retry_after_ms" not in response.to_dict()["error"]


def _query_request(**query_overrides):
    query = {
        "write_char": WRITE_CHAR,
        "notify_char": NOTIFY_CHAR,
        "data": base64.b64encode(b"\xaa\xa5\x01").decode(),
    }
    query.update(query_overrides)
    return _request(ops=[{"query": query}])


def test_a_query_is_not_optional_by_default():
    """Silence keeps failing a job unless the caller says otherwise."""
    job = parse_request(_query_request())

    assert isinstance(job.ops[0], QueryOp)
    assert job.ops[0].optional is False


def test_a_query_can_be_marked_optional():
    """Asking whether a device has segments is speculative: one that has none
    ignores it, and that silence must not take the whole session down. It did
    -- a working Bluetooth-only light was set aside on every poll."""
    job = parse_request(_query_request(optional=True))

    assert job.ops[0].optional is True


def test_a_query_can_ask_to_stop_the_job_on_silence():
    """Paged data: the first unanswered page says there are no more, so the
    remaining questions would each cost a full timeout to learn nothing."""
    job = parse_request(_query_request(optional=True, stop_if_unanswered=True))

    assert job.ops[0].optional is True
    assert job.ops[0].stop_if_unanswered is True


def test_stopping_on_silence_is_off_by_default():
    job = parse_request(_query_request())

    assert job.ops[0].stop_if_unanswered is False
