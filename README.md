# Govee to MQTT bridge for Home Assistant

[![Container Build](https://github.com/phischl/govee2mqtt/actions/workflows/build.yml/badge.svg?branch=main)](https://github.com/phischl/govee2mqtt/actions/workflows/build.yml)
[![Security](https://github.com/phischl/govee2mqtt/actions/workflows/security.yml/badge.svg?branch=main)](https://github.com/phischl/govee2mqtt/actions/workflows/security.yml)
[![Home Assistant Integration](https://github.com/phischl/govee2mqtt/actions/workflows/component.yml/badge.svg?branch=main)](https://github.com/phischl/govee2mqtt/actions/workflows/component.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE.md)

This repo provides a `govee` executable whose primary purpose is to act
as a bridge between [Govee](https://govee.com) devices and Home Assistant,
via the [Home Assistant MQTT Integration](https://www.home-assistant.io/integrations/mqtt/).

> **This is a fork of [wez/govee2mqtt](https://github.com/wez/govee2mqtt)** that adds
> Bluetooth as a transport for lights, driven through Home Assistant's own adapters and
> ESPHome Bluetooth proxies. It publishes its own images under
> `ghcr.io/phischl/govee2mqtt`. See [docs/BLUETOOTH.md](docs/BLUETOOTH.md) for how it
> works and [docs/CONFIG.md](docs/CONFIG.md#bluetooth-configuration) for the settings.
>
> Bluetooth is tried **last** by default, after the LAN and cloud paths, which are faster
> and better proven. That costs nothing for a Bluetooth-only light, which has no other
> path and is served by Bluetooth regardless — and `transport_order` reorders it for
> anyone who wants Bluetooth first.
>
> **Bluetooth control needs a second piece:** the *Govee BLE Executor* integration from
> this same repository, installed through HACS. It is not optional, and without it the
> Bluetooth transport simply declines every command. The add-on cannot reach the proxies
> itself — an ESPHome proxy accepts exactly one advertisement subscriber, so it would be
> competing with Home Assistant for it.

## Installation

**Add-on** — add `https://github.com/phischl/govee2mqtt` as a repository under
Settings → Add-ons → Add-on Store → ⋮ → Repositories, then install *Govee to MQTT Bridge*.

**Bluetooth integration** — add the same URL to HACS as a custom repository of type
*Integration*, install *Govee BLE Executor*, and add it from Settings → Devices & Services.
Required for Bluetooth; the add-on alone cannot use it. It is an integration rather than a
second add-on, so it installs through HACS and not through the Add-on Store.

## Features

* Bluetooth control of lights through your existing ESPHome Bluetooth proxies, with
  command coalescing and connection-slot aware scheduling. Bluetooth-only lights, which
  upstream hides because it cannot reach them, appear in Home Assistant.

* Robust LAN-first design. Not all of Govee's devices support LAN control,
  but for those that do, you'll have the lowest latency and ability to
  control them even when your primary internet connection is offline.
* Support for per-device modes and scenes.
* Support for the undocumented AWS IoT interface to your devices, provides
  low latency status updates.
* Support for the new [Platform
  API](https://developer.govee.com/reference/get-you-devices) in case the AWS
  IoT or LAN control is unavailable.

|Feature|Requires|Notes|
|-------|--------|-------------|
|DIY Scenes|API Key|Find in the list of Effects for the light in Home Assistant|
|Music Modes|API Key|Find in the list of Effects for the light in Home Assistant|
|Tap-to-Run / One Click Scene|IoT|Find in the overall list of Scenes in Home Assistant, as well as under the `Govee to MQTT` device|
|Live Device Status Updates|LAN and/or IoT|Devices typically report most changes within a couple of seconds.|
|Segment Color|API Key|Find the `Segment 00X` light entities associated with your main light device in Home Assistant. Setting a colour needs the API Key; *reading back* which segment is which colour needs `IoT`, since the Platform API reports segment state as empty|

* `API Key` means that you have [applied for a key from Govee](https://developer.govee.com/reference/apply-you-govee-api-key)
  and have configured it for use in govee2mqtt
* `IoT` means that you have configured your Govee account email and password for
  use in govee2mqtt, which will then attempt to use the
  *undocumented and likely unsupported* AWS MQTT-based IoT service
* `LAN` means that you have enabled the [Govee LAN API](https://app-h5.govee.com/user-manual/wlan-guide)
  on supported devices and that the LAN API protocol is functional on your network

## Usage

* [Installing the HASS Add-On](docs/ADDON.md) - for HAOS and Supervised HASS users
* [Running it in Docker](docs/DOCKER.md)
* [Configuration](docs/CONFIG.md)

## Have a question?

* [Is my device supported?](docs/SKUS.md)
* [Check out the FAQ](docs/FAQ.md)

## Want to say thanks?

Almost everything here was written by Wez Furlong; this fork adds the Bluetooth
transport on top of it. So the first thanks belong upstream:

* [Sponsor Wez on Github](https://github.com/sponsors/wez)
* [Sponsor Wez on Patreon](https://patreon.com/WezFurlong)
* [Sponsor Wez on Ko-Fi](https://ko-fi.com/wezfurlong)
* [Sponsor Wez via liberapay](https://liberapay.com/wez)

If the Bluetooth support in particular is what you came for, you can buy me a coffee:

[![ko-fi](https://ko-fi.com/img/githubbutton_sm.svg)](https://ko-fi.com/phizzl)

## Credits

The original project is [wez/govee2mqtt](https://github.com/wez/govee2mqtt), which grew out
of Wez Furlong's earlier [Govee LAN Control](https://github.com/wez/govee-lan-hass/).

The AWS IoT support was made possible by the work of @bwp91 in
[homebridge-govee](https://github.com/bwp91/homebridge-govee/).

The Bluetooth frame formats were reconstructed from captures against real devices, building
on the reverse engineering published by AlgoClaw and egold555.

