use crate::ble::{NotifyHumidifierNightlightParams, NotifySegmentColors, SegmentColor};
use crate::commands::serve::POLL_INTERVAL;
use crate::lan_api::{DeviceColor, DeviceStatus as LanDeviceStatus, LanDevice};
use crate::platform_api::{
    DeviceCapability, DeviceCapabilityState, DeviceType, HttpDeviceInfo, HttpDeviceState,
};
use crate::service::quirks::{resolve_quirk, Quirk, BULB};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::HashMap;
use std::net::IpAddr;

#[derive(Default, Clone, Debug)]
pub struct Device {
    pub sku: String,
    pub id: String,

    /// Probed LAN device information, found either via discovery
    /// or explicit probing by IP address
    pub lan_device: Option<LanDevice>,
    pub last_lan_device_update: Option<DateTime<Utc>>,

    pub lan_device_status: Option<LanDeviceStatus>,
    pub last_lan_device_status_update: Option<DateTime<Utc>>,

    pub http_device_info: Option<HttpDeviceInfo>,
    pub last_http_device_update: Option<DateTime<Utc>>,

    pub http_device_state: Option<HttpDeviceState>,
    pub last_http_device_state_update: Option<DateTime<Utc>>,

    pub undoc_device_info: Option<UndocDeviceInfo>,
    pub last_undoc_device_info_update: Option<DateTime<Utc>>,

    pub iot_device_status: Option<LanDeviceStatus>,
    pub last_iot_device_status_update: Option<DateTime<Utc>>,

    /// Status assembled from Bluetooth notifications. Unlike the other sources
    /// this arrives one attribute at a time, so it is merged rather than
    /// replaced.
    pub ble_device_status: Option<LanDeviceStatus>,
    pub last_ble_device_status_update: Option<DateTime<Utc>>,

    /// Per-segment colours, keyed by the Govee segment index. Only the
    /// undocumented AWS IoT channel reports these, and only in reply to a
    /// status request, so they refresh on the poll interval rather than live.
    pub segment_colors: HashMap<u32, SegmentColor>,
    pub last_segment_colors_update: Option<DateTime<Utc>>,

    pub nightlight_state: Option<NotifyHumidifierNightlightParams>,
    pub target_humidity_percent: Option<u8>,
    pub humidifier_work_mode: Option<u8>,
    pub humidifier_param_by_mode: HashMap<u8, u8>,

    pub last_polled: Option<DateTime<Utc>>,

    active_scene: Option<ActiveSceneInfo>,
}

impl std::fmt::Display for Device {
    fn fmt(&self, fmt: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(fmt, "{} ({} {})", self.name(), self.id, self.sku)
    }
}

/// Govee doesn't report the active scene or music mode,
/// so we maintain our own idea of it, clearing it when
/// the color of the light is changed
#[derive(Clone, Debug)]
struct ActiveSceneInfo {
    pub name: String,
    pub color: crate::lan_api::DeviceColor,
    pub kelvin: u32,
}

/// Represents the device state; synthesized from the various
/// sources of facts that we have in the Device
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct DeviceState {
    /// Whether the device is powered on
    pub on: bool,
    /// Whether the light function of the device is powered on
    pub light_on: Option<bool>,

    /// Whether the device is connected to the Govee cloud
    pub online: Option<bool>,

    /// The color temperature in kelvin
    pub kelvin: u32,

    /// The color
    pub color: crate::lan_api::DeviceColor,

    /// The brightness in percent (0-100)
    pub brightness: u8,

    /// The active effect mode, if known
    pub scene: Option<String>,

    /// Where the information came from
    pub source: &'static str,
    pub updated: DateTime<Utc>,
}

/// Where a device's Bluetooth address came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BleAddressSource {
    /// Stated by Govee in the account metadata. Authoritative.
    Metadata,
    /// Taken from the last six octets of the device id, because the metadata
    /// carried none. A guess, and known to be off by one on H601B hardware.
    DerivedFromId,
}

