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
- **A segmented device ignores `33 05 0d` in both its forms.** Measured on an H6054 on
  2026-08-27: sent RGB, and sent `33 05 0d ff ff ff 19 64` for 6500 K, both are receipted and
  neither changes anything — the segments keep their colours and no colour temperature appears in
  the status. Such a device takes `33 05 15` and nothing else in this family, which is why
  whole-device colour is sent to it as a mask over every segment. **There is no segment
  equivalent for colour temperature**, so a segmented device cannot be set to white over the
  radio at all; that still goes through the cloud.
- **Not every device reports its colour.** An H613D answers `aa 05 01` with
  `aa 05 0d 00 00 00 …` — the mode byte and nothing else — however it is actually lit. Power and
  brightness it reports honestly. Read literally that is "black", and a reader that believes it
  loses the colour it just set. Treat an `aa 05` carrying neither RGB nor Kelvin as carrying no
  information: a lit device is never black, and an unlit one says so through `aa 01`.

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

**And a group whose brightness is not a percentage is not a segment at all.** Some devices answer
*every* page they are asked for, filling the ones they do not have with `ff`. An H6116 with
fifteen segments answers five pages carrying brightnesses like `23` and `41`, and then:

```
AA A5 06  FF 00 00 00  FF 17 3B 80  FF 00 00 00  00 00 00 00
```

`0xff` is 255, and brightness is a percentage — `0x64` is the ceiling. That is the only thing
separating an invented group from a real one: it is not all-zero, so the padding rule cannot see
it, and its colour bytes look like a plausible colour. Believing it is expensive: discovery asks
one page past what it knows, so a device like this grows by three segments every poll until
something stops it.

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

### Writing a segment's brightness

```
33 05 15 02 <percent> <mask, little-endian from byte 5>
```

Note where the mask sits: **immediately behind the value**, not at byte 12 where the colour
frame keeps it. The sub-command is `02` against colour's `01`.

That difference is the whole story of why this took so long. An early probe used sub-command
`02` — correct — with the mask at the colour frame's position, found nothing, and the
sub-command was written off with it. Byte 12 is payload for this command.

Taken from a Bluetooth HCI capture of the Govee app, which is the only way to see what it
writes: over the cloud its commands go to the device's own topic and only a receipt is visible.
Two frames straight out of that log, then two of our own confirmed against hardware:

| Source | Frame | Effect |
|---|---|---|
| the app | `33 05 15 02 28 20` | segment 5 → 40 % |
| the app | `33 05 15 02 64 20` | segment 5 → 100 % |
| ours | `33 05 15 02 28 00 01` | H6054 segment 8 → 40 %, colour untouched |
| ours | `33 05 15 02 32 00 00 01` | H7020 segment 16 → 50 % |

So the mask reaches at least byte 7, twenty-four segments. Setting brightness leaves the
segment's colour alone, and setting colour leaves its brightness alone — they are independent
commands over the same segments, and both can travel in one `ptReal`.

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

**The Govee app does not derive the count from the pages either — it stops early.** Captured on an
H7020 that reports thirty slots: the app read `AA A5 01` through `AA A5 05` and then stopped. Five
pages of three is fifteen, which is what it displays and what the hardware has. It knew before it
started reading.

What it asked first is the interesting part. Only on this device, and only right before the pages,
it sent `AA 0F` and got `01` back. That is the frame long suspected of counting connected strings,
and one string of fifteen bulbs gives exactly the fifteen the app then read. The reading is
consistent but not proven: the app also knows the SKU from the account, so "one string × fifteen
per string, fifteen from a product table" fits the same evidence. Either way the *per-string
length* is knowledge the device does not appear to volunteer, which means a per-SKU rule is
unavoidable for this family — the device can only be asked how many strings, not how long they are.

Confirmed at the same time: the app addresses this device as **fifteen** segments, not thirty.
Setting "the last two bulbs" produced mask `0x6000` — bits 13 and 14, not 28 and 29.

### `AA 0F <n>` is the number of chained strings — settled 2026-08-26

Some products take a second light string plugged into the first. Such a device exposes slots for
the maximum it could ever drive — an H7020 reports thirty over `AA A5`, ten pages of three — while
only the connected ones exist in hardware. Govee's own app shows the real number, and this is how
it knows.

**The app does not count the pages. It asks, then stops early.** Captured over Bluetooth: it sent
`AA 0F`, got `01` back, and then read `AA A5 01` through `AA A5 05` and no further. Five pages of
three is fifteen, which is exactly one string.

**`AA 0F` is writable, and that proves it without buying a second string.** Sending `33 0F 02` —
"two strings" — was receipted and the device then reported `AA A9`… no: `AA 0F 02`. Nothing else in
its status changed, the ten `AA A5` pages included. The app, reopened, drew **thirty** bulbs in six
rows of five where it had drawn fifteen in three. Restored with `33 0F 01`.

So:

```
33 0F <strings>      set how many strings are chained
AA 0F <strings>      read it back
```

and the app's segment count is `strings × 15`.

