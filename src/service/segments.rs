//! Batching for per-segment commands.
//!
//! Govee's `segmentedColorRgb` and `segmentedBrightness` capabilities take an
//! *array* of segment indices, but Home Assistant models each segment as its
//! own light entity and so sends one command per segment. Left alone that
//! turns a scene over a fifteen-segment device into fifteen cloud requests.
//!
//! This collects the commands that arrive together and groups them by the value
//! they ask for. Frame transports carry them in one message or session; the
//! Platform API needs one request per distinct value.

use crate::ble::{Base64HexBytes, SetSegmentBrightness, SetSegmentColorRgb};
use crate::platform_api::HttpDeviceInfo;
use crate::service::device::Device;
use crate::service::quirks::supports_lan_segment_color;
use crate::service::state::StateHandle;
use crate::service::transport::{resolve_transport_order, TransportId};
use anyhow::Context;
use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{oneshot, Mutex};

/// How long to gather segment commands before sending them.
///
/// Home Assistant fans a service call out across entities concurrently, so the
/// members of one scene land within a few milliseconds of each other.
const WINDOW: Duration = Duration::from_millis(150);

type Waiter = oneshot::Sender<Result<(), String>>;

#[derive(Default)]
struct Pending {
    /// Segments wanting each colour.
    rgb: HashMap<(u8, u8, u8), Vec<u32>>,
    /// Segments wanting each brightness.
    brightness: HashMap<u8, Vec<u32>>,
    waiters: Vec<Waiter>,
    flush_scheduled: bool,
}

impl Pending {
    /// Record one segment's wishes, grouped by the value asked for.
    fn add(&mut self, segment: u32, brightness: Option<u8>, rgb: Option<(u8, u8, u8)>) {
        if let Some(percent) = brightness {
            self.brightness.entry(percent).or_default().push(segment);
        }
        if let Some(colour) = rgb {
            self.rgb.entry(colour).or_default().push(segment);
        }
    }

    /// How many requests this batch will cost.
    fn request_count(&self) -> usize {
        self.brightness.len() + self.rgb.len()
    }

    /// What the Platform API costs, for the log line.
    ///
    /// Reached only when no frame transport carried the batch, so
    /// there is nothing to subtract: both colour and brightness now travel as
    /// frames, and they travel together or not at all.
    fn platform_only(&self) -> (usize, usize) {
        (
            self.request_count(),
            self.brightness.values().map(Vec::len).sum::<usize>()
                + self.rgb.values().map(Vec::len).sum::<usize>(),
        )
    }
}

#[derive(Default)]
pub struct SegmentBatcher {
    devices: Mutex<HashMap<String, Pending>>,
}

impl SegmentBatcher {
    /// Queue a change to one segment and wait for the request that carries it.
    pub async fn apply(
        self: &Arc<Self>,
        state: &StateHandle,
        device: &Device,
        segment: u32,
        brightness: Option<u8>,
        rgb: Option<(u8, u8, u8)>,
    ) -> anyhow::Result<()> {
        if brightness.is_none() && rgb.is_none() {
            return Ok(());
        }

        let (tx, rx) = oneshot::channel();
        let device_id = device.id.to_string();

        {
            let mut devices = self.devices.lock().await;
            let pending = devices.entry(device_id.clone()).or_default();

            pending.add(segment, brightness, rgb);
            pending.waiters.push(tx);

            if !pending.flush_scheduled {
                pending.flush_scheduled = true;
                let batcher = self.clone();
                let state = state.clone();
                let id = device_id.clone();
                tokio::spawn(async move {
                    batcher.flush_after_window(state, id).await;
                });
            }
        }

        rx.await
            .map_err(|_| anyhow::anyhow!("segment batch for {device} was dropped"))?
            .map_err(|err| anyhow::anyhow!("{err}"))
    }