#[derive(Debug, Clone)]
pub struct UndocDeviceInfo {
    pub room_name: Option<String>,
    pub entry: crate::undoc_api::DeviceEntry,
}

impl Device {
    /// Create a new device given just its sku and id.
    /// No other facts are known or reflected by it at this time;
    /// they will need to be added by the caller.
    pub fn new<S: Into<String>, I: Into<String>>(sku: S, id: I) -> Self {
        Self {
            sku: sku.into(),
            id: id.into(),
            ..Self::default()
        }
    }

    /// Returns the device name; either the name defined in the Govee App,
    /// or, if we don't have the information for some reason, then we compute
    /// a name from the SKU and the last couple of bytes from the device id,
    /// similar to the device name that would show up in a BLE scan, or
    /// the default name for the device if not otherwise configured in the
    /// Govee App.
    pub fn name(&self) -> String {
        if let Some(name) = self.govee_name() {
            return name.to_string();
        }
        self.computed_name()
    }

    /// Returns the name defined for the device in the Govee App
    pub fn govee_name(&self) -> Option<&str> {
        if let Some(info) = &self.http_device_info {
            return Some(&info.device_name);
        }
        None
    }

    pub fn room_name(&self) -> Option<&str> {
        if let Some(info) = &self.undoc_device_info {
            return info.room_name.as_deref();
        }
        None
    }

    /// compute a name from the SKU and the last couple of bytes from the
    /// device id, similar to the device name that would show up in a BLE
    /// scan, or the default name for the device if not otherwise configured
    /// in the Govee App.
    pub fn computed_name(&self) -> String {
        // The id is usually "XX:XX:XX:XX:XX:XX:XX:XX" but some devices
        // report it without colons, and in lowercase.  Normalize it.
        let mut id = String::new();
        for c in self.id.chars() {
            if c == ':' {
                continue;
            }
            id.push(c.to_ascii_uppercase());
        }

        format!("{}_{}", self.sku, &id[id.len().saturating_sub(4)..])
    }

    pub fn preferred_poll_interval(&self) -> chrono::Duration {
        match self.device_type() {
            // If the kettle is on, read its temperature more frequently
            DeviceType::Kettle => {
                if self.device_state().map(|s| s.on).unwrap_or(false) {
                    chrono::Duration::seconds(60)
                } else {
                    *POLL_INTERVAL
                }
            }
            _ => *POLL_INTERVAL,
        }
    }

    pub fn ip_addr(&self) -> Option<IpAddr> {
        self.lan_device.as_ref().map(|device| device.ip)
    }

    pub fn set_last_polled(&mut self) {
        self.last_polled.replace(Utc::now());
    }

    /// Merge one page of segment colours into the picture.
    ///
    /// Pages are merged rather than replacing the map wholesale: they arrive as
    /// separate frames, and a device that reports only some of them should
    /// still update the segments it did report.
    pub fn set_segment_colors(&mut self, page: &NotifySegmentColors) {
        let first = page.first_segment_index();
        for (n, segment) in page.segments.iter().enumerate() {
            self.segment_colors.insert(first + n as u32, *segment);
        }
        self.last_segment_colors_update.replace(Utc::now());
    }

    /// Colour last reported for one segment.
    ///
    /// A page always carries three slots, so the map can hold entries past the
    /// device's real segment count. Callers only ask about segments that have
    /// an entity, which keeps that filler out of Home Assistant.
    pub fn segment_color(&self, segment: u32) -> Option<SegmentColor> {
        self.segment_colors.get(&segment).copied()
    }

    pub fn set_nightlight_state(&mut self, params: NotifyHumidifierNightlightParams) {
        self.nightlight_state.replace(params);
    }

    pub fn set_target_humidity(&mut self, percent: u8) {
        self.target_humidity_percent.replace(percent);
    }