**The per-string length is not on the wire.** Nothing the device answers says "fifteen"; the app
knows that from the product. So a reader of these frames can learn how many strings are attached
but not how long one is, and a per-SKU constant is unavoidable for this family. That is a real
limit, not a gap in the capture — we watched the whole handshake.

Worth noting what this also settles: the phantom slots are not a reporting bug. The device
genuinely addresses thirty segments, accepts writes to all of them, and holds the values; fifteen
of them simply have no bulbs on the end.

One case defeats both counts: the H7020 reports thirty
slots for fifteen bulbs whether or not anything is plugged in, and both sources agree on thirty. The
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
| `AA 36 <n> <n> <n>` | power per lamp — see below |
| `AA A9 …` | one two-bar device only; see the segment-write section |
| `AA 54` | as an unsolicited `ptReal` |
| `AA 0F <n>` | one chainable device only |
| `AA BA 01 00 64 64 64 …` | one device; three times 100 |

Five frames that used to be on that list are now accounted for, all from watching the Govee
app's own connection handshake over Bluetooth:

| Query | Answer |
|---|---|
| `AA 06` | `"1.09.13"` — plain ASCII |
| `AA 07 03` | `"3.02.01"` |
| `AA 20` | `"1.03.00"` |
| `AA 21` | `"1.00.29"` |
| `AA 33` | `AA 33 11` |
| `AA A3` | all zeros — the scene family, idle |

So four are version strings, and the app keeps the connection alive by **repeating one query every
two seconds** for its whole life. Which query differs by device: `AA 33` on an H6054, `AA 01` on an
H7020. So it is the repetition that matters, not a particular keep-alive frame — which is what
§4.4 of the working notes was looking for and would not have found. `AA 14` answers with six bytes that look like an address. The app also writes
`33 09 10 24 25 03 01 02 00 1A 08 EA 07 …` right after connecting, where `EA 07` is `0x07EA` =
2026 little-endian: a clock synchronisation.

**`AA 36` is power per lamp.** Measured on an H60B2 on 2026-08-27: switched on it reports
`AA 36 01 01 01`, switched off `AA 36 00 00 00`. That device is three separate lamp heads in one
housing, and Govee's own metadata exposes three `lightN` toggles for it, so the three bytes are
the three lamps. An H7093 reports three as well where only two spots are attached, so the field
may be fixed-width rather than sized to the hardware — not settled.

This matters for **colour temperature on a segmented device**, which is still unsolved. Watching
the Govee app on four products: an H60B2 sets it per lamp, an H6054 sets it per *bar* even when
one segment is selected, and an H6072 and H7020 only for the whole device. So the granularity is
the lamp with its own white LEDs, not the segment — RGBIC segments within one strip share the
white channel or have none. Two of those devices describe their units on the wire (`AA A9 00` on
the H6054, `AA 36` here) and the others do not, so a per-model rule may still be unavoidable.

None of that is actionable until the write frame is known, and the way to find it is the way
per-segment brightness was found: a Bluetooth HCI capture of the app moving the slider.

**The app queries far more than it displays**, and its opening burst is the cheapest catalogue of
valid queries there is: `AA 06`, `AA 07`, `AA 21`, `AA 20`, `AA 14`, `AA 23`, `AA 11`, `AA 12`,
`AA 04`, `AA 01`, `AA 33`, `AA 05 01`, `AA A5 01..03`, `AA A9 00`, `AA A9 02`. Everything in it is
a question the device answers.

Also not worked out: music mode, DIY modes and keep-alive.

**Per-segment brightness is solved** — see the segment write section above for the command. What
follows is how it was found, kept because the wrong turns are the useful part.

*Reading it is done.* The `AA A5` pages carry it as a percentage, per segment, independent of the
master dimmer. Verified on three SKUs against screenshots of the Govee app taken at a known time:
`0x4B` where the app said 75 %, `0x1F` where it said 31 %. Nothing more is needed there.

*Writing it as a frame took a Bluetooth capture.* Three probes had failed — brightness in byte 7,
a `02` sub-command, brightness placed after the mask — and the second of those was the painful
one: the sub-command was right and only the mask position was wrong, so a correct guess was
discarded as a dead end.

Two observations narrowed it before the capture, and both held up. The colour write leaves the
brightness byte alone, so brightness had to be a separate command rather than a field. And with
the phone's Bluetooth switched off, the app's brightness change produced the receipt `33 05 00` —
the same opcode as its colour change — so the command had to be a sibling of `33 05 15`.

One candidate recorded here earlier is **refuted**: `AA A9 02 01 32` carries a value that happened
to equal the device's per-segment brightness, and `33 A9 02 01 <v>` turned out to be a working
write — the device echoes the new value straight back. But the `AA A5` pages do not move when it
does, so whatever that sets, it is not this.

The published reverse-engineering material was no help and is worth ruling out in writing. Two
well-known script collections between them use exactly five write opcodes — `33 01`, `33 04` and
`33 05 0d` — all against a single non-segmented downlight. Segments do not appear at all. The HCI
log was the only route, and it took four minutes once someone thought of it.

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
