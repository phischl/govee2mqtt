# Configuration Options

## Govee Credentials

While `govee2mqtt` can run without any govee credentials, it can only discover
and control the devices for which you have already enabled LAN control.

It is recommended that you configure at least your Govee username and password
prior to your first run, as that is the only way for `govee2mqtt` to determine
room names to pre-assign your lights into the appropriate Home Assistant areas.

For scene control, for devices that don't support the LAN API, a Govee API Key
is required.  If you don't already have one, [you can find instructions on
obtaining one
here](https://developer.govee.com/reference/apply-you-govee-api-key).

|CLI|ENV|AddOn|Purpose|
|---|---|-----|-------|
|`--govee-email`|`GOVEE_EMAIL`|`govee_email`|The email address you registered with your govee account|
|`--govee-password`|`GOVEE_PASSWORD`|`govee_password`|The password you registered for your govee account|
|`--api-key`|`GOVEE_API_KEY`|`govee_api_key`|The API key you requested from Govee support|

*Concerned about sharing your credentials? See [Privacy](PRIVACY.md) for
information about how data is used and retained by `govee2mqtt`*

## LAN API Control

A number of Govee's devices support a local control protocol that doesn't require
your primary internet connection to be online.  This offers the lowest latency
for control and is the preferred way for `govee2mqtt` to interact with your
devices.

The [Govee LAN API is described in more detail
here](https://app-h5.govee.com/user-manual/wlan-guide), including a list of
supported devices.

*Note that you must use the Govee Home app to enable the LAN API for each
individual device before it will be possible for `govee2mqtt` to control
it via the LAN API.*

In theory the LAN API is zero-configuration and auto-discovery, but this
relies on your network supporting multicast-UDP, which is challenging
on some networks, especially across wifi access points and routers.

|CLI|ENV|AddOn|Purpose|
|---|---|-----|-------|
|`--no-multicast`|`GOVEE_LAN_NO_MULTICAST=true`|`no_multicast`|Do not multicast discovery packets to the Govee multicast group `239.255.255.250`. It is not recommended to use this option.|
|`--broadcast-all`|`GOVEE_LAN_BROADCAST_ALL=true`|`broadcast_all`|Enumerate all non-loopback network interfaces and send discovery packets to the broadcast address of each one, individually. This may be a good option if multicast-UDP doesn't work well on your network|
|`--global-broadcast`|`GOVEE_LAN_BROADCAST_GLOBAL=true`|`global_broadcast`|Send discovery packets to the global broadcast address `255.255.255.255`. This may be a possible solution if multicast-UDP doesn't work well on your network.|
|`--scan`|`GOVEE_LAN_SCAN=10.0.0.1,10.0.0.2`|`scan`|Specify a list of addresses that should be scanned by sending them discovery packets. Each element in the list can be an individual IP address (eg: the address of a specific device: be sure to assign it a static IP in your DHCP or other network setup!) or a network broadcast address like `10.0.0.255` for networks that are reachable but not directly plumbed on the machine where `govee2mqtt` is running.|

[Read more about LAN API Requirements here](LAN.md)

## Bluetooth Configuration

Bluetooth control requires the companion **Govee BLE Executor** Home Assistant
integration, which carries out the Bluetooth work using Home Assistant's own
adapters and ESPHome proxies. See [docs/BLUETOOTH.md](BLUETOOTH.md) for why the
add-on cannot reach the proxies itself.

It is tried **last** by default, after the LAN and cloud paths: those are faster
and the LAN is the only transport that reads back what it wrote. That costs
nothing for a Bluetooth-only light, which no other transport will touch, and
`transport_order` promotes it for anyone who prefers local control.

|CLI|Environment|Add-on Option|Purpose|
|---|-----------|-------------|-------|
|`--ble-topic-prefix`|`GOVEE_BLE_TOPIC_PREFIX=gv2mqtt/ble`|`ble_topic_prefix`|MQTT topic prefix shared with the Govee BLE Executor integration. Must match the value configured there. Defaults to `gv2mqtt/ble`.|
|`--no-ble`|`GOVEE_BLE_DISABLE=true`|`no_ble`|Disable the Bluetooth transport entirely, even when the executor is online.|
|`--ble-max-concurrent`|`GOVEE_BLE_MAX_CONCURRENT=3`|`ble_max_concurrent`|How many Bluetooth sessions may run at once, across all devices. Defaults to 1, which is the safest setting and enough for controlling one light at a time. Raise it if scenes that touch several lights feel slow; each concurrent session holds one of a proxy's connection slots, so keep it below the number of proxies you have. The companion integration has a matching setting that must be raised too.|
|`--ble-address-map`|`GOVEE_BLE_ADDRESS_MAP=15:25:...:A4=60:74:F4:2B:2E:A4`|`ble_address_map`|Correct the Bluetooth address for specific devices, as a comma separated list of `device-id=AA:BB:CC:DD:EE:FF` pairs. Only needed when Govee's metadata reports an address the device does not answer on — one H601B reported an address one higher than the one derived from its device id, which advertised strongly but refused every connection.|
|`--ble-exclude`|`GOVEE_BLE_EXCLUDE=H601B,Desk Lamp`|`ble_exclude`|Keep individual devices off Bluetooth while leaving it enabled for everything else. Comma separated; each entry matches a device id, SKU or name, so `H601B` excludes a whole model and `15:25:60:74:F4:2B:2E:A4` a single light. Excluded devices fall back to LAN or the cloud exactly as they did before.|
|`--transport-order`|`GOVEE_TRANSPORT_ORDER=lan,ble,iot,platform`|`transport_order`|Override which transports are preferred, as a comma separated list of `ble`, `nightlight`, `lan`, `iot`, `platform`. Acts as a priority prefix: the transports named here are tried first, followed by whatever else the operation allows. It never enables a transport an operation does not support. The default puts Bluetooth **last**, so devices that can be reached another way are; name `ble` earlier to prefer local Bluetooth control over the cloud.|

Bluetooth is used only when the executor reports itself online and the device's
Bluetooth address is known. Addresses come from your Govee account metadata, so
no manual configuration is needed. If Bluetooth fails repeatedly for a device,
it is set aside for five minutes and the usual LAN or cloud path is used instead.

Bluetooth-only lights, which previous versions hid because there was no way to
reach them, now appear in Home Assistant as long as their address is known.
Their state is read back after each command and refreshed periodically on the
usual poll interval.

## Polling

Nothing here changes how quickly your own commands take effect — that is
immediate. This is about noticing changes made *elsewhere*: someone using the
Govee app, a physical remote, or a wall switch.

Each transport polls on its own schedule, because they do not cost the same
thing. A LAN query is a UDP packet on your own network. An AWS IoT request rides
a connection that is already open. A Platform API call spends part of a daily
quota. A Bluetooth poll occupies a proxy connection slot for a second or two.
A single number would force those against each other.

|CLI|Environment|Add-on Option|Purpose|
|---|-----------|-------------|-------|
|`--poll-interval`|`GOVEE_POLL_INTERVAL=900`|`poll_interval`|Seconds a device's state may be stale before it is polled again. The default for the AWS IoT, Platform API and Bluetooth paths. Defaults to 900.|
|`--poll-interval-lan`|`GOVEE_POLL_INTERVAL_LAN=30`|`poll_interval_lan`|Seconds between LAN status queries. Defaults to 30 — every pass of the poll loop.|
|`--poll-interval-iot`|`GOVEE_POLL_INTERVAL_IOT=900`|`poll_interval_iot`|Seconds between AWS IoT status requests. Defaults to `--poll-interval`.|
|`--poll-interval-platform`|`GOVEE_POLL_INTERVAL_PLATFORM=1800`|`poll_interval_platform`|Seconds between Platform API polls. Defaults to `--poll-interval`.|
|`--poll-interval-ble`|`GOVEE_POLL_INTERVAL_BLE=900`|`poll_interval_ble`|Seconds between Bluetooth polls of Bluetooth-only devices. Defaults to `--poll-interval`.|
|`--poll-after-control`|`GOVEE_POLL_AFTER_CONTROL=5`|`poll_after_control`|Seconds to wait after a command before reading the device back. Defaults to 5.|
|`--poll-order`|`GOVEE_POLL_ORDER=ble,lan,iot,platform`|`poll_order`|Where a device's state is read from, and in what order, as a comma separated list of `lan`, `iot`, `platform` and `ble`. A priority prefix: it promotes what you name and never removes the rest. Defaults to `lan,iot,platform,ble`.|

The poll loop runs at the shortest configured interval, bounded to between 5 and
30 seconds, so a short interval is honoured rather than silently rounded up to a
fixed tick.

### Which one to change

**Segment colours feel stale.** Lower `poll_interval_iot`. Per-segment state
arrives only over the AWS IoT channel — the Platform API reports it as empty —
and only in reply to a poll, so segment entities are exactly as current as that
interval.

**You are worried about Govee's request quota.** Raise
`poll_interval_platform`. Every device without an AWS IoT path costs one request
per interval, so thirty devices at the default spend roughly 2,900 requests a
day, plus one enumeration every ten minutes.

Govee returns **no rate-limit headers** on the Platform API, so neither this
add-on nor you can see how much of the quota is left, and there is no backoff to
fall back on. If you are near the limit, raising the interval is the only lever.

**Your proxies are busy.** Raise `poll_interval_ble`. Only Bluetooth-only
devices are polled this way; anything with a LAN or cloud presence is left to
those, because its Bluetooth writes are already verified inside the session that
issued them.

**A light briefly shows its old state after you change it.** Raise
`poll_after_control`. Every command is followed by a read-back, because neither
cloud path confirms that the device acted: the Platform API's status is not
guaranteed to be coherent with a command issued a moment earlier, and an AWS IoT
command is never acknowledged at all — the broker accepts the publish and says
nothing more. The read-back uses the same channel the device is reachable on, so
for an IoT device it costs no Platform API quota. Bluetooth is exempt: a session
already reads back the attributes it changed, and LAN is exempt because it
verifies its own writes.

**Your internet goes down.** Nothing to configure — this is what the default order is for.
AWS IoT stops answering, the Platform API stops answering, and Bluetooth carries on for every
device within reach of a proxy. The add-on notices the AWS IoT connection dropping rather than
guessing: a publish to a broker that is gone succeeds locally and the reply simply never arrives,
so the connection state is what decides, not whether a client was configured.

Two honest caveats. Bluetooth only reaches what is near a proxy, so garden and outbuilding devices
will go stale until the connection returns. And on the first poll round after an outage every
unreachable device costs a failed radio session before its circuit breaker sets it aside for five
minutes.

**You would rather not depend on Govee's cloud at all.** Put `ble` first in `poll_order`. Expect
it to be slower — a radio poll takes a second or two and holds one of a proxy's three connection
slots, where an AWS IoT poll costs a single MQTT message and returns the whole device at once.

**A device flickers about a minute after Home Assistant starts talking to it.**
That is [known Govee firmware behaviour](https://github.com/wez/govee2mqtt/issues/250)
and the reason LAN polling is regular rather than opportunistic. Changing
`poll_interval_lan` moves the rhythm; it will not remove it.

Note that the diagnostic "Status" sensor calls a device missing once its state
is older than the **longest** configured interval plus thirty seconds, so
raising an interval will not make healthy devices report themselves as gone.

## Everything else

|CLI|ENV|AddOn|Purpose|
|---|---|-----|-------|
|`--temperature-scale`|`GOVEE_TEMPERATURE_SCALE=C`|`temperature_scale`|`C` for Celsius or `F` for Fahrenheit, for the temperature values reported to Home Assistant. Defaults to `C`.|
|n/a|`RUST_LOG=govee=info`|`debug_level`|How much the add-on logs, as a Rust log filter. `govee=debug` is the usual first step; `govee=trace` logs sensitive values including MQTT topics and tokens, so redact before sharing. Individual modules can be raised on their own, which is usually what you want: `govee=info,govee::service::ble_scheduler=debug` logs every Bluetooth frame and nothing else.|

## MQTT Configuration

In order to make your devices appear in Home Assistant, you will need to have configured Home Assistant with an MQTT broker.

  * [follow these steps](https://www.home-assistant.io/integrations/mqtt/#configuration)

You will also need to configure `govee2mqtt` to use the same broker:

|CLI|ENV|AddOn|Purpose|
|---|---|-----|-------|
|`--mqtt-host`|`GOVEE_MQTT_HOST`|`mqtt_host`|The host name or IP address of your mqtt broker. This should be the same broker that you have configured in Home Assistant.|
|`--mqtt-port`|`GOVEE_MQTT_PORT`|`mqtt_port`|The port number of the mqtt broker. The default is `1883`|
|`--mqtt-username`|`GOVEE_MQTT_USER`|`mqtt_username`|If your broker requires authentication, the username to use|
|`--mqtt-password`|`GOVEE_MQTT_PASSWORD`|`mqtt_password`|If your broker requires authentication, the password to use|