    pub fn set_humidifier_work_mode_and_param(&mut self, mode: u8, param: u8) {
        self.humidifier_work_mode.replace(mode);
        self.humidifier_param_by_mode.insert(mode, param);
    }

    /// Update the LAN device information
    pub fn set_lan_device(&mut self, device: LanDevice) {
        self.lan_device.replace(device);
        self.last_lan_device_update.replace(Utc::now());
    }

    /// Update the LAN device status information
    /// Apply what a Bluetooth notification told us.
    ///
    /// Each notification covers a single attribute, so this merges into the
    /// running picture instead of replacing it. `apply` is handed the current
    /// status to modify.
    pub fn update_ble_device_status<F: FnOnce(&mut LanDeviceStatus)>(&mut self, apply: F) -> bool {
        let mut status = match &self.ble_device_status {
            Some(status) => status.clone(),
            // Seed from whatever we last knew rather than from zeroes. A session
            // only reads back the attributes it changed, so switching a light on
            // would otherwise report brightness 0 and colour black as though we
            // had measured them.
            None => self
                .device_state()
                .map(|state| LanDeviceStatus {
                    on: state.on,
                    brightness: state.brightness,
                    color: state.color,
                    color_temperature_kelvin: state.kelvin,
                })
                .unwrap_or_default(),
        };
        (apply)(&mut status);

        let changed = self.ble_device_status.as_ref() != Some(&status);
        self.ble_device_status.replace(status);
        self.last_ble_device_status_update.replace(Utc::now());
        self.clear_scene_if_color_changed();
        changed
    }

    pub fn set_lan_device_status(&mut self, status: LanDeviceStatus) -> bool {
        let changed = self
            .lan_device_status
            .as_ref()
            .map(|prior| *prior != status)
            .unwrap_or(true);
        self.lan_device_status.replace(status);
        self.last_lan_device_status_update.replace(Utc::now());
        self.clear_scene_if_color_changed();
        changed
    }

    pub fn set_iot_device_status(&mut self, status: LanDeviceStatus) {
        self.iot_device_status.replace(status);
        self.last_iot_device_status_update.replace(Utc::now());
        self.clear_scene_if_color_changed();
    }

    pub fn set_http_device_info(&mut self, info: HttpDeviceInfo) {
        self.http_device_info.replace(info);
        self.last_http_device_update.replace(Utc::now());
    }

    pub fn set_http_device_state(&mut self, state: HttpDeviceState) {
        self.http_device_state.replace(state);
        self.last_http_device_state_update.replace(Utc::now());
        self.clear_scene_if_color_changed();
    }

    pub fn set_undoc_device_info(
        &mut self,
        entry: crate::undoc_api::DeviceEntry,
        room_name: Option<&str>,
    ) {
        self.undoc_device_info.replace(UndocDeviceInfo {
            entry,
            room_name: room_name.map(|s| s.to_string()),
        });
        self.last_undoc_device_info_update.replace(Utc::now());
        self.clear_scene_if_color_changed();
    }

    pub fn compute_ble_device_state(&self) -> Option<DeviceState> {
        let updated = self.last_ble_device_status_update?;
        let status = self.ble_device_status.as_ref()?;

        Some(DeviceState {
            on: status.on,
            light_on: Some(status.on),
            // Bluetooth says nothing about cloud connectivity, and claiming
            // otherwise would make a locally reachable device look offline.
            online: None,
            brightness: status.brightness,
            color: status.color,
            kelvin: status.color_temperature_kelvin,
            scene: self.active_scene.as_ref().map(|info| info.name.to_string()),
            source: "BLE",
            updated,
        })
    }

