# What this fork changes

[wez/govee2mqtt](https://github.com/wez/govee2mqtt) reaches Govee devices over the LAN, Govee's
official Platform API, and their undocumented AWS IoT channel. This fork adds **Bluetooth** as a
fourth path and follows the consequences: devices that upstream cannot see at all, per-segment
control and state that Govee's own API does not offer, and polling that survives an internet
outage.

Eighty-eight commits since the fork point. Grouped by what they do rather than when they landed.

## Bluetooth

**Bluetooth-only devices work.** Upstream hides a device it cannot reach; this fork reaches it.
Control and state both travel over the radio, through ESPHome Bluetooth proxies.

That needs a companion piece, and the reason is worth knowing: an ESPHome proxy accepts exactly
**one** advertisement subscriber, so an add-on talking to the proxies directly would spend its
time stealing the subscription from Home Assistant. Instead the **Govee BLE Executor**
integration (in this repository, installed through HACS) performs the GATT work inside Home
Assistant, and the two halves exchange jobs over MQTT. All Govee protocol knowledge stays in the
add-on; the integration is a generic "connect, write these bytes, hand back the reply".

- Complete frame codecs for power, brightness, colour and colour temperature, each verified
  against traffic captured from real hardware.
- A scheduler with connection-slot awareness, per-device serialisation, command coalescing,
  backoff and a circuit breaker. Bluetooth punishes bursts, so this was designed in rather than
  added later.
- A light command's attributes travel in **one** radio session — "on, 60 %, warm white" is one
  connection, not three.
- Read-back happens after the caller is released, so verification no longer sits in the latency
  path.
- `ble_exclude`, `ble_max_concurrent`, `ble_address_map` and `no_ble` for control over all of it.

**Bluetooth is tried last by default.** The fork began with it as the preferred path; a day of
live use said otherwise, and the LAN and cloud paths are faster and better proven. Being last
costs a Bluetooth-only device nothing — every other transport declines it — and
`transport_order` reorders it for anyone who wants local control first.

## Segments

Govee's Platform API reports segment state as an empty string and charges one request per
segment. Both were fixed by reading what the devices already say.

- **Per-segment colour is readable**, from `aa a5` frames the devices emit and this fork now
  decodes. Over AWS IoT it arrives with the status; over Bluetooth it is a query we send, so it
  is live rather than tied to the poll interval.
- **The segment write command was reverse-engineered** —
  `33 05 15 01 <r> <g> <b> 00 00 00 00 00 <mask>`, a little-endian bitfield addressing at least
  thirty-two segments. Verified frame by frame against hardware.
- **Segment colour costs no Platform API quota.** A fifteen-segment scene cost fifteen cloud
  requests upstream, two after batching, and none now: the colours ride one AWS IoT message, or
  one Bluetooth session.
- **Segment colour actually reaches the device.** Until 2026.08.26-c0a15a90 it did not, on either
  of those two paths, and there was no fallback because a publish that succeeds locally looks
  like success. Over AWS IoT every frame went out with checksum `00` — the encoder finished an
  already-finished frame, XORing the payload with its own checksum — and devices discard those
  without a word. Over Bluetooth the frames were handed to the executor as raw bytes where the
  wire format wants base64. Both are fixed and verified against hardware; a Govee frame is never
  acknowledged on the wire, which is why neither showed up as an error.
- **Segment discovery no longer truncates.** It stopped at eighteen segments, and a device that
  reported exactly eighteen could not be told from one that had been cut off. A thirty-slot
  string would have lost twelve silently.
- **Segmented devices take colour over Bluetooth**, which they could not before — the whole-strip
  colour write does nothing on them.
- **Segments are discovered from the device**, not from Govee's metadata, which turns out to be
  unreliable in both directions: it claims fifteen segments for a two-spot lamp and omits twelve
  on another entirely. A device is mapped in one poll and the count is remembered across
  restarts.
- Switching a segment off works (upstream's brightness-zero approach would power the whole device
  up; setting black does not).

## Polling and the cloud

- **Per-transport poll intervals** (`poll_interval`, and `_lan` / `_iot` / `_platform` / `_ble`).
  A LAN query is a UDP packet, an AWS IoT request rides a connection we already hold, a Platform
  API call spends a daily quota, and a Bluetooth poll occupies a proxy slot. One number would have
  forced those against each other.
- **AWS IoT is chosen from Govee's metadata, not a model list.** Upstream gated it on a hardcoded
  list of SKUs that had drifted badly — on the author's account one of eleven models was in it,
  so ten spent Platform API quota while answering AWS IoT perfectly well. Measured effect: 240
  poll requests split 7 IoT / 233 Platform became 44 requests, all IoT and none Platform.
- **A command over AWS IoT is read back.** That path is fire and forget — the broker accepts the
  publish and never says whether the device acted — so a command that quietly did nothing used to
  leave Home Assistant showing a state the light never reached.
- **Polling falls back between sources** (`poll_order`, default `lan,iot,platform,ble`). When the
  internet goes, both cloud paths stop answering and Bluetooth keeps reporting whatever is in
  range of a proxy. The switchover watches the AWS IoT broker's connection rather than guessing:
  a publish to a broker that is gone succeeds locally and the reply simply never arrives.

## Robustness

- One unreadable frame in a status message no longer discards the whole message. A device sent
  something that was not valid base64 — random enough to fit the `supportEnc` flag Govee added the
  same week — and strictness at the wrong granularity turned one unknown into total data loss.
- `aa 05 15` no longer decodes as a colour. It fits the colour layout well enough to be mistaken
  for one, and over Bluetooth that would have written a segmented device's state as near-black.
- A speculative query may go unanswered without failing its session. Asking a device whether it
  has segments used to take a working Bluetooth-only light out of service on every poll.

## Packaging and CI

- Everything publishes under `ghcr.io/phischl/govee2mqtt`. `addon/Dockerfile` pulled the binary
  from upstream's image, so the fork's add-on would otherwise have shipped upstream's binary with
  none of this in it.
- Versions come from the tag, everywhere. Three places derived one from the checked-out commit
  instead, which on a release is the "Tag …" commit and not the code commit the tag is named
  after: the add-on was published under a version its own `config.yaml` did not ask for, and the
  binary announced a version nobody could look up. Releases are now verified against the
  registry rather than the workflow's exit code.
- A tagged add-on is reproducible. `addon/Dockerfile` copied the binary out of `:latest`, so an
  add-on contained whatever `main` had built most recently and rebuilding an old tag produced a
  different image. It is pinned to the tag's own image now.
- Trivy scans the working tree and all three published images. Its first run moved the runtime
  image from `distroless/cc-debian12` to `static-debian13` — every glibc CVE came from a library
  the static musl binary never calls — and the add-on base from Debian 12, end of life since
  July 2026, to 13.
- Every add-on option has a name and a description. The Bluetooth and transport options shipped
  as raw keys in the configuration UI for a while.

## Known limits

- An H7020 reports thirty segments for fifteen bulbs, because a second string can be chained to
  it and nothing distinguishes "absent" from "present" in the frames.
- Per-segment *brightness* can be read but not written; the command for it is not known.
- Colour temperature on a segmented device still goes to the cloud — there is no segment
  equivalent for it.
- Base image signatures are not verified during the add-on build. The bundled cosign is too old
  for what sigstore now requires, and bumping the builder is not possible — the monolithic
  action was removed. Migrating to the composable builder actions would restore it.
