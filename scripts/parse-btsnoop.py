#!/usr/bin/env python3
"""Read Govee frames out of an Android Bluetooth HCI snoop log.

Watching what the Govee app writes over Bluetooth is the only way to learn a
command the app can send and we cannot. The cloud is no substitute: commands
travel to a device's own topic, which the broker will not let us subscribe to,
and all the account topic carries is a receipt with the payload zeroed. Every
frame this project could not guess -- per-segment brightness, per-segment
colour temperature -- came out of a capture like this in minutes, after weeks of
probing had produced nothing.

Two mistakes cost real time on the way to this script, and both are handled
here so nobody repeats them:

**Attribute every frame to its connection.** A phone holds several links at
once, and grepping a whole capture for anything that looks like a Govee frame
mixes them. That is how a frame from a floor lamp was written up as coming from
a garden spot. Handles are mapped to addresses from the LE Connection Complete
events, so `--device` really means that device.

**Reassemble L2CAP fragments.** At the default ATT MTU a write longer than
twenty bytes arrives in pieces, and reading only the first packet shows the
first twenty bytes of it. That nearly settled a question about frame length in
exactly the wrong direction.

Usage:

    scripts/parse-btsnoop.py btsnoop_hci.log                 # everything
    scripts/parse-btsnoop.py btsnoop_hci.log --connections   # who was talking
    scripts/parse-btsnoop.py btsnoop_hci.log --device AA:BB:CC:DD:EE:FF
    scripts/parse-btsnoop.py btsnoop_hci.log --writes        # commands only

Getting a capture: enable Developer options on the phone, turn on "Bluetooth
HCI snoop log", force-close the Govee app so the next open makes a fresh
connection, drive the device, then take a bug report and look under
`FS/data/log/bt/`. Check the app shows the **Bluetooth** symbol and not Wi-Fi;
over Wi-Fi the phone's radio is not involved and the capture stays empty however
well the device responds.
"""

from __future__ import annotations

import argparse
import datetime
import struct
import sys

# btsnoop timestamps count microseconds from year zero.
EPOCH_OFFSET_US = 0x00DCDDB30F2F8000

H4_ACL = 0x02
H4_EVENT = 0x04
LE_META = 0x3E
LE_CONNECTION_COMPLETE = (0x01, 0x0A)  # plain and enhanced
L2CAP_CID_ATT = 0x0004

# Every Govee frame is exactly this long, checksum included.
FRAME_LEN = 20

ATT_OPCODES = {
    0x12: "write-req",
    0x52: "write-cmd",
    0x1B: "notify",
    0x1D: "indicate",
}


def checksum(data: bytes) -> int:
    """The XOR every Govee frame ends with."""
    result = 0
    for byte in data:
        result ^= byte
    return result


def looks_like_govee(frame: bytes) -> bool:
    """Whether these twenty bytes are a Govee frame.

    The checksum makes this all but impossible to fool, which is why encrypted
    traffic stands out at once: it is the right length and fails here.
    """
    return (
        len(frame) == FRAME_LEN
        and frame[0] in (0x33, 0xAA, 0xA1, 0xA3)
        and checksum(frame[:19]) == frame[19]
        and any(frame[1:19])
    )


def records(path: str):
    """Yield (timestamp, is_incoming, packet) for each btsnoop record."""
    with open(path, "rb") as handle:
        data = handle.read()
    if data[:8] != b"btsnoop\x00":
        raise SystemExit(f"{path} is not a btsnoop capture")

    offset = 16
    while offset + 24 <= len(data):
        _, included, flags, _, stamp = struct.unpack(">IIIIq", data[offset : offset + 24])
        offset += 24
        packet = data[offset : offset + included]
        offset += included
        when = datetime.datetime.fromtimestamp((stamp - EPOCH_OFFSET_US) / 1e6, datetime.UTC)
        yield when, bool(flags & 1), packet