    pub fn compute_iot_device_state(&self) -> Option<DeviceState> {
        let updated = self.last_iot_device_status_update?;
        let status = self.iot_device_status.as_ref()?;

        Some(DeviceState {
            on: status.on,
            light_on: if self.device_type() == DeviceType::Light {
                Some(status.on)
            } else {
                self.nightlight_state.as_ref().map(|s| s.on)
            },
            online: None,
            brightness: status.brightness,
            color: status.color,
            kelvin: status.color_temperature_kelvin,
            scene: self.active_scene.as_ref().map(|info| info.name.to_string()),
            source: "AWS IoT API",
            updated,
        })
    }

    pub fn compute_lan_device_state(&self) -> Option<DeviceState> {
        let updated = self.last_lan_device_status_update?;
        let status = self.lan_device_status.as_ref()?;

        Some(DeviceState {
            on: status.on,
            light_on: Some(status.on), // assumption: LAN API == light
            online: None,
            brightness: status.brightness,
            color: status.color,
            kelvin: status.color_temperature_kelvin,
            scene: self.active_scene.as_ref().map(|info| info.name.to_string()),
            source: "LAN API",
            updated,
        })
    }

    pub fn compute_http_device_state(&self) -> Option<DeviceState> {
        let updated = self.last_http_device_state_update?;
        let state = self.http_device_state.as_ref()?;

        let mut online = None;
        let mut on = false;
        let mut light_on = None;
        let mut brightness = 0;
        let mut color = DeviceColor::default();
        let mut kelvin = 0;

        #[derive(serde::Deserialize)]
        struct IntegerValueState {
            value: u32,
        }
        #[derive(serde::Deserialize)]
        struct BoolValueState {
            value: bool,
        }

        let light_instance = self.get_light_power_toggle_instance_name();

        for cap in &state.capabilities {
            if let Ok(value) = serde_json::from_value::<IntegerValueState>(cap.state.clone()) {
                if light_instance
                    .map(|inst| inst == cap.instance.as_str())
                    .unwrap_or(false)
                {
                    light_on.replace(value.value != 0);
                }

                match cap.instance.as_str() {
                    "powerSwitch" => {
                        on = value.value != 0;
                    }
                    "colorRgb" => {
                        color = DeviceColor {
                            r: ((value.value >> 16) & 0xff) as u8,
                            g: ((value.value >> 8) & 0xff) as u8,
                            b: (value.value & 0xff) as u8,
                        };
                    }
                    "brightness" => {
                        brightness = value.value as u8;
                    }
                    "colorTemperatureK" => {
                        kelvin = value.value;
                    }
                    _ => {}
                }
            } else if cap.instance == "online" {
                if let Ok(value) = serde_json::from_value::<BoolValueState>(cap.state.clone()) {
                    online.replace(value.value);
                }
            }
        }

        Some(DeviceState {
            on,
            light_on,
            online,
            brightness,
            color,
            kelvin,
            scene: self.active_scene.as_ref().map(|info| info.name.to_string()),
            source: "PLATFORM API",
            updated,
        })
    }

    /// Returns the most recently received state information
    pub fn device_state(&self) -> Option<DeviceState> {
        let mut candidates = vec![];

        if let Some(state) = self.compute_lan_device_state() {
            candidates.push(state);
        }
        if let Some(state) = self.compute_http_device_state() {
            candidates.push(state);
        }
        if let Some(state) = self.compute_iot_device_state() {
            candidates.push(state);
        }
        if let Some(state) = self.compute_ble_device_state() {
            candidates.push(state);
        }

        candidates.sort_by(|a, b| a.updated.cmp(&b.updated));

        candidates.pop()
    }

    /// Records the active scene name
    pub fn set_active_scene(&mut self, scene: Option<&str>) {
        match scene {
            None => {
                self.active_scene.take();
            }
            Some(scene) => {
                let (color, kelvin) = self
                    .device_state()
                    .map(|s| (s.color, s.kelvin))
                    .unwrap_or_default();
                self.active_scene.replace(ActiveSceneInfo {
                    name: scene.to_string(),
                    color,
                    kelvin,
                });
            }
        }
    }

