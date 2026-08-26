# The Govee device protocol, as far as we have worked it out

Everything here was measured against real hardware on one account, or transcribed from
byte-level captures and then re-verified. Where something is a hypothesis rather than a
measurement, it says so.

Two channels carry the *same* bytes, which is the single most useful fact in this document:

- **Bluetooth LE**, written to a GATT characteristic.
- **Govee's undocumented AWS IoT MQTT API**, where the identical 20-byte frames travel
  base64-encoded inside a `ptReal` command.

So a frame worked out over one channel works over the other, and an unverified frame can be
tried over the cloud with no radio, no connection slot and no risk to the BLE scheduler.
`src/ble.rs` is the single implementation of the codec for both.

---

## 1. Bluetooth transport

```
Service       00010203-0405-0607-0809-0a0b0c0d1910
Read/Notify   00010203-0405-0607-0809-0a0b0c0d2b10
Write         00010203-0405-0607-0809-0a0b0c0d2b11
```

Write **without** response. Devices advertise as `Govee_<SKU>_<4 hex>`; older
reverse-engineering notes claim a prefix of `ihoment_`, which did not match any device here.

Max write-without-response payload at the default ATT MTU of 23 is 20 bytes — exactly one
frame, which is presumably not a coincidence.

## 2. Frame format

Always exactly **20 bytes**:

```
[0]      0x33 = write, 0xaa = query or notification
[1]      type
[2..19]  payload, zero-padded
[19]     XOR checksum over bytes 0..18
```

## 3. Whole-device frames

| Frame | Meaning |
|---|---|
| `33 01 <on>` | power |
| `33 04 <percent>` | brightness, `0x00`–`0x64` |
| `33 05 0d <r> <g> <b>` | colour |
| `33 05 0d ff ff ff <kelvin be16>` | colour temperature; `ff ff ff` marks CT mode |
| `aa 01 <on>` | power, query and notification |
| `aa 04 <percent>` | brightness, query and notification |
| `aa 05 01` / `aa 05 <mode> <r> <g> <b> <kelvin be16>` | colour query / notification |

Notes that cost time to learn:

- **Brightness is a plain byte, 0–100.** Both Python reference implementations format the
  percentage as a *decimal string* and parse it as hex, so 20 % becomes `0x13` = 19. Do not
  copy that.
- **`0x00` brightness switches the device off**, so clamp a zero to `0x01` unless switching off
  is what you meant.
- **Brightness does not power a light on.** The LAN and cloud APIs do that as a side effect;
  the frame does not. A light sent only a brightness frame stays dark and remembers how bright
  it would have been.
- **Colour temperature is big-endian Kelvin**, not mired: `0x1964` = 6500 K.
- **A white RGB frame and a CT frame share their first six bytes.** The Kelvin field is the
  only thing distinguishing them. In a notification a zero there is meaningful — it says the
  device is in RGB mode.
- The official app appends an RGB companion value to CT frames (6500 K → `ff f9 fb`). It is
  optional, and its values do not follow any colour-temperature approximation we could
  reproduce, so we omit it.

## 4. Segments

### The device reports its segment colours

```
AA A5 <page> then N × <brightness> <r> <g> <b>
```

`AA A5 <page>` is **both a query and the shape of the answer** — sending it returns that page,
confirmed against hardware two seconds before any status request went out.

**The number of groups per page varies by SKU.** H6072, H7020 and H60B2 carry three and leave
the last four payload bytes zero; an H6054 — two light bars of six — carries four and uses all
sixteen. A single frame cannot
tell you which: a three-group device pads with zeroes, and a four-group device whose last
segment is switched off — which is done by setting it to black — looks identical. Decide the
stride from a whole batch of pages, and keep the answer.

**An all-zero group is padding, not a black segment.** A segment that is off keeps its
brightness byte, so a group that is zero all through is past the end of the real list.

