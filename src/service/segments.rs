//! Batching for per-segment commands.
//!
//! Govee's `segmentedColorRgb` and `segmentedBrightness` capabilities take an
//! *array* of segment indices, but Home Assistant models each segment as its
//! own light entity and so sends one command per segment. Left alone that
//! turns a scene over a fifteen-segment device into fifteen cloud requests.
//!
//! This collects the commands that arrive together, groups them by the value
//! they ask for, and issues one request per distinct value.

use crate::ble::{Base64HexBytes, SetSegmentColorRgb, GENERIC_LIGHT};
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

    /// What is left for the Platform API once AWS IoT has taken the colours.
    fn platform_only(&self, colour_sent: bool) -> (usize, usize) {
        if colour_sent {
            (
                self.brightness.len(),
                self.brightness.values().map(Vec::len).sum(),
            )
        } else {
            (
                self.request_count(),
                self.brightness.values().map(Vec::len).sum::<usize>()
                    + self.rgb.values().map(Vec::len).sum::<usize>(),
            )
        }
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

        for waiter in pending.waiters {
            let _ = waiter.send(outcome.clone());
        }
    }

    async fn send(
        &self,
        state: &StateHandle,
        device_id: &str,
        pending: &Pending,
    ) -> anyhow::Result<()> {
        let device = state
            .device_by_id(device_id)
            .await
            .ok_or_else(|| anyhow::anyhow!("device {device_id} went away"))?;

        // Colour goes over AWS IoT when the device is reachable that way: one
        // message carries every colour in the batch, and it spends no Platform
        // API quota — where fifteen segments otherwise cost fifteen requests.
        let colour_sent = match self.send_rgb_via_iot(state, &device, pending).await {
            Ok(sent) => sent,
            Err(err) => {
                // Falling back rather than failing: the Platform API below did
                // this job before the frame was reverse-engineered.
                log::warn!("Setting segments for {device} over IoT failed: {err:#}");
                false
            }
        };

        if pending.brightness.is_empty() && colour_sent {
            return Ok(());
        }

        let client = state.get_platform_client().await.ok_or_else(|| {
            anyhow::anyhow!("set segments for {device}: Platform API unavailable")
        })?;
        let info = device
            .http_device_info
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("set segments for {device}: no HTTP device info"))?;

        let (requests, segments) = pending.platform_only(colour_sent);
        log::info!(
            "Using Platform API to set {segments} segment change(s) on {device} \
             in {requests} request(s)"
        );

        for (percent, segments) in &pending.brightness {
            client
                .set_segment_brightness(info, segments, *percent)
                .await?;
        }
        if !colour_sent {
            for ((r, g, b), segments) in &pending.rgb {
                client.set_segment_rgb(info, segments, *r, *g, *b).await?;
            }
        }

        Ok(())
    }

    /// Send the batch's colours as raw frames over AWS IoT.
    ///
    /// Returns whether it carried them. Every colour rides in one `ptReal`
    /// message: the frames are independent, each naming its own segments, and a
    /// device applies several from one message — measured while restoring a
    /// lamp after the reverse-engineering session.
    ///
    /// Brightness is deliberately not attempted. The read frames report it and
    /// the Govee app sets it, but the command that does so is not known yet
    /// (CLAUDE.md §17), so it stays on the Platform API.
    async fn send_rgb_via_iot(
        &self,
        state: &StateHandle,
        device: &Device,
        pending: &Pending,
    ) -> anyhow::Result<bool> {
        if pending.rgb.is_empty() {
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
        for ((r, g, b), segments) in &pending.rgb {
            let command = SetSegmentColorRgb::for_segments(segments.iter().copied(), (*r, *g, *b))?;
            frames.extend(Base64HexBytes::encode_for_sku(GENERIC_LIGHT, &command)?.base64());
        }

        let count: usize = pending.rgb.values().map(Vec::len).sum();
        log::info!(
            "Using AWS IoT to set {count} segment colour(s) on {device} in one message, \
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

    /// Once AWS IoT has carried the colours, only brightness is left for the
    /// Platform API — which is the whole point of routing colour that way.
    #[test]
    fn iot_carrying_the_colours_leaves_only_brightness_for_the_cloud() {
        let mut pending = Pending::default();
        pending.add(0, Some(50), Some((255, 0, 0)));
        pending.add(1, None, Some((255, 0, 0)));
        pending.add(2, None, Some((0, 0, 255)));

        // Everything over the Platform API: two colours plus one brightness.
        assert_eq!(pending.platform_only(false), (3, 4));

        // Colours over IoT: one request for the single brightness change.
        assert_eq!(pending.platform_only(true), (1, 1));
    }

    #[test]
    fn one_segment_still_costs_one_request() {
        let mut pending = Pending::default();
        pending.add(3, None, Some((1, 2, 3)));

        assert_eq!(pending.request_count(), 1);
        assert_eq!(pending.rgb[&(1, 2, 3)], vec![3]);
    }
}