    pub fn clear_scene_if_color_changed(&mut self) {
        if let Some(info) = &self.active_scene {
            let current = self
                .device_state()
                .map(|s| (s.color, s.kelvin))
                .unwrap_or_default();
            let scene_state = (info.color, info.kelvin);
            if current != scene_state {
                log::info!(
                    "Clearing reported scene because current {current:?} != {scene_state:?}"
                );
                self.active_scene.take();
            }
        }
    }

    pub fn device_type(&self) -> DeviceType {
        if let Some(info) = &self.http_device_info {
            info.device_type.clone()
        } else if let Some(q) = resolve_quirk(&self.sku) {
            q.device_type.clone()
        } else {
            DeviceType::Light
        }
    }

    /// Indicate whether we require the platform API data in order
    /// to correctly report the device
    pub fn needs_platform_poll(&self) -> bool {
        if !self.iot_api_supported() {
            return true;
        }

        let device_type = self.device_type();
        match (device_type, self.sku.as_str()) {
            (_, "H7160") => false,
            (DeviceType::Humidifier, _) => true,
            (DeviceType::Light, _) => false,
            (DeviceType::Kettle, _) => true,
            _ => true,
        }
    }

    pub fn pollable_via_lan(&self) -> bool {
        self.lan_device.is_some()
    }

    pub fn pollable_via_iot(&self) -> bool {
        if !self.iot_api_supported() {
            return false;
        }
        let device_type = self.device_type();
        matches!(
            (device_type, self.sku.as_str()),
            (_, "H7160") | (DeviceType::Light, _)
        )
    }

    pub fn avoid_platform_api(&self) -> bool {
        if let Some(quirk) = self.resolve_quirk() {
            if quirk.avoid_platform_api {
                return true;
            }
            if self.lan_device.is_some()
                && !self
                    .http_device_info
                    .as_ref()
                    .map(|info| info.supports_rgb())
                    .unwrap_or(false)
            {
                // Conflicting information:
                // Platform API says that this device isn't
                // a light, but the LAN API support suggests
                // that it is a light!
                // Therefore we will not trust the Platform API
                return true;
            }
        }
        false
    }

    pub fn resolve_quirk(&self) -> Option<Quirk> {
        match resolve_quirk(&self.sku) {
            Some(q) => Some(q.clone()),
            None => {
                // It's an unknown device, but since it showed up via LAN disco,
                // we can assume that it is a light
                if self.lan_device.is_some() {
                    Some(Quirk::light(Cow::Owned(self.sku.to_string()), BULB).with_lan_api())
                } else {
                    None
                }
            }
        }
    }

    pub fn get_capability_by_instance(&self, instance: &str) -> Option<&DeviceCapability> {
        self.http_device_info
            .as_ref()
            .and_then(|info| info.capability_by_instance(instance))
    }

    pub fn get_state_capability_by_instance(
        &self,
        instance: &str,
    ) -> Option<&DeviceCapabilityState> {
        self.http_device_state
            .as_ref()
            .and_then(|info| info.capability_by_instance(instance))
    }

    /// The device's Bluetooth address, if we can work it out.
    ///
    /// Two independent sources, which agree wherever both are present in the
    /// account data: the metadata states the address outright, and the Govee
    /// device id turns out to be that same address with two bytes prepended
    /// (`47:13:CF:00:00:00:00:25` carries `CF:00:00:00:00:25`).
    pub fn ble_address(&self) -> Option<String> {
        self.ble_address_with_source().map(|(address, _)| address)
    }