def att_messages(path: str):
    """Yield (when, address, direction, att_opcode_name, payload).

    Fragments are joined; anything that is not ATT is dropped.
    """
    address_of_handle: dict[int, str] = {}
    partial: dict[tuple[int, str], list] = {}

    def finish(entry):
        when, buffer, direction, address = entry
        if len(buffer) < 8:
            return None
        if struct.unpack("<H", buffer[2:4])[0] != L2CAP_CID_ATT:
            return None
        att = buffer[4:]
        name = ATT_OPCODES.get(att[0]) if att else None
        if not name:
            return None
        # Opcode, then a two-byte attribute handle, then the value.
        return when, address, direction, name, bytes(att[3:])

    for when, incoming, packet in records(path):
        if not packet:
            continue

        if (
            packet[0] == H4_EVENT
            and len(packet) >= 15
            and packet[1] == LE_META
            and packet[3] in LE_CONNECTION_COMPLETE
            and packet[4] == 0  # status: success
        ):
            handle = struct.unpack("<H", packet[5:7])[0] & 0x0FFF
            address_of_handle[handle] = ":".join(f"{b:02X}" for b in reversed(packet[9:15]))
            continue

        if packet[0] != H4_ACL or len(packet) < 7:
            continue

        header = struct.unpack("<H", packet[1:3])[0]
        handle = header & 0x0FFF
        boundary = (header >> 12) & 0x3
        address = address_of_handle.get(handle, f"handle{handle}")
        key = (handle, "in" if incoming else "out")

        if boundary == 1:  # continuation of the previous message
            if key in partial:
                partial[key][1] += packet[5:]
        else:
            if key in partial:
                done = finish(partial.pop(key))
                if done:
                    yield done
            partial[key] = [when, bytearray(packet[5:]), key[1], address]

        if key in partial:
            entry = partial[key]
            declared = struct.unpack("<H", entry[1][0:2])[0] if len(entry[1]) >= 2 else 0
            if len(entry[1]) >= declared + 4:
                done = finish(partial.pop(key))
                if done:
                    yield done

    for entry in partial.values():
        done = finish(entry)
        if done:
            yield done


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("capture", help="btsnoop_hci.log from an Android bug report")
    parser.add_argument("--device", help="only this address, as AA:BB:CC:DD:EE:FF")
    parser.add_argument(
        "--connections",
        action="store_true",
        help="list which devices were talked to, and how much",
    )
    parser.add_argument("--writes", action="store_true", help="only what the phone sent")
    parser.add_argument(
        "--all",
        action="store_true",
        help="include payloads that are not Govee frames, such as encrypted ones",
    )
    args = parser.parse_args()

    if args.connections:
        seen: dict[str, list] = {}
        for when, address, _, _, payload in att_messages(args.capture):
            row = seen.setdefault(address, [when, when, 0, 0])
            row[1] = when
            row[2] += 1
            row[3] += 1 if looks_like_govee(payload) else 0
        if not seen:
            print("no ATT traffic in this capture")
            return
        print(f"{'address':20} {'from':8} {'to':8} {'messages':>8} {'Govee':>7}")
        for address, (first, last, total, govee) in sorted(seen.items(), key=lambda kv: kv[1][0]):
            note = "" if govee else "   <- nothing readable: encrypted?"
            print(f"{address:20} {first:%H:%M:%S} {last:%H:%M:%S} {total:8} {govee:7}{note}")
        return

    for when, address, direction, name, payload in att_messages(args.capture):
        if args.device and address.upper() != args.device.upper():
            continue
        if args.writes and direction != "out":
            continue
        govee = looks_like_govee(payload)
        if not govee and not args.all:
            continue
        mark = " " if govee else "?"
        print(
            f"{when:%Y-%m-%d %H:%M:%S} {address:20} {direction:3} {name:9}{mark}"
            f" {' '.join(f'{b:02X}' for b in payload)}"
        )


if __name__ == "__main__":
    try:
        main()
    except BrokenPipeError:
        sys.exit(0)