    async fn flush_after_window(self: Arc<Self>, state: StateHandle, device_id: String) {
        tokio::time::sleep(WINDOW).await;

        let pending = {
            let mut devices = self.devices.lock().await;
            let Some(pending) = devices.get_mut(&device_id) else {
                return;
            };
            pending.flush_scheduled = false;
            std::mem::take(pending)
        };

        let outcome = self
            .send(&state, &device_id, &pending)
            .await
            .map_err(|err| format!("{err:#}"));

        if outcome.is_ok() {
            Self::remember_what_we_asked_for(&state, &device_id, &pending).await;
        }

        for waiter in pending.waiters {
            let _ = waiter.send(outcome.clone());
        }
    }

    /// Write the commanded colours into the device's own picture.
    ///
    /// Without this the segment topic keeps carrying the colour the device last
    /// *reported* until a poll brings a fresh `aa a5` page — so Home Assistant
    /// showed the new colour, snapped back to the old one as soon as anything
    /// republished the device, and moved to the new one seconds later. The
    /// device's own report still corrects this when it arrives.
    ///
    /// Brightness is deliberately not recorded: the byte a device reports per
    /// segment is on a scale we have not identified, so a percentage from a
    /// command cannot be written into the same field.
    async fn remember_what_we_asked_for(state: &StateHandle, device_id: &str, pending: &Pending) {
        let Some(device) = state.device_by_id(device_id).await else {
            return;
        };

        let changed = {
            let mut device = state.device_mut(&device.sku, &device.id).await;
            let mut changed = false;
            for ((r, g, b), segments) in &pending.rgb {
                for segment in segments {
                    changed |= device.set_commanded_segment_color(*segment, *r, *g, *b);
                }
            }
            changed
        };

        // Outside the guard: notifying re-reads the device and would deadlock.
        if changed {
            if let Err(err) = state.notify_of_state_change(device_id).await {
                log::error!("publishing commanded segment colours for {device_id}: {err:#}");
            }
        }
    }

    async fn send(
        &self,
        state: &StateHandle,
        device_id: &str,
        pending: &Pending,
    ) -> anyhow::Result<()> {
        // One permit for the whole batch, taken here rather than by each
        // arriving command. `Coordinator` also schedules the read-back when it
        // drops, so a batch of twelve segments now costs one status request
        // instead of twelve. Held until this function returns, which is after
        // the last transport has had its turn.
        let control = state.resolve_device_for_control(device_id).await?;
        let device = (*control).clone();

        let override_order = state.get_transport_order().await;
        let order = Self::transport_order(&device, pending, override_order.as_deref());
        let sent = Self::try_transports(&order, |id| {
            let device = &device;
            async move {
                let result = match id {
                    TransportId::Lan => self.send_via_lan(device, pending).await,
                    TransportId::Iot => self.send_via_iot(state, device, pending).await,
                    TransportId::Ble => self.send_via_ble(state, device, pending).await,
                    TransportId::Platform => self.send_via_platform(state, device, pending).await,
                    TransportId::Nightlight => return Ok(false),
                };
                match result {
                    Err(err) if matches!(id, TransportId::Iot | TransportId::Ble) => {
                        log::warn!(
                            "Setting segments for {device} over {id} failed: {err:#}; \
                             trying next transport"
                        );
                        Ok(false)
                    }
                    result => result,
                }
            }
        })
        .await?;

        if sent {
            return Ok(());
        }

        anyhow::bail!("set segments for {device}: no transport available; tried {order:?}");
    }

    fn lan_eligible(device: &Device, pending: &Pending) -> bool {
        supports_lan_segment_color(&device.sku)
            && !pending.rgb.is_empty()
            && pending.brightness.is_empty()
    }

    fn platform_eligible(info: &HttpDeviceInfo, pending: &Pending) -> bool {
        (pending.rgb.is_empty() || info.capability_by_instance("segmentedColorRgb").is_some())
            && (pending.brightness.is_empty() || info.supports_segmented_brightness().is_some())
    }

    fn transport_order(
        device: &Device,
        pending: &Pending,
        override_order: Option<&[TransportId]>,
    ) -> Vec<TransportId> {
        use TransportId::*;
        let default: &[TransportId] = if Self::lan_eligible(device, pending) {
            &[Lan, Iot, Ble, Platform]
        } else {
            &[Iot, Ble, Platform]
        };
        resolve_transport_order(default, override_order)
    }