    /// The address together with where it came from.
    ///
    /// The distinction matters. Live traffic showed an H601B whose metadata
    /// address is the device id **plus one** — and that is the address the lamp
    /// answers on. So the derived form is not a second opinion on the same
    /// value, it is a guess that is systematically wrong for at least one
    /// device family. It only ever applies when Govee tells us nothing.
    pub fn ble_address_with_source(&self) -> Option<(String, BleAddressSource)> {
        fn is_mac(candidate: &str) -> bool {
            let octets: Vec<&str> = candidate.split(':').collect();
            octets.len() == 6
                && octets
                    .iter()
                    .all(|octet| octet.len() == 2 && octet.chars().all(|c| c.is_ascii_hexdigit()))
        }

        if let Some(info) = &self.undoc_device_info {
            if let Some(address) = &info.entry.device_ext.device_settings.address {
                if is_mac(address) {
                    return Some((address.to_uppercase(), BleAddressSource::Metadata));
                }
            }
        }

        let octets: Vec<&str> = self.id.split(':').collect();
        if octets.len() == 8
            && octets
                .iter()
                .all(|octet| octet.len() == 2 && octet.chars().all(|c| c.is_ascii_hexdigit()))
        {
            return Some((
                octets[2..].join(":").to_uppercase(),
                BleAddressSource::DerivedFromId,
            ));
        }

        None
    }

