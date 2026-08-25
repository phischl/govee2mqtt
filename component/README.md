# Govee BLE Executor

A Home Assistant integration that executes Bluetooth LE operations on behalf of
the [govee2mqtt](https://github.com/phischl/govee2mqtt) add-on.

## Why this exists

The add-on runs in a container with no Bluetooth stack, so it cannot reach BLE
devices directly. Talking to the ESP32 proxies itself is not an option either:
an ESPHome Bluetooth proxy accepts exactly **one** advertisement subscriber, and
a new subscription silently replaces the previous one. Home Assistant and the
add-on would spend their time stealing the slot from each other.

This integration sidesteps that by doing the Bluetooth work inside Home
Assistant, where the proxies are already connected, and exchanging jobs with the
add-on over MQTT.

It knows nothing about Govee. All protocol knowledge and all scheduling live in
the add-on; this side connects, writes bytes, and reports what came back.

## Throttling

Home Assistant has no BLE queue of its own. `establish_connection` does not wait
for a free connection slot — it fails and retries with backoff — so N parallel
jobs really do produce N parallel connects, and habluetooth's path scorer
penalises every connect already in flight on a proxy.

The executor therefore runs a small fixed worker pool. **One worker by default**,
which means one connect in flight at a time. Raise it only if commands feel
sluggish and your proxies have slots to spare; an ESP32 proxy typically has three.

Connections are kept open for a configurable idle timeout (30 s by default) so
that a burst of commands reuses one connection instead of reconnecting per frame.

## Configuration

| Setting | Default | Meaning |
|---|---|---|
| MQTT topic prefix | `gv2mqtt/ble` | Must match the add-on |
| Concurrent connections | `1` | Ceiling on connects in flight |
| Idle timeout | `30 s` | How long a connection is held after the last command |

## Wire protocol

Three topics under the configured prefix.

### `<prefix>/req` — add-on to executor

A request describes a whole session, not a single write, so that a burst of
commands costs one connection.

```json
{
  "id": "01JABCDEF",
  "address": "60:74:F4:2B:2E:A5",
  "priority": "user",
  "keep_open_ms": 30000,
  "deadline_ms": 20000,
  "ops": [
    {"write": {"char": "00010203-0405-0607-0809-0a0b0c0d2b11",
               "data": "MwEBAAAAAAAAAAAAAAAAAAAAADM=", "response": false}},
    {"delay_ms": 200},
    {"query": {"write_char": "00010203-0405-0607-0809-0a0b0c0d2b11",
               "notify_char": "00010203-0405-0607-0809-0a0b0c0d2b10",
               "data": "qgEAAAAAAAAAAAAAAAAAAAAAAKs=",
               "timeout_ms": 5000}}
  ]
}
```

`priority` is `user` or `poll`; `user` jobs are dequeued first. `deadline_ms`
covers queue time: a job that waited longer than its deadline is answered with a
`timeout` error rather than executed, since the add-on has already given up on it.

`keep_open_ms` overrides the configured idle timeout for this device; `0` means
use the configured value.

### `<prefix>/res` — executor to add-on

```json
{"id": "01JABCDEF", "ok": true, "duration_ms": 812,
 "results": [{"kind": "write"}, {"kind": "delay"},
             {"kind": "notify", "data": "qgEBAAAAAAAAAAAAAAAAAAAAAKo="}]}
```

```json
{"id": "01JABCDEF", "ok": false, "duration_ms": 4021,
 "error": {"kind": "out_of_slots", "retry_after_ms": 4000,
           "message": "No backend with an available connection slot ..."}}
```

Error kinds, and what the scheduler should do about them:

| Kind | Meaning |
|---|---|
| `bad_request` | The job was malformed. A bug; do not retry. |
| `not_found` | No connectable scanner has seen this device recently. |
| `out_of_slots` | Every reachable proxy is at capacity. Retry after `retry_after_ms`. |
| `connect_failed` | The connection attempt failed for another reason. |
| `gatt_error` | Connected, but the device refused an operation. |
| `timeout` | The job outlived its deadline, or a query got no notification. |
| `internal` | Unexpected failure in the executor. |

### `<prefix>/status` — executor to add-on, retained

```json
{"online": true, "max_concurrent": 1, "idle_timeout_s": 30.0, "queue_depth": 0,
 "proxies": [{"source": "AA:BB:CC:DD:EE:FF", "slots": 3, "free": 2,
              "allocated": ["60:74:F4:2B:2E:A5"]}]}
```

Scanners that have not yet reported their capacity are omitted rather than
reported as having zero slots — `slots == 0` means "unknown", not "exhausted",
and reporting it would make the add-on throttle for no reason.

## Installation

Via HACS as a custom repository, or by copying
`custom_components/govee_ble_executor/` into your Home Assistant `config`
directory. Requires the MQTT integration and at least one connectable Bluetooth
adapter or proxy.
