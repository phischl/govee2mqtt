# Govee LAN API Information

[Govee's LAN control API](https://app-h5.govee.com/user-manual/wlan-guide) is a
UDP based protocol with the following requirements:

* Govee2MQTT must be able to bind to UDP port `4002` on the machine where it runs
* Each Govee device must individually have had its LAN API access enabled
  in its settings in the Govee Home App
* UDP ports 4001 and 4003 must be reachable on each Govee device

## Device Discovery

Govee devices with LAN protocol enabled will listen for discovery packets
UDP port 4001.  They join the multicast group `239.255.255.250` so that
a client performing discovery, in theory, only needs to multicast to
that same group and limit the amount of network traffic expended on
discovery.

In practice, multicast-UDP is not well supported by various routers, especially
on WiFI enabled networks.

Govee2MQTT provides a couple of options that can help in situations where
multicast-UDP isn't working well in your environment, or where you have more
unusual network topology.

* You can specify a list of IP address to which discovery packets should
  be sent directly
* You can specify a number of variations on regular UDP broadcasts that
  might work better than multicast in some situations

For a device to be shown as usable via the LAN API in Govee2MQTT:

* UDP ports `4001` and `4003` must both be reachable from the Govee2MQTT instance
* The Govee device will respond to the source IP address of the packets sent
  from Govee2MQTT, but UDP port `4002` will be used instead of the originating
  port. Your network must allow this sort of "reply" to route back to Govee2MQTT.

See [LAN API Control Config](CONFIG.md#lan-api-control) for more details on how
to configure these options.

## Segment colour batches

H7020 colour-only segment batches can use the LAN API instead of Govee's
cloud. The existing 150 ms batch window groups colours into segment-mask
frames and sends them together in one UDP `ptReal` message. No new MQTT topic,
API key or Govee account login is needed for this delivery path; the bridge
must already have discovered the device and its segment entities.

The default segment transport order is LAN, AWS IoT, Bluetooth, then the
Platform API. `--transport-order` / `GOVEE_TRANSPORT_ORDER` applies its usual
priority prefix to the supported routes. Without LAN discovery, the next
eligible transport is tried.
The Platform route is skipped when its metadata cannot support every
requested segment colour or brightness change, before any HTTP writes.

LAN segment colour is enabled only for H7020. Testing on 2026-10-21 used
firmware **1.00.14**, a string with **15 physical bulbs**, and an ARM64 Linux
container. Initial raw-packet tests confirmed individual bulb addressing and
two different colours in one message. Testing through Home Assistant and the
patched bridge confirmed:

* A single addressed bulb changed colour.
* A full 15-colour palette was submitted in one LAN message containing 15
  frames; the colours were reflected in the Govee app.
* Main-light power and overall brightness controls continued to work.
* After restarting the bridge, LAN discovery and MQTT reconnected, and another
  15-colour palette was submitted in one LAN message.

These checks do not establish support for other models, a second chained
string, segment brightness, or atomic palette application.
Brightness-only and mixed colour/brightness batches keep their existing
AWS IoT, Bluetooth and Platform API routes; their default order is unchanged.
A transport override cannot enable LAN for these batches or other models.

A successful UDP send means the datagram was submitted, not that the device
acknowledged or applied it. The bridge retains its existing optimistic
commanded-colour updates. A local LAN send error is reported rather than
silently retried through the cloud.

Commands must arrive within the batching window to share a message. A card
or automation that waits seconds between segment commands will still
produce separate batches.

## Router / Network Setup tips

* Some routers have optimizations that prevent multicast-UDP from crossing from
  the WLAN to the LAN. Check your router's manual and configuration options.
  Don't confuse it with multicast-DNS. While that also uses UDP, it is a
  specialization and having that working doesn't imply that multicast-UDP in
  general will work.

* Consider enabling the `broadcast_all` option for the addon, which uses
  explicit UDP broadcasts to each network interface, rather than multicast.

* Assign a static IP to the device in your DHCP setup, then add that IP to the
  [scan list](CONFIG.md#lan-api-control) in the addon config, which will use
  unicast UDP packets to each device.  This is heavier on your network, but
  more compatible with certain VLAN setups.

* If you have an IOT VLAN or similar, ensure that your firewall is not blocking
  the ports mentioned above