    pub fn get_light_power_toggle_instance_name(&self) -> Option<&'static str> {
        match self.device_type() {
            DeviceType::Light => Some("powerSwitch"),
            _ => {
                // If the device's primary function is not a light,
                // then we need to avoid powering on its other function
                // here.  If it has a nightlight capability, that is
                // probably what we are controlling.
                // We may need to expand this to other power toggles
                // in the future.
                if self
                    .get_capability_by_instance("nightlightToggle")
                    .is_some()
                {
                    Some("nightlightToggle")
                } else {
                    None
                }
            }
        }
    }

    pub fn get_color_temperature_range(&self) -> Option<(u32, u32)> {
        if let Some(quirk) = self.resolve_quirk() {
            return quirk.color_temp_range;
        }

        if self.lan_device.is_some() {
            // LAN API support suggests that it is a light
            return Some((2000, 9000));
        }

        if self.is_ble_only_light() {
            return Some((2000, 9000));
        }

        self.http_device_info
            .as_ref()
            .and_then(|info| info.get_color_temperature_range())
    }

    pub fn supports_brightness(&self) -> bool {
        if let Some(quirk) = self.resolve_quirk() {
            return quirk.supports_brightness;
        }

        if self.lan_device.is_some() {
            // LAN API support suggests that it is a light
            return true;
        }

        if self.is_ble_only_light() {
            return true;
        }

        self.http_device_info
            .as_ref()
            .map(|info| info.supports_brightness())
            .unwrap_or(false)
    }

    pub fn iot_api_supported(&self) -> bool {
        if let Some(quirk) = self.resolve_quirk() {
            return quirk.iot_api_supported;
        }

        false
    }

    /// Whether this device is addressed as a set of segments.
    ///
    /// Segmented devices accept the whole-device power frame but ignore the
    /// whole-strip colour command: an H613D switched on over Bluetooth and kept
    /// the colour it already had. Segment control has never been
    /// reverse-engineered, so anything beyond power would report success while
    /// doing nothing.
    ///
    /// Detected from the Platform capability rather than an SKU list. Note the
    /// gap that leaves: a Bluetooth-only segmented device has no Platform data
    /// to read this from, and will still be offered colour it cannot apply.
    pub fn is_segmented(&self) -> bool {
        self.http_device_info
            .as_ref()
            .and_then(|info| info.supports_segmented_rgb())
            .is_some()
    }

    /// Whether this device is a light we can only reach over Bluetooth.
    ///
    /// Such a device has no Platform capabilities to read and no LAN presence to
    /// infer from, so the usual capability questions all answer "no" and it ends
    /// up in Home Assistant without a light entity. But the `Generic:Light`
    /// command set applies to every Govee light, so what we can do is known
    /// regardless of what Govee's metadata says.
    pub fn is_ble_only_light(&self) -> bool {
        matches!(self.is_ble_only_device(), Some(true))
            && matches!(self.device_type(), DeviceType::Light)
            && self.ble_address().is_some()
    }

    pub fn supports_rgb(&self) -> bool {
        if let Some(quirk) = self.resolve_quirk() {
            return quirk.supports_rgb;
        }

        if self.lan_device.is_some() {
            // LAN API support suggests that it is a light
            return true;
        }

        if self.is_ble_only_light() {
            return true;
        }

        self.http_device_info
            .as_ref()
            .map(|info| info.supports_rgb())
            .unwrap_or(false)
    }

    pub fn is_ble_only_device(&self) -> Option<bool> {
        if let Some(quirk) = self.resolve_quirk() {
            return Some(quirk.ble_only);
        }

        if self.http_device_info.is_some() {
            // truly BLE-only devices are not returned via the Platform API,
            // unless we have a quirk to say otherwise
            return Some(false);
        }

        self.undoc_device_info
            .as_ref()
            .map(|info| info.entry.device_ext.device_settings.wifi_name.is_none())
    }

    /// Whether Bluetooth reported this device's state within `window`.
    ///
    /// Used to skip the post-control cloud poll: a BLE session reads back what
    /// it changed, so polling again would spend a cloud request on something we
    /// already know.
    pub fn has_fresh_ble_state(&self, window: chrono::Duration) -> bool {
        self.last_ble_device_status_update
            .is_some_and(|updated| Utc::now() - updated < window)
    }

    pub fn is_controllable(&self) -> bool {
        if !matches!(self.is_ble_only_device(), Some(true)) {
            return true;
        }

        // Bluetooth-only lights used to be hidden from Home Assistant because
        // there was no way to reach them. Now there is, provided we know the
        // address. Deliberately not conditioned on the executor being online:
        // entities appearing and disappearing with it would be worse than an
        // entity that is briefly unavailable.
        matches!(self.device_type(), DeviceType::Light) && self.ble_address().is_some()
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn name_compute() {
        let device = Device::new("H6000", "AA:BB:CC:DD:EE:FF:42:2A");
        assert_eq!(device.name(), "H6000_422A");

        let device = Device::new("H6127", "cef142b0b354995f");
        assert_eq!(device.name(), "H6127_995F");

        let device = Device::new("H6127", "ce");
        assert_eq!(device.name(), "H6127_CE");
    }
    #[test]
    fn ble_address_is_derived_from_the_device_id() {
        // Verified against test-data/undoc-device-list.json, where the account
        // metadata reports address=CF:00:00:00:00:25 for this device id.
        let device = Device::new("H6072", "47:13:CF:00:00:00:00:25");
        assert_eq!(device.ble_address().as_deref(), Some("CF:00:00:00:00:25"));
    }

    #[test]
    fn ble_address_is_upper_cased() {
        let device = Device::new("H6072", "47:13:cf:00:00:00:00:25");
        assert_eq!(device.ble_address().as_deref(), Some("CF:00:00:00:00:25"));
    }

    #[test]
    fn an_id_that_is_not_a_mac_yields_no_ble_address() {
        // Some devices report a bare hex id with no separators.
        assert_eq!(Device::new("H6127", "aabbccddeeff4222").ble_address(), None);
        assert_eq!(Device::new("H6127", "not-a-mac").ble_address(), None);
    }

    #[test]
    fn ble_status_updates_merge_rather_than_replace() {
        // Each notification covers one attribute, so brightness must survive a
        // later power notification.
        let mut device = Device::new("H6127", "AA:BB:CC:DD:EE:FF:11:22");
        assert!(device.update_ble_device_status(|status| status.brightness = 42));
        assert!(device.update_ble_device_status(|status| status.on = true));

        let status = device.ble_device_status.as_ref().unwrap();
        assert_eq!(status.brightness, 42);
        assert!(status.on);
    }

    /// A real account entry with the Wi-Fi name cleared. That absence is exactly
    /// what marks a device as Bluetooth-only, so this is the shape the heuristic
    /// actually sees.
    fn undoc_entry_without_wifi() -> crate::undoc_api::DeviceEntry {
        let response: crate::undoc_api::DevicesResponse =
            crate::platform_api::from_json(include_str!("../../test-data/undoc-device-list.json"))
                .unwrap();
        let mut entry = response.devices[0].clone();
        entry.device_ext.device_settings.wifi_name = None;
        entry
    }

    /// A Bluetooth-only light has no Platform capabilities and no LAN presence,
    /// so without this it lands in Home Assistant as a diagnostic sensor with no
    /// way to switch it on.
    #[test]
    fn a_ble_only_light_reports_the_generic_light_capabilities() {
        let mut device = Device::new("H6116", "7E:16:A4:C1:38:14:E6:5A");
        device.undoc_device_info.replace(UndocDeviceInfo {
            room_name: None,
            entry: undoc_entry_without_wifi(),
        });

        assert_eq!(device.is_ble_only_device(), Some(true));
        assert!(device.is_ble_only_light());
        assert!(device.supports_rgb());
        assert!(device.supports_brightness());
        assert_eq!(device.get_color_temperature_range(), Some((2000, 9000)));
    }

    #[test]
    fn the_metadata_address_wins_over_the_derived_one() {
        // The account states the address outright; deriving it from the device
        // id is only the fallback.
        let mut device = Device::new("H6116", "7E:16:A4:C1:38:14:E6:5A");
        device.undoc_device_info.replace(UndocDeviceInfo {
            room_name: None,
            entry: undoc_entry_without_wifi(),
        });

        assert_eq!(device.ble_address().as_deref(), Some("CF:00:00:00:00:25"));
    }

    #[test]
    fn a_device_without_a_ble_address_stays_uncontrollable() {
        // No address means nothing to connect to, so exposing it would only
        // produce an entity that can never work.
        let mut entry = undoc_entry_without_wifi();
        entry.device_ext.device_settings.address = None;

        let mut device = Device::new("H6116", "not-a-mac");
        device.undoc_device_info.replace(UndocDeviceInfo {
            room_name: None,
            entry,
        });

        assert_eq!(device.ble_address(), None);
        assert!(!device.is_ble_only_light());
        assert!(!device.is_controllable());
    }

    #[test]
    fn a_first_ble_update_inherits_what_we_already_knew() {
        // A session that only switches a light on reads back power alone. The
        // attributes it did not ask about must keep their known values rather
        // than appear as measured zeroes.
        let mut device = Device::new("H601B", "AA:BB:CC:DD:EE:FF:11:22");
        device.set_lan_device_status(LanDeviceStatus {
            on: false,
            brightness: 80,
            color: DeviceColor { r: 1, g: 2, b: 3 },
            color_temperature_kelvin: 2700,
        });

        device.update_ble_device_status(|status| status.on = true);

        let status = device.ble_device_status.as_ref().unwrap();
        assert!(status.on);
        assert_eq!(status.brightness, 80);
        assert_eq!(status.color, DeviceColor { r: 1, g: 2, b: 3 });
        assert_eq!(status.color_temperature_kelvin, 2700);
    }

    #[test]
    fn ble_freshness_gates_the_post_control_poll() {
        let mut device = Device::new("H601B", "AA:BB:CC:DD:EE:FF:11:22");
        assert!(!device.has_fresh_ble_state(chrono::Duration::seconds(15)));

        device.update_ble_device_status(|status| status.on = true);
        assert!(device.has_fresh_ble_state(chrono::Duration::seconds(15)));
        assert!(!device.has_fresh_ble_state(chrono::Duration::zero()));
    }

    #[test]
    fn an_unchanged_ble_status_reports_no_change() {
        let mut device = Device::new("H6127", "AA:BB:CC:DD:EE:FF:11:22");
        assert!(device.update_ble_device_status(|status| status.brightness = 42));
        assert!(!device.update_ble_device_status(|status| status.brightness = 42));
    }
}
