# Roadmap

What is left, why it matters, and what it would cost. Ordered within each group by
value against effort rather than by when it came up.

Last reviewed **2026-08-27**, against the code at `2026.08.27-63f966a9`.

---

## 1. Honesty of reported state

The theme that produced the most user-visible damage this month, and the one with no
protocol research left in it. Both items below are about the add-on believing its own
optimism.

### A command is reported as successful the moment its bytes leave

Nothing on the Govee BLE wire acknowledges a write. A device receipts a frame it does
not understand — `33 05 00` — exactly as it receipts one it does. So "we sent it"
became "it happened", and for months an H613D showed the colour Home Assistant had
asked for while the strip stayed as it was.

The colour-dialect fix removed that particular cause, not the general one. Worse, the
optimistic value is now **persisted** across restarts, so a device that silently
ignores a command has its wrong state restored at every start.

What would fix it: treat a value as measured only when a read-back confirms it, and
keep the optimistic value clearly separate — publish it, but never persist it and
never let it outrank a measurement. Where a device cannot report an attribute at all,
the honest state is "unknown", not "what we asked for".

Not started. No protocol work needed.

### "No transport available" when the only transport failed

A Bluetooth-only device whose radio link drops produces:

```
no transport available; declined: BLE, LAN API, IoT API, Platform API
```

BLE did not decline, it *failed* — and because `BleTransport::fallback_on_error` is
true, the failure is converted into a fallback, after which nothing is left. The
message reads as "your device does not support this", which is the most misleading
thing it could say.

The router should distinguish "everyone declined" from "the transport that could have
served this one failed", and report the underlying error in the second case.

Small, self-contained, and would have saved an hour today.

---

## 2. Protocol, still unknown

### Per-segment colour temperature on a device with no Kelvin field

Two separate gaps that look alike:

- A **segmented** device takes `33 05 15 01 ff ff ff <kelvin>` per segment; solved.
- A **`0x02`-dialect** device has no Kelvin field anywhere and takes a rendered colour
  instead; solved for the whole device, untested per segment because no device here is
  both.

Nothing to do until a device that is both shows up.

### Scenes over Bluetooth

`SetSceneCode` in `ble.rs` implements the `0xa3` chunked encoding from published
research and has never been sent over a real radio link. `govee undoc pt-real` can try
it over the cloud first, costing no connection slot.

### Humidifiers over Bluetooth

The encoders exist and ship over AWS IoT; pointing them at the radio is mechanical.
Worth nothing to this installation — there is no humidifier here.

### The frames we still cannot read

`AA 11`, `AA 12`, `AA 23`, `AA 36`, `AA 41`, `AA 55`, `AA A9 02`, `AA BA`. Curiosity
only: everything the product needs is understood. `AA 06` and `AA 07` turned out to be
firmware version strings, which is the kind of thing that falls out of a capture for
free.

### `AA 0F` on a chained string

`AA 0F <n>` is believed to be the number of chained light strings, from a single
observation on a single device. The decisive test is to attach a second string to the
H7020 and see whether it becomes `AA 0F 02`. Until then the H7020's fifteen phantom
segments cannot be told from real ones.

---

## 3. Hardware and environment

### Two more ESP32 Bluetooth proxies

Ordered 2026-08-27, expected around 2026-08-29. The Keller H613D sits at −87 to
−99 dBm against a noise floor near −100, which produces 20-second connect timeouts that
look exactly like a software fault. Placing these should end that class of report.

Note the second, unrelated cause of the same symptom: while the Govee phone app holds
its one allowed BLE connection, the device stops advertising entirely and Home
Assistant reports `has not been seen by a connectable scanner`.

### Devices that encrypt their Bluetooth traffic

Two models here wrap every frame in AES-GCM after a session handshake. Both are
reachable through the cloud, so nothing is lost today; a Bluetooth-only one would be
unreachable. Govee's metadata flags them as `supportEnc` and the startup log reports
it. The scheme is documented in the homebridge-govee project and could be implemented
if a device ever needs it.

---

## 4. Housekeeping

- **Segment brightness has no optimistic value.** The colour a segment is commanded to show is
  now written into the device's picture straight away, so the picker no longer flickers. The
  brightness slider still can, and deliberately so: the per-segment brightness byte a device
  reports is on a scale nobody has identified, so a percentage from a command cannot honestly be
  written into the same field. Identifying that scale would close it.
- **Segment batching never fires.** `service::segments` has a 150 ms coalescing window
  that cannot coalesce, because `mqtt_light_segment_command` takes the per-device
  semaphore one line before reaching it. Each segment therefore costs its own message
  and its own read-back. Cheap on AWS IoT, expensive on the Platform API. The fix is a
  restructure — batch first, then take one permit for the flush — and the read-back
  scheduling hangs off the same `Coordinator`, so it has to move too.
- **The raised debug level.** `govee::service::ble_scheduler=debug` logs every
  notification. It has earned its keep repeatedly, most recently in finding the colour
  dialect, but it should come back out once nothing is being chased.
- **A stale add-on instance.** `b9845f46_govee2mqtt` at `2026.03.25-ab9deb66` is
  installed and stopped, left from an earlier repository, and contributes a second and
  permanently wrong update entity.
- **A brand icon for HACS.** Cannot be generated here; the validation workflow passes
  `ignore: brands` in the meantime.
- **Base image signature verification** during the add-on build. The bundled cosign is
  older than sigstore now requires and the monolithic builder action was removed, so
  restoring it means migrating to the composable builder actions.

---

## Not planned

- **Music modes, DIY scenes and saved snapshots as entities.** Removed on purpose;
  see the README. Nothing reports them back, so Home Assistant could offer them but
  never show which is active.
- **Govee app groups as devices.** Same reasoning. Grouping belongs to Home Assistant,
  where it can span more than one vendor.
- **Sensors via BLE advertisements.** Cheap and general — no connection, no slots — but
  no H5xxx sensor has ever appeared on this account.
- **Contributing back upstream.** Decided 2026-08-25. This fork has diverged too far,
  and rebases are no longer a goal.