    async fn try_transports<F, Fut>(order: &[TransportId], mut send: F) -> anyhow::Result<bool>
    where
        F: FnMut(TransportId) -> Fut,
        Fut: Future<Output = anyhow::Result<bool>>,
    {
        for &id in order {
            if send(id).await? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    async fn send_via_lan(&self, device: &Device, pending: &Pending) -> anyhow::Result<bool> {
        if !Self::lan_eligible(device, pending) {
            return Ok(false);
        }
        let Some(lan_device) = &device.lan_device else {
            return Ok(false);
        };

        let frames = Self::encode_base64(pending)?;
        log::info!(
            "Using LAN API to send {} segment colour change(s) for {device} \
             in one message, {} frame(s)",
            Self::touched(pending).len(),
            frames.len()
        );
        lan_device
            .send_real(frames)
            .await
            .with_context(|| format!("sending segment colours for {device} over LAN"))?;
        Ok(true)
    }

    async fn send_via_platform(
        &self,
        state: &StateHandle,
        device: &Device,
        pending: &Pending,
    ) -> anyhow::Result<bool> {
        let Some(client) = state.get_platform_client().await else {
            return Ok(false);
        };
        let Some(info) = &device.http_device_info else {
            return Ok(false);
        };
        if !Self::platform_eligible(info, pending) {
            return Ok(false);
        }

        let (requests, segments) = pending.platform_only();
        log::info!(
            "Using Platform API to set {segments} segment change(s) on {device} \
             in {requests} request(s)"
        );

        for (percent, segments) in &pending.brightness {
            client
                .set_segment_brightness(info, segments, *percent)
                .await?;
        }
        for ((r, g, b), segments) in &pending.rgb {
            client.set_segment_rgb(info, segments, *r, *g, *b).await?;
        }

        Ok(true)
    }

    /// Send the batch as raw frames over Bluetooth.
    ///
    /// Returns whether it carried them. Declines quietly when Bluetooth is off,
    /// excluded for this device, the executor is absent, or the circuit breaker
    /// is open — those are routing decisions, and the Platform API is still
    /// there.
    async fn send_via_ble(
        &self,
        state: &StateHandle,
        device: &Device,
        pending: &Pending,
    ) -> anyhow::Result<bool> {
        if pending.rgb.is_empty() && pending.brightness.is_empty() {
            return Ok(true);
        }

        let Some(scheduler) = state.get_ble_scheduler().await else {
            return Ok(false);
        };
        let Some(address) = scheduler.address_for(device) else {
            return Ok(false);
        };
        if !scheduler.is_available_for(device).await {
            return Ok(false);
        }

        let frames = Self::encode(pending)?;
        let touched = Self::touched(pending);
        log::info!(
            "Using Bluetooth to set {} segment change(s) on {device} in one session, \
             {} frame(s)",
            touched.len(),
            frames.len()
        );

        scheduler
            .send_frames(state, &device.id, &device.sku, &address, &frames, &touched)
            .await?;
        Ok(true)
    }

    /// One frame per distinct value, each naming its own segments.
    ///
    /// Colour and brightness travel together: they are independent frames and a
    /// device applies several from one message, so a batch that changes both
    /// costs one message rather than two.
    fn encode(pending: &Pending) -> anyhow::Result<Vec<Vec<u8>>> {
        let colours = pending.rgb.iter().map(|((r, g, b), segments)| {
            let command = SetSegmentColorRgb::for_segments(segments.iter().copied(), (*r, *g, *b))?;
            crate::ble::encode_for_generic_light(&command)
        });
        let brightnesses = pending.brightness.iter().map(|(percent, segments)| {
            let command = SetSegmentBrightness::for_segments(segments.iter().copied(), *percent)?;
            crate::ble::encode_for_generic_light(&command)
        });
        colours.chain(brightnesses).collect()
    }

    fn encode_base64(pending: &Pending) -> anyhow::Result<Vec<String>> {
        Ok(Self::encode(pending)?
            .into_iter()
            .flat_map(|raw| Base64HexBytes::with_bytes(raw).base64())
            .collect())
    }

    /// Every segment the batch touches, for the read-back.
    fn touched(pending: &Pending) -> Vec<u32> {
        pending
            .rgb
            .values()
            .chain(pending.brightness.values())
            .flatten()
            .copied()
            .collect()
    }

    /// Send the batch as raw frames over AWS IoT.
    ///
    /// Returns whether it carried them. Every colour rides in one `ptReal`
    /// message: the frames are independent, each naming its own segments, and a
    /// device applies several from one message — measured while restoring a
    /// lamp after the reverse-engineering session.
    ///
    async fn send_via_iot(
        &self,
        state: &StateHandle,
        device: &Device,
        pending: &Pending,
    ) -> anyhow::Result<bool> {
        if pending.rgb.is_empty() && pending.brightness.is_empty() {
            return Ok(true);
        }

        let Some(iot) = state.get_iot_client().await else {
            return Ok(false);
        };
        let Some(info) = device.undoc_device_info.as_ref() else {
            return Ok(false);
        };
        if !iot.is_device_compatible(&info.entry) {
            return Ok(false);
        }

        let frames = Self::encode_base64(pending)?;

        let count = Self::touched(pending).len();
        log::info!(
            "Using AWS IoT to set {count} segment change(s) on {device} in one message, \
             {} frame(s)",
            frames.len()
        );

        iot.send_real(&info.entry, frames).await?;
        Ok(true)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::lan_api::LanDevice;
    use crate::platform_api::GoveeApiClient;
    use crate::service::state::State;
    use serde_json::{json, Value};
    use std::future::ready;
    use tokio::net::UdpSocket;
    use tokio::time::timeout;

    fn colour_batch() -> Pending {
        let mut pending = Pending::default();
        pending.add(0, None, Some((255, 0, 0)));
        pending
    }

    fn lan_device(sku: &str) -> Device {
        let mut device = Device::new(sku, "test-device");
        device.lan_device = Some(LanDevice {
            ip: "127.0.0.1".parse().unwrap(),
            device: device.id.clone(),
            sku: sku.to_string(),
            ble_version_hard: String::new(),
            ble_version_soft: String::new(),
            wifi_version_hard: String::new(),
            wifi_version_soft: String::new(),
        });
        device
    }

    fn platform_info(capabilities: Vec<Value>) -> HttpDeviceInfo {
        serde_json::from_value(json!({
            "sku": "H6054",
            "device": "test-device",
            "type": "devices.types.light",
            "capabilities": capabilities,
        }))
        .unwrap()
    }

    fn rgb_capability() -> Value {
        json!({
            "type": "devices.capabilities.segment_color_setting",
            "instance": "segmentedColorRgb",
        })
    }

    fn brightness_capability() -> Value {
        json!({
            "type": "devices.capabilities.segment_color_setting",
            "instance": "segmentedBrightness",
            "parameters": {
                "dataType": "STRUCT",
                "fields": [{
                    "fieldName": "brightness",
                    "dataType": "INTEGER",
                    "range": {"min": 0, "max": 100, "precision": 1},
                }],
            },
        })
    }

    #[test]
    fn platform_preflight_requires_every_requested_capability() {
        let colour = colour_batch();
        let mut brightness = Pending::default();
        brightness.add(0, Some(50), None);
        let mut mixed = colour_batch();
        mixed.add(0, Some(50), None);

        for (capabilities, colour_ok, brightness_ok, mixed_ok) in [
            (vec![], false, false, false),
            (vec![rgb_capability()], true, false, false),
            (vec![brightness_capability()], false, true, false),
            (
                vec![rgb_capability(), brightness_capability()],
                true,
                true,
                true,
            ),
        ] {
            let info = platform_info(capabilities);
            assert_eq!(SegmentBatcher::platform_eligible(&info, &colour), colour_ok);
            assert_eq!(
                SegmentBatcher::platform_eligible(&info, &brightness),
                brightness_ok
            );
            assert_eq!(SegmentBatcher::platform_eligible(&info, &mixed), mixed_ok);
        }
    }

    #[test]
    fn malformed_platform_brightness_capabilities_are_not_eligible() {
        let info = platform_info(vec![
            rgb_capability(),
            json!({
                "type": "devices.capabilities.segment_color_setting",
                "instance": "segmentedBrightness",
            }),
        ]);
        let mut pending = colour_batch();
        pending.add(0, Some(50), None);
        assert!(!SegmentBatcher::platform_eligible(&info, &pending));
    }

    #[tokio::test]
    async fn platform_declines_missing_capabilities_with_a_configured_client() {
        let state = Arc::new(State::new());
        state
            .set_platform_client(GoveeApiClient::new("unused-test-key"))
            .await;
        let batcher = SegmentBatcher::default();

        for (capabilities, brightness, rgb) in [
            (vec![], None, Some((255, 0, 0))),
            (vec![], Some(50), None),
            (vec![rgb_capability()], Some(50), Some((255, 0, 0))),
        ] {
            let mut device = Device::new("H6054", "test-device");
            device.http_device_info = Some(platform_info(capabilities));
            let mut pending = Pending::default();
            pending.add(0, brightness, rgb);
            assert!(!batcher
                .send_via_platform(&state, &device, &pending)
                .await
                .unwrap());
        }
    }

    #[tokio::test]
    async fn platform_first_can_decline_to_a_working_bluetooth_route() {
        use TransportId::*;
        let state = Arc::new(State::new());
        state
            .set_platform_client(GoveeApiClient::new("unused-test-key"))
            .await;
        state.set_transport_order(Some(vec![Platform, Ble])).await;
        let mut device = Device::new("H6054", "test-device");
        device.http_device_info = Some(platform_info(vec![]));
        let pending = colour_batch();
        let batcher = SegmentBatcher::default();
        let priority = state.get_transport_order().await;
        let order = SegmentBatcher::transport_order(&device, &pending, priority.as_deref());
        let mut tried = vec![];

        assert!(SegmentBatcher::try_transports(&order, |id| {
            tried.push(id);
            let state = &state;
            let device = &device;
            let pending = &pending;
            let batcher = &batcher;
            async move {
                match id {
                    Platform => batcher.send_via_platform(state, device, pending).await,
                    Ble => Ok(true),
                    _ => panic!("unexpected fallback after working Bluetooth: {id}"),
                }
            }
        })
        .await
        .unwrap());
        assert_eq!(tried, vec![Platform, Ble]);
    }

    #[tokio::test]
    async fn real_batcher_sends_one_lan_datagram_without_cloud_clients() {
        let receiver = UdpSocket::bind(("127.0.0.1", 4003))
            .await
            .expect("LAN integration test requires loopback UDP port 4003");
        let state = Arc::new(State::new());
        let device = lan_device("H7020");
        *state.device_mut(&device.sku, &device.id).await = device.clone();
        let batcher = state.segment_batcher().clone();

        assert!(state.get_platform_client().await.is_none());
        assert!(state.get_iot_client().await.is_none());
        assert!(state.get_ble_scheduler().await.is_none());

        let (blue, green) = timeout(Duration::from_secs(2), async {
            tokio::join!(
                batcher.apply(&state, &device, 0, None, Some((0, 0, 255))),
                batcher.apply(&state, &device, 1, None, Some((0, 255, 0))),
            )
        })
        .await
        .expect("segment batch did not complete");
        blue.unwrap();
        green.unwrap();

        let mut buffer = [0; 4096];
        let (size, source) = timeout(Duration::from_secs(2), receiver.recv_from(&mut buffer))
            .await
            .expect("successful batch did not send a LAN datagram")
            .unwrap();
        assert!(source.ip().is_loopback());
        let mut message: Value = serde_json::from_slice(&buffer[..size]).unwrap();
        let commands = message["msg"]["data"]["command"].as_array_mut().unwrap();
        commands.sort_by(|a, b| a.as_str().cmp(&b.as_str()));
        let mut expected = vec![
            "MwUVAQAA/wAAAAAAAQAAAAAAANw=",
            "MwUVAQD/AAAAAAAAAgAAAAAAAN8=",
        ];
        expected.sort();
        assert_eq!(
            message,
            json!({
                "msg": {
                    "cmd": "ptReal",
                    "data": {"command": expected},
                },
            })
        );
        assert!(
            timeout(Duration::from_millis(200), receiver.recv_from(&mut buffer))
                .await
                .is_err(),
            "colours were split into more than one datagram"
        );
        let updated = state.device_by_id(&device.id).await.unwrap();
        let blue = updated.segment_color(0).unwrap();
        let green = updated.segment_color(1).unwrap();
        assert_eq!((blue.r, blue.g, blue.b), (0, 0, 255));
        assert_eq!((green.r, green.g, green.b), (0, 255, 0));
    }

    #[test]
    fn lan_colour_support_is_limited_to_verified_models() {
        assert!(supports_lan_segment_color("H7020"));
        for sku in ["H7021", "H6072", "unknown"] {
            assert!(!supports_lan_segment_color(sku));
        }
    }

    #[test]
    fn verified_colour_batches_prefer_lan() {
        use TransportId::*;
        assert_eq!(
            SegmentBatcher::transport_order(&lan_device("H7020"), &colour_batch(), None),
            vec![Lan, Iot, Ble, Platform]
        );
    }

    #[test]
    fn other_models_keep_the_existing_default_routes() {
        use TransportId::*;
        assert_eq!(
            SegmentBatcher::transport_order(&lan_device("H7021"), &colour_batch(), None),
            vec![Iot, Ble, Platform]
        );
    }

    #[test]
    fn brightness_and_mixed_batches_do_not_offer_lan() {
        use TransportId::*;
        for rgb in [None, Some((255, 0, 0))] {
            let mut pending = Pending::default();
            pending.add(0, Some(50), rgb);
            assert_eq!(
                SegmentBatcher::transport_order(
                    &lan_device("H7020"),
                    &pending,
                    Some(&[Lan, Nightlight])
                ),
                vec![Iot, Ble, Platform]
            );
        }
    }

    #[test]
    fn segment_routes_honour_the_configured_priority_prefix() {
        use TransportId::*;
        assert_eq!(
            SegmentBatcher::transport_order(
                &lan_device("H7020"),
                &colour_batch(),
                Some(&[Iot, Platform])
            ),
            vec![Iot, Platform, Lan, Ble]
        );
        assert_eq!(
            SegmentBatcher::transport_order(
                &lan_device("H7020"),
                &colour_batch(),
                Some(&[Nightlight, Ble])
            ),
            vec![Ble, Lan, Iot, Platform]
        );
    }

    #[test]
    fn an_override_cannot_enable_lan_on_an_unverified_model() {
        use TransportId::*;
        assert_eq!(
            SegmentBatcher::transport_order(
                &lan_device("H6072"),
                &colour_batch(),
                Some(&[Lan, Ble])
            ),
            vec![Ble, Iot, Platform]
        );
    }

    #[test]
    fn empty_batches_do_not_offer_lan() {
        assert!(!SegmentBatcher::lan_eligible(
            &lan_device("H7020"),
            &Pending::default()
        ));
    }

    #[test]
    fn lan_frames_match_the_hardware_test_packets() {
        for (segment, rgb, expected) in [
            (0, (255, 0, 0), "MwUVAf8AAAAAAAAAAQAAAAAAANw="),
            (0, (0, 0, 255), "MwUVAQAA/wAAAAAAAQAAAAAAANw="),
            (1, (0, 255, 0), "MwUVAQD/AAAAAAAAAgAAAAAAAN8="),
        ] {
            let mut pending = Pending::default();
            pending.add(segment, None, Some(rgb));
            assert_eq!(
                SegmentBatcher::encode_base64(&pending).unwrap(),
                vec![expected.to_string()]
            );
        }
    }

    #[test]
    fn several_colours_share_one_command_array() {
        let mut pending = Pending::default();
        pending.add(0, None, Some((0, 0, 255)));
        pending.add(1, None, Some((0, 255, 0)));
        let mut frames = SegmentBatcher::encode_base64(&pending).unwrap();
        frames.sort();
        let mut expected = vec![
            "MwUVAQAA/wAAAAAAAQAAAAAAANw=".to_string(),
            "MwUVAQD/AAAAAAAAAgAAAAAAAN8=".to_string(),
        ];
        expected.sort();
        assert_eq!(frames, expected);
    }

    #[test]
    fn a_fifteen_bulb_palette_has_valid_frames_and_masks() {
        let mut pending = Pending::default();
        for segment in 0..15 {
            pending.add(segment, None, Some((segment as u8, 128, 255)));
        }
        let frames = SegmentBatcher::encode_base64(&pending).unwrap();
        assert_eq!(frames.len(), 15);
        let mut masks = vec![];
        for frame in frames {
            let bytes = data_encoding::BASE64.decode(frame.as_bytes()).unwrap();
            assert_eq!(bytes.len(), 20);
            assert_eq!(&bytes[..4], &[0x33, 0x05, 0x15, 0x01]);
            assert_eq!(bytes.iter().fold(0, |checksum, byte| checksum ^ byte), 0);
            masks.push(u32::from_le_bytes(bytes[12..16].try_into().unwrap()));
        }
        masks.sort();
        assert_eq!(
            masks,
            (0..15).map(|segment| 1u32 << segment).collect::<Vec<_>>()
        );
    }

    #[test]
    fn invalid_segment_indices_fail_before_sending() {
        let mut pending = Pending::default();
        pending.add(u32::MAX, None, Some((255, 0, 0)));
        assert!(SegmentBatcher::encode_base64(&pending).is_err());
    }

    #[tokio::test]
    async fn missing_lan_discovery_declines_without_sending() {
        let device = Device::new("H7020", "test-device");
        assert!(!SegmentBatcher::default()
            .send_via_lan(&device, &colour_batch())
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn unsupported_lan_batches_decline_without_sending() {
        let batcher = SegmentBatcher::default();
        assert!(!batcher
            .send_via_lan(&lan_device("H7021"), &colour_batch())
            .await
            .unwrap());
        let mut pending = colour_batch();
        pending.add(0, Some(50), None);
        assert!(!batcher
            .send_via_lan(&lan_device("H7020"), &pending)
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn a_handled_lan_batch_stops_before_cloud_transports() {
        use TransportId::*;
        let mut tried = vec![];
        assert!(
            SegmentBatcher::try_transports(&[Lan, Iot, Ble, Platform], |id| {
                tried.push(id);
                ready(Ok(true))
            })
            .await
            .unwrap()
        );
        assert_eq!(tried, vec![Lan]);
    }

    #[tokio::test]
    async fn a_declined_lan_batch_can_use_the_next_transport() {
        use TransportId::*;
        let mut tried = vec![];
        assert!(
            SegmentBatcher::try_transports(&[Lan, Iot, Ble, Platform], |id| {
                tried.push(id);
                ready(Ok(id == Iot))
            })
            .await
            .unwrap()
        );
        assert_eq!(tried, vec![Lan, Iot]);
    }

    #[tokio::test]
    async fn a_send_error_is_not_hidden_by_cloud_fallback() {
        use TransportId::*;
        let mut tried = vec![];
        let error = SegmentBatcher::try_transports(&[Lan, Iot], |id| {
            tried.push(id);
            ready(Err(
                anyhow::anyhow!("test UDP send failed").context("sending LAN batch")
            ))
        })
        .await
        .unwrap_err();
        assert_eq!(
            format!("{error:#}"),
            "sending LAN batch: test UDP send failed"
        );
        assert_eq!(tried, vec![Lan]);
    }

    #[tokio::test]
    async fn all_declined_transports_do_not_report_success() {
        assert!(
            !SegmentBatcher::try_transports(&[TransportId::Lan, TransportId::Iot], |_| {
                ready(Ok(false))
            })
            .await
            .unwrap()
        );
    }

    #[tokio::test]
    async fn segment_routing_reads_the_configured_state_priority() {
        use TransportId::*;
        let state = crate::service::state::State::new();
        assert_eq!(state.get_transport_order().await, None);
        state.set_transport_order(Some(vec![Iot, Lan])).await;
        let priority = state.get_transport_order().await;
        assert_eq!(
            SegmentBatcher::transport_order(
                &lan_device("H7020"),
                &colour_batch(),
                priority.as_deref()
            ),
            vec![Iot, Lan, Ble, Platform]
        );
    }

    #[test]
    fn segments_sharing_a_colour_travel_together() {
        // What a scene over a Govee floor lamp looks like: two colours across
        // four segments. Upstream sent four requests; this sends two.
        let mut pending = Pending::default();
        pending.add(0, None, Some((0, 0, 255)));
        pending.add(1, None, Some((0, 0, 255)));
        pending.add(6, None, Some((255, 0, 0)));
        pending.add(7, None, Some((255, 0, 0)));

        assert_eq!(pending.request_count(), 2);
        assert_eq!(pending.rgb[&(0, 0, 255)], vec![0, 1]);
        assert_eq!(pending.rgb[&(255, 0, 0)], vec![6, 7]);
    }

    #[test]
    fn colour_and_brightness_are_separate_requests() {
        // They are different capabilities, so they cannot share one call even
        // when they target the same segment.
        let mut pending = Pending::default();
        pending.add(0, Some(50), Some((255, 255, 255)));

        assert_eq!(pending.request_count(), 2);
    }

    /// One encoder feeds both channels: the bytes that go out over the radio
    /// are the same bytes that go out base64-wrapped over AWS IoT.
    #[test]
    fn each_distinct_colour_becomes_one_frame() {
        let mut pending = Pending::default();
        pending.add(0, None, Some((0, 0, 255)));
        pending.add(1, None, Some((0, 0, 255)));
        pending.add(5, None, Some((255, 255, 255)));

        let frames = SegmentBatcher::encode(&pending).unwrap();
        assert_eq!(frames.len(), 2, "one frame per distinct colour");

        // Both are the segment colour command, and every mask bit set is one
        // of the segments that asked for that colour.
        for frame in &frames {
            assert_eq!(&frame[..4], &[0x33, 0x05, 0x15, 0x01]);
            let colour = (frame[4], frame[5], frame[6]);
            let mask = frame[12];
            let expected: u8 = pending.rgb[&colour].iter().map(|n| 1u8 << n).sum();
            assert_eq!(mask, expected, "mask for {colour:?}");
        }
    }

    /// Brightness is a frame now too, and rides the same message.
    ///
    /// Until the Govee app's own traffic was captured this went to the Platform
    /// API, one request per distinct value, because the command was unknown.
    #[test]
    fn colour_and_brightness_ride_one_message() {
        let mut pending = Pending::default();
        pending.add(0, Some(40), Some((255, 0, 0)));
        pending.add(1, Some(40), None);

        let frames = SegmentBatcher::encode(&pending).unwrap();
        assert_eq!(frames.len(), 2, "one colour frame and one brightness frame");

        let colour = frames.iter().find(|f| f[3] == 0x01).expect("colour frame");
        let bright = frames
            .iter()
            .find(|f| f[3] == 0x02)
            .expect("brightness frame");

        // The colour names segment 0 only, with the mask at byte 12.
        assert_eq!((colour[4], colour[5], colour[6]), (255, 0, 0));
        assert_eq!(colour[12], 0b0000_0001);

        // The brightness names both, with the mask right behind the value.
        assert_eq!(bright[4], 40);
        assert_eq!(bright[5], 0b0000_0011);

        assert_eq!(SegmentBatcher::touched(&pending).len(), 3);
    }

    /// What the Platform API would cost, when it is reached at all.
    ///
    /// It no longer is for a device with an AWS IoT or Bluetooth path: colour
    /// and brightness are both frames now, they ride one message, and they
    /// travel together or not at all. This counts the fallback.
    #[test]
    fn the_platform_fallback_costs_one_request_per_distinct_value() {
        let mut pending = Pending::default();
        pending.add(0, Some(50), Some((255, 0, 0)));
        pending.add(1, None, Some((255, 0, 0)));
        pending.add(2, None, Some((0, 0, 255)));

        // Two colours plus one brightness, over four segment changes.
        assert_eq!(pending.platform_only(), (3, 4));
    }

    #[test]
    fn one_segment_still_costs_one_request() {
        let mut pending = Pending::default();
        pending.add(3, None, Some((1, 2, 3)));

        assert_eq!(pending.request_count(), 1);
        assert_eq!(pending.rgb[&(1, 2, 3)], vec![3]);
    }
}
