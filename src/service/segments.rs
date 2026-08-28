//! Batching for per-segment commands.
//!
//! Govee's `segmentedColorRgb` and `segmentedBrightness` capabilities take an
//! *array* of segment indices, but Home Assistant models each segment as its
//! own light entity and so sends one command per segment. Left alone that
//! turns a scene over a fifteen-segment device into fifteen cloud requests.
//!
//! This collects the commands that arrive together, groups them by the value
//! they ask for, and issues one request per distinct value.

use crate::ble::{Base64HexBytes, SetSegmentBrightness, SetSegmentColorRgb};
use crate::service::device::Device;
use crate::service::state::StateHandle;
use std::collections::HashMap;
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
    /// Reached only when neither AWS IoT nor Bluetooth carried the batch, so
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
            .map_err(|err| err.to_string());

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

        // Colour goes over AWS IoT when the device is reachable that way: one
        // message carries every colour in the batch, and it spends no Platform
        // API quota — where fifteen segments otherwise cost fifteen requests.
        let sent = match self.send_via_iot(state, &device, pending).await {
            Ok(sent) => sent,
            Err(err) => {
                // Falling back rather than failing: the Platform API below did
                // this job before the frames were reverse-engineered.
                log::warn!("Setting segments for {device} over IoT failed: {err:#}");
                false
            }
        };

        // Bluetooth carries the same frames, and for a segmented device that
        // has no cloud path it is the only thing that can. Tried after IoT
        // because a radio session is slower and holds a proxy connection slot.
        let sent = sent
            || match self.send_via_ble(state, &device, pending).await {
                Ok(sent) => sent,
                Err(err) => {
                    log::warn!("Setting segments for {device} over Bluetooth failed: {err:#}");
                    false
                }
            };

        if sent {
            return Ok(());
        }

        let client = state.get_platform_client().await.ok_or_else(|| {
            anyhow::anyhow!("set segments for {device}: Platform API unavailable")
        })?;
        let info = device
            .http_device_info
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("set segments for {device}: no HTTP device info"))?;

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

        Ok(())
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

    /// Send the batch's colours as raw frames over AWS IoT.
    ///
    /// Returns whether it carried them. Every colour rides in one `ptReal`
    /// message: the frames are independent, each naming its own segments, and a
    /// device applies several from one message — measured while restoring a
    /// lamp after the reverse-engineering session.
    ///
    /// Brightness is deliberately not attempted. The read frames report it and
    /// the Govee app sets it, but the command that does so is not known: three
    /// probes -- brightness in byte 7, a `02` sub-command, brightness after the
    /// mask -- all did nothing or something else. So it stays on the Platform
    /// API.
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

        let mut frames = vec![];
        for raw in Self::encode(pending)? {
            frames.extend(Base64HexBytes::with_bytes(raw).base64());
        }

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