Brightness here is a **percentage**, independent of the master dimmer: a lamp at 60 % overall
reported 95 % per segment.

### Slot order

**Slot 0 is the segment nearest the power connector**, and the order runs away from it.
Confirmed on four devices against app screenshots. The Govee app draws the strip in whichever
direction suits the product photo, so it agrees with the frame order on some devices and
reverses it on others — the connector is the invariant.

### Writing a segment

```
33 05 15 01 <r> <g> <b> 00 00 00 00 00 <mask, little-endian>
```

- `mask` bit N addresses segment N. Segments not named keep what they had.
- Byte 3 is `0x01`, the same value the status frame `AA 05 15 01` carries.
- **The mask is at least four bytes wide**, starting at byte 12, with room for seven. Bits 0,
  3, 5, 15, 16 and 24 were each confirmed individually.
- **Bytes 7–11 must stay zero.** Setting byte 7 to `0x0a` turned segments 0, 1 and 2 black
  while the mask named only segment 0, so a value there changes how the rest of the frame is
  read.
- Several such frames can travel in one `ptReal` message, each with its own mask. That is how
  a multi-colour scene goes out in a single round trip.

One refuted hypothesis worth keeping, because it looked principled: `33 A5 <page> …`, mirroring
the read frame the way `33 01`/`AA 01` and `33 05 0d`/`AA 05 0d` do. The device ignored it
silently, both switched off and switched on.

**A note on a wrong turn, because the shape of it is instructive.** This frame was written up
here as "not universal" after an H6054 acknowledged it twice over `ptReal` and did nothing. That
was wrong twice over. The command works on that device — sent by hand with a valid checksum it
sets exactly the segments its mask names, single bit or all twelve. What did not work was our own
encoder, which finished an already-finished frame and so shipped every one of them with checksum
`00`. The device discarded them in silence.

Two things made this survivable for so long. A frame is never acknowledged on the wire, so
nothing distinguishes "rejected" from "applied" without reading the state back. And the
acknowledgement that *does* exist here — see the receipt below — was absent for the broken
frames, which is the signal that finally located the fault: our own hand-sent frames were
receipted, the add-on's were not.

The device is still the only one seen sending `AA A9`:

```
AA A9 00  06  01 10 03  01 10 03      six, then twice a three-byte group
AA A9 02  01 32                       0x32 = 50
```

`06` with two repeats fits "six per unit, two units" for a twelve-segment two-bar device, so
`A9 00` reads like a layout descriptor. `33 A9 02 01 <v>` is a **working write** — the device
echoes the new value back as `AA A9 02 01 <v>` — but it changes nothing in the `AA A5` pages, so
whatever it sets, it is not per-segment brightness.

### `AA 05 15` means "I have segments"

Present in the status of every segmented device on one account and in none of the others. It is
a better source than a per-SKU list, and a better source than Govee's own metadata, which omits
segments entirely for at least one segmented product.

Its payload byte is `00` on some devices and `01` on others, with no pattern we can see, so it
is carried and not interpreted.

Note the trap: `aa 05 15 01 …` fits the colour layout `aa 05 <mode> <r> <g> <b>` and will
happily decode as a near-black colour. A decoder has to refuse mode `0x15` explicitly.

### Segment counts are unreliable in both directions

Measured across one account, each against an app screenshot:

| SKU | Govee metadata claims | Device actually has |
|---|---|---|
| H7093 | 15 | **2** |
| H7020 | 30 | **15** |
| H6054 | *no segment capability at all* | **12** |
| H6072 | 8 | 8 |
| H60B2 | 3 | 3 |

The device's own frames over-report too, in a different way: a page can end in a filler group
that is not all-zero and so survives the padding rule, and there is no way to tell it from a
real segment by looking at it.

The practical consequence is that the question has two answers, because the errors point
opposite ways:

| | For a command mask | For creating entities |
|---|---|---|
| Take | the **larger** of reported and claimed | the **smaller** of the two |
| Because | mask bits past the end reach nothing; a count too small leaves segments untouched | an entity too many is a control that does nothing |

One case defeats both: the H7020, which a second string can be chained to, reports thirty slots
for fifteen bulbs whether or not anything is plugged in, and both sources agree on thirty. The
extra slots accept writes and hold the value while driving nothing. `AA 0F <n>` appears only on
that device and is the best candidate for the count of connected strings, on one observation of
one value — untested, because it needs a second string.

## 5. Scenes

`src/ble.rs` implements `SetSceneCode` as the `0xa3` chunked multi-frame encoding, from
published research rather than our own capture. **Untested over a real radio link.**

## 6. Frames we see and do not understand

Observed in status replies. All checksums verify.

| Frame | Seen on |
|---|---|
| `AA 11 00 1E 0F 0F …` | every device, segmented or not — so it is not a segment count |
| `AA 12 FF 64 00 00 80 <n> …` | several; last byte differs per device |
| `AA 23 FF <00 00 00 80> × 4` | several |
| `AA 41 <n>` | some segmented devices, values `01` and `02` |
| `AA A9 …` | one two-bar device only; see the segment-write section |
| `AA 33 11` | the same device |
| `AA 54` | the same device, as an unsolicited `ptReal` |
| `AA 0F <n>` | one chainable device only |
| `AA BA 01 00 64 64 64 …` | one device; three times 100 |

Also not worked out: music mode, DIY modes and keep-alive.

**Per-segment brightness is half solved**, and the halves are worth keeping apart.

*Reading it is done.* The `AA A5` pages carry it as a percentage, per segment, independent of the
master dimmer. Verified on three SKUs against screenshots of the Govee app taken at a known time:
`0x4B` where the app said 75 %, `0x1F` where it said 31 %. Nothing more is needed there.

*Writing it as a frame is not.* Three probes failed — brightness in byte 7, a `02` sub-command,
brightness placed after the mask — and each did nothing or something else. Two things narrow it
since:

- **The colour write leaves brightness alone.** `33 05 15` sets `<r> <g> <b>` and the segment
  keeps the brightness byte it had, so brightness is a separate command rather than a field of
  this one.
- **The app's brightness change is in the `05` family.** Driven over the cloud with the phone's
  Bluetooth off, setting one segment's brightness produced the receipt `33 05 00` — the same
  opcode as its colour change. So the command is a sibling of `33 05 15`, not something unrelated.

A candidate recorded here earlier is **refuted**: `AA A9 02 01 32` carries a value that happened to
equal the device's per-segment brightness, and `33 A9 02 01 <v>` turned out to be a working write —
the device echoes the new value straight back. But the `AA A5` pages do not move when it does, so
whatever that sets, it is not this.

Note that per-segment brightness is not *missing* from the product: Govee's Platform API sets it,
and that is where this fork sends it. What is missing is the frame, which is what a
Bluetooth-only device would need.

The published reverse-engineering material does not have it either. Two well-known script
collections between them use exactly five write opcodes — `33 01`, `33 04` and `33 05 0d`, all
against a single non-segmented downlight. Segments do not appear at all. So the remaining route
is a Bluetooth HCI log taken on a phone while the Govee app moves one segment's brightness
slider.

## 7. The same frames over AWS IoT

Govee's own app does not use the public Platform API. It uses an AWS IoT MQTT broker that
carries both commands and live status.

**Getting in:** log in with email and password to `app2.govee.com`, which returns an account id,
a token and the account's MQTT topic; exchange the token for `{endpoint, p12, p12_pass}`; unpack
the PKCS#12 into a key and certificate; connect to `{endpoint}:8883` over mTLS with the Amazon
root CA. The client id must be `AP/{account_id}/{random}`.

**Topics:** subscribe to the *account* topic for every device's status; publish to a device's
own `deviceSettings.topic` to command it. Subscribing to device topics as well makes the broker
drop the connection. A device with no `deviceSettings.topic` cannot be reached this way at all.

**Envelope:**

```jsonc
{"msg": {"cmd": "status",     "cmdVersion": 2, "transaction": "v_…000", "type": 0}}
{"msg": {"cmd": "turn",       "cmdVersion": 0, "data": {"val": 1},      "type": 1}}
{"msg": {"cmd": "brightness", "cmdVersion": 0, "data": {"val": 50},     "type": 1}}
{"msg": {"cmd": "ptReal",     "cmdVersion": 0,
         "data": {"command": ["<base64 frame>", …]}, "type": 1}}
```

Three traps:

- **`sku` and `device` can be at the top level or inside `state`.** Look in both.
- **Not every entry in `op.command` is a frame.** One device sent a 27-character string that is
  not valid base64 at all. A strict deserializer refuses the whole message over that one entry
  and loses an entire status report. Keep what you can read and log the rest.
- **The SKU is the codec key, and the light codecs are not registered under one** — they are
  generic. Decoding strictly by SKU makes ordinary colour frames look like unknown data, which
  is why this channel appeared far more opaque than it is.

**Status is pull, not push, for segments.** Whole-device attributes are volunteered; segment
pages arrive only inside the answer to a `status` request. A capture taken after a colour change
was byte-identical to the one before it.

**A write is acknowledged, and the acknowledgement names the opcode.** Measured 2026-08-26:
every `33 <opcode> …` frame sent to a device over `ptReal` is followed on the account topic by

```
cmd ptReal    33 <opcode> 00 00 … 00
```

The opcode survives; byte 2 and the whole payload are zeroed. Sending `33 05 0d …` produced
`33 05 00`, and `33 A9 02 01 0A` produced `33 A9 00`, so this is a per-write receipt and not a
notification about any particular subject.

It says **received**, not **applied** — both of those frames were acknowledged and only one of
them did anything. Its use is narrower but real: with a device the Govee app is driving, the
receipt reveals which opcode family the app is using, even though the payload is stripped.

*This paragraph replaces an earlier reading of the same frames as a "segment changed" marker.
Two observations coincided with two segment changes in the app and that looked like a signal; the
log then showed the same shape following writes of a completely different opcode, including ones
that had nothing to do with segments. A correlation with two data points was not a finding.*

**A capture over the cloud cannot recover the app's frames.** Worth knowing before anyone spends
an afternoon on it as we did: forcing the app onto Wi-Fi makes it controllable and observable in
its *effects*, and the receipt names its opcode, but the command itself goes to the device's own
topic — and subscribing to that makes the broker drop the connection. Only an on-device Bluetooth
HCI log will show the payload.

**There are no rate-limit headers**, on this channel or the public one. A live request to
`openapi.api.govee.com` returns only `date` and `content-type`, so quota use is not observable.

---

## Method notes

Four traps that each cost real time here, in the hope they cost someone else less.

- **When a measurement disagrees with an observation, re-run it before theorising.** A capture
  once appeared to be missing a colour the lamp was showing, which looked like evidence that the
  frames covered only part of the strip. It was two colours being confused in a spoken report.
  Re-run with values that cannot be mistaken for each other.
- **Stability across two reads is not freshness.** Two captures twenty minutes apart disagreed
  with a description of the device's state, identically both times, which looked like evidence
  that a byte meant something else. Both were simply older than the state being described.
  Compare against a screenshot taken at a known time.
- **Do not hand-assemble a mask.** One typed a byte too far left, set segments 8–15 on an
  eight-segment lamp, and silently did nothing while five segments stayed black.
- **A speculative question needs a protocol that allows silence.** Adding a segment probe to the
  Bluetooth poll turned a working device into a broken one: a device with no segments does not
  answer, an unanswered query failed the whole session, and the circuit breaker set the device
  aside every five minutes. A query that might not be answered has to be marked as such.
