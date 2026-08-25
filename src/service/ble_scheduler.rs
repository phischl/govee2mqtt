//! Scheduling for the BLE transport.
//!
//! This is the piece that has to earn its keep. Home Assistant provides no BLE
//! queue of its own, and an ESP32 proxy has around three connection slots, so
//! a burst of commands that turns into a burst of connections is exactly how
//! Bluetooth control becomes unreliable.
//!
//! Three mechanisms, in the order they matter:
//!
//! 1. **Coalescing.** Commands for one device inside a short window merge into a
//!    single session. "On, 80%, warm white" arrives as three MQTT messages and
//!    leaves as one connection carrying three frames.
//! 2. **A concurrency gate**, defaulting to one session at a time, so we never
//!    contend with ourselves for connection slots.
//! 3. **A circuit breaker**, so a device that is out of range stops being
//!    retried and the router falls through to the cloud instead.

use crate::ble::{
    decode_notification, query_device_brightness, query_device_color, query_device_power,
    Base64HexBytes, GoveeBlePacket, Kelvin, SetDeviceBrightness, SetDeviceColorRgb,
    SetDeviceColorTemperature, SetDevicePower, GENERIC_LIGHT,
};
use crate::lan_api::DeviceColor;
use crate::service::ble_bridge::{
    BleBridge, ErrorKind, JobOp, JobRequest, JobResult, QuerySpec, WriteSpec,
};
use crate::service::device::Device;
use crate::service::hass::topic_safe_id;
use crate::service::state::StateHandle;
use crate::service::transport::DeviceOp;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{oneshot, Mutex, Semaphore};

/// Govee's GATT characteristics. Identical across every model seen so far.
pub const GOVEE_WRITE_CHAR: &str = "00010203-0405-0607-0809-0a0b0c0d2b11";
pub const GOVEE_NOTIFY_CHAR: &str = "00010203-0405-0607-0809-0a0b0c0d2b10";

/// Devices the user wants kept off Bluetooth even though it is enabled.
///
/// Each entry is matched against the same identifiers `resolve_device` accepts —
/// device id, name, computed name, MQTT topic id — plus the SKU, so a single
/// entry can exclude one light or a whole model.
#[derive(Clone, Debug, Default)]
pub struct BleExclusions {
    entries: Vec<String>,
}

impl BleExclusions {
    /// Parse a comma separated spec. Blank entries are ignored, so a trailing
    /// comma or a stray space is harmless.
    pub fn parse(spec: &str) -> Self {
        Self {
            entries: spec
                .split(',')
                .map(|entry| entry.trim().to_ascii_lowercase())
                .filter(|entry| !entry.is_empty())
                .collect(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn entries(&self) -> &[String] {
        &self.entries
    }

    /// Whether this entry names the device.
    pub fn entry_matches(entry: &str, device: &Device) -> bool {
        entry.eq_ignore_ascii_case(&device.id)
            || entry.eq_ignore_ascii_case(&device.sku)
            || entry.eq_ignore_ascii_case(&device.name())
            || entry.eq_ignore_ascii_case(&device.computed_name())
            || entry.eq_ignore_ascii_case(&topic_safe_id(device))
    }

    pub fn excludes(&self, device: &Device) -> bool {
        self.entries
            .iter()
            .any(|entry| Self::entry_matches(entry, device))
    }
}

#[derive(Clone, Debug)]
pub struct BleSchedulerConfig {
    /// How long to gather commands for one device before sending them.
    /// Long enough to catch Home Assistant's habit of sending power, brightness
    /// and colour as separate messages; short enough not to feel laggy.
    pub coalesce_window: Duration,
    /// Gap between frames within a session. Govee devices drop commands sent
    /// closer together than this.
    pub inter_frame_delay: Duration,
    /// Sessions in flight across all devices.
    pub max_concurrent: usize,
    /// How long the executor should hold the connection open afterwards.
    pub keep_open: Duration,
    /// The whole budget for a job, queue time and execution together. The
    /// executor enforces it and answers with a timeout rather than running on.
    pub deadline: Duration,
    /// Consecutive failures before BLE is disabled for a device.
    pub breaker_threshold: u32,
    /// How long BLE stays disabled for a device once the breaker opens.
    pub breaker_cooldown: Duration,
    /// How long to wait for a device to answer a status query.
    pub query_timeout: Duration,
    /// Devices to keep off Bluetooth while it stays enabled for everything else.
    pub exclusions: BleExclusions,
    /// Read back the attributes we just changed, in the same session.
    ///
    /// Costs no extra connection and catches a frame the device dropped, which
    /// matters because writes are unacknowledged.
    pub verify_writes: bool,
}

impl Default for BleSchedulerConfig {
    fn default() -> Self {
        Self {
            coalesce_window: Duration::from_millis(150),
            inter_frame_delay: Duration::from_millis(200),
            max_concurrent: 1,
            keep_open: Duration::from_secs(30),
            deadline: Duration::from_secs(30),
            breaker_threshold: 3,
            breaker_cooldown: Duration::from_secs(300),
            query_timeout: Duration::from_secs(5),
            exclusions: BleExclusions::default(),
            verify_writes: true,
        }
    }
}

/// The commands accumulated for one device, collapsed to a final state.
#[derive(Default, Clone, Debug, PartialEq, Eq)]
struct PendingOps {
    power: Option<bool>,
    brightness: Option<u8>,
    color: Option<(u8, u8, u8)>,
    kelvin: Option<u16>,
}

impl PendingOps {
    fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    fn merge(&mut self, op: &DeviceOp) -> anyhow::Result<()> {
        match op {
            DeviceOp::PowerOn(on) | DeviceOp::LightPowerOn(on) => self.power = Some(*on),
            DeviceOp::SetBrightness(percent) => self.brightness = Some(*percent),
            DeviceOp::SetColorRgb { r, g, b } => {
                self.color = Some((*r, *g, *b));
                // Colour and colour temperature are the same command with
                // different arguments; keeping both would send two conflicting
                // frames back to back.
                self.kelvin = None;
            }
            DeviceOp::SetColorTemperature(kelvin) => {
                let kelvin = u16::try_from(*kelvin)
                    .map_err(|_| anyhow::anyhow!("{kelvin}K is out of range"))?;
                self.kelvin = Some(kelvin);
                self.color = None;
            }
            other => anyhow::bail!("the BLE transport cannot {}", other.describe()),
        }
        Ok(())
    }

    /// The attributes worth reading back after this session.
    ///
    /// Only what we actually changed: verifying everything would triple the
    /// length of a session that set one attribute.
    fn verification_queries(&self) -> Vec<Query> {
        if self.power == Some(false) {
            // Nothing else was sent, so nothing else is worth asking about.
            return vec![Query::Power];
        }

        let mut queries = vec![];
        if self.power.is_some() {
            queries.push(Query::Power);
        }
        if self.brightness.is_some() {
            queries.push(Query::Brightness);
        }
        if self.color.is_some() || self.kelvin.is_some() {
            queries.push(Query::Color);
        }
        queries
    }

    /// Render to wire frames, in the order the device expects them.
    fn frames(&self) -> anyhow::Result<Vec<Vec<u8>>> {
        let mut frames = vec![];

        if let Some(on) = self.power {
            frames.push(encode(&SetDevicePower { on })?);
            if !on {
                // Nothing else is worth sending to a device we just switched off.
                return Ok(frames);
            }
        }

        if let Some(percent) = self.brightness {
            // Zero is "off" rather than "as dim as possible", and a brightness
            // command that silently switches the light off would be a surprise.
            frames.push(encode(&SetDeviceBrightness {
                percent: percent.max(1),
            })?);
        }

        if let Some((r, g, b)) = self.color {
            frames.push(encode(&SetDeviceColorRgb { r, g, b })?);
        } else if let Some(kelvin) = self.kelvin {
            frames.push(encode(&SetDeviceColorTemperature {
                kelvin: Kelvin::new(kelvin)?,
            })?);
        }

        Ok(frames)
    }
}

/// A status attribute we can ask a device about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Query {
    Power,
    Brightness,
    Color,
}

impl Query {
    const ALL: [Query; 3] = [Query::Power, Query::Brightness, Query::Color];

    fn frame(&self) -> anyhow::Result<Vec<u8>> {
        let encoded = match self {
            Self::Power => query_device_power(),
            Self::Brightness => query_device_brightness(),
            Self::Color => query_device_color(),
        };
        let mut chunks = encoded.base64();
        anyhow::ensure!(chunks.len() == 1, "a query should be a single frame");
        Ok(chunks.remove(0).into_bytes())
    }
}

fn encode<T: 'static>(value: &T) -> anyhow::Result<Vec<u8>> {
    let encoded = Base64HexBytes::encode_for_sku(GENERIC_LIGHT, value)?;
    let mut chunks = encoded.base64();
    anyhow::ensure!(
        chunks.len() == 1,
        "expected a single 20 byte frame, got {} chunks",
        chunks.len()
    );
    Ok(chunks.remove(0).into_bytes())
}

type Waiter = oneshot::Sender<Result<(), String>>;

#[derive(Default)]
struct DeviceQueue {
    address: String,
    pending: PendingOps,
    waiters: Vec<Waiter>,
    flush_scheduled: bool,
}

#[derive(Default, Debug)]
struct Breaker {
    consecutive_failures: u32,
    open_until: Option<Instant>,
}

impl Breaker {
    fn is_open(&self, now: Instant) -> bool {
        self.open_until.is_some_and(|until| now < until)
    }
}

pub struct BleScheduler {
    config: BleSchedulerConfig,
    bridge: Arc<BleBridge>,
    gate: Arc<Semaphore>,
    devices: Mutex<HashMap<String, DeviceQueue>>,
    breakers: Mutex<HashMap<String, Breaker>>,
}

impl BleScheduler {
    pub fn new(bridge: Arc<BleBridge>, config: BleSchedulerConfig) -> Self {
        Self {
            gate: Arc::new(Semaphore::new(config.max_concurrent.max(1))),
            config,
            bridge,
            devices: Mutex::new(HashMap::new()),
            breakers: Mutex::new(HashMap::new()),
        }
    }

    pub fn bridge(&self) -> &Arc<BleBridge> {
        &self.bridge
    }

    /// Whether BLE is currently worth attempting for a device.
    pub async fn is_available_for(&self, device: &Device) -> bool {
        if self.config.exclusions.excludes(device) {
            return false;
        }
        if !self.bridge.is_online() {
            return false;
        }
        let breakers = self.breakers.lock().await;
        !breakers
            .get(&device.id)
            .is_some_and(|breaker| breaker.is_open(Instant::now()))
    }

    /// Queue an operation and wait until its session completes.
    pub async fn apply(
        self: &Arc<Self>,
        state: &StateHandle,
        device: &Device,
        address: &str,
        op: &DeviceOp,
    ) -> anyhow::Result<()> {
        let (tx, rx) = oneshot::channel();
        let device_id = device.id.to_string();

        {
            let mut devices = self.devices.lock().await;
            let queue = devices.entry(device_id.clone()).or_default();
            queue.address = address.to_string();
            queue.pending.merge(op)?;
            queue.waiters.push(tx);

            if !queue.flush_scheduled {
                queue.flush_scheduled = true;
                let scheduler = self.clone();
                let state = state.clone();
                let sku = device.sku.to_string();
                let id = device_id.clone();
                tokio::spawn(async move {
                    scheduler.flush_after_window(state, id, sku).await;
                });
            }
        }

        rx.await
            .map_err(|_| anyhow::anyhow!("BLE session for {device} was dropped"))?
            .map_err(|err| anyhow::anyhow!("{err}"))
    }

    async fn flush_after_window(
        self: Arc<Self>,
        state: StateHandle,
        device_id: String,
        sku: String,
    ) {
        tokio::time::sleep(self.config.coalesce_window).await;

        let (address, pending, waiters) = {
            let mut devices = self.devices.lock().await;
            let Some(queue) = devices.get_mut(&device_id) else {
                return;
            };
            queue.flush_scheduled = false;
            if queue.pending.is_empty() {
                return;
            }
            (
                queue.address.clone(),
                std::mem::take(&mut queue.pending),
                std::mem::take(&mut queue.waiters),
            )
        };

        let result = self
            .run_session(&state, &device_id, &sku, &address, &pending)
            .await;

        match &result {
            Ok(()) => self.note_success(&device_id).await,
            Err(err) => self.note_failure(&device_id, &err.to_string()).await,
        }

        let outcome = result.map_err(|err| err.to_string());
        for waiter in waiters {
            let _ = waiter.send(outcome.clone());
        }
    }

    async fn run_session(
        &self,
        state: &StateHandle,
        device_id: &str,
        sku: &str,
        address: &str,
        pending: &PendingOps,
    ) -> anyhow::Result<()> {
        let frames = pending.frames()?;
        anyhow::ensure!(!frames.is_empty(), "nothing to send");

        let queries = if self.config.verify_writes {
            pending.verification_queries()
        } else {
            vec![]
        };

        self.exchange(state, device_id, sku, address, &frames, &queries, "user")
            .await
    }

    /// Read a device's current state without changing anything.
    pub async fn poll(
        &self,
        state: &StateHandle,
        device_id: &str,
        sku: &str,
        address: &str,
    ) -> anyhow::Result<()> {
        let result = self
            .exchange(state, device_id, sku, address, &[], &Query::ALL, "poll")
            .await;

        match &result {
            Ok(()) => self.note_success(device_id).await,
            Err(err) => self.note_failure(device_id, &err.to_string()).await,
        }
        result
    }

    /// Build one session, hand it to the executor, and apply whatever came back.
    #[allow(clippy::too_many_arguments)]
    async fn exchange(
        &self,
        state: &StateHandle,
        device_id: &str,
        sku: &str,
        address: &str,
        frames: &[Vec<u8>],
        queries: &[Query],
        priority: &'static str,
    ) -> anyhow::Result<()> {
        let mut ops = Vec::with_capacity((frames.len() + queries.len()) * 2);

        for frame in frames {
            if !ops.is_empty() {
                ops.push(JobOp::Delay(
                    self.config.inter_frame_delay.as_millis() as u64
                ));
            }
            ops.push(JobOp::Write(WriteSpec {
                char: GOVEE_WRITE_CHAR,
                data: String::from_utf8(frame.clone())?,
                response: false,
            }));
        }

        for query in queries {
            if !ops.is_empty() {
                ops.push(JobOp::Delay(
                    self.config.inter_frame_delay.as_millis() as u64
                ));
            }
            ops.push(JobOp::Query(QuerySpec {
                write_char: GOVEE_WRITE_CHAR,
                notify_char: GOVEE_NOTIFY_CHAR,
                data: String::from_utf8(query.frame()?)?,
                timeout_ms: self.config.query_timeout.as_millis() as u64,
            }));
        }
        anyhow::ensure!(!ops.is_empty(), "nothing to do");

        let job = JobRequest {
            id: uuid::Uuid::new_v4().to_string(),
            address: address.to_uppercase(),
            priority,
            keep_open_ms: self.config.keep_open.as_millis() as u64,
            deadline_ms: self.config.deadline.as_millis() as u64,
            ops,
        };
        let job_id = job.id.clone();

        // The executor bounds the job to `deadline` and answers either way, so
        // our own timeout only needs enough margin to distinguish "the executor
        // is gone" from "the executor is about to answer".
        let timeout = self.config.deadline + Duration::from_secs(15);

        // Held for the whole exchange: releasing on publish would let the next
        // session start while this one still owns a connection slot.
        let _permit = self.gate.acquire().await?;

        log::debug!(
            "BLE job {job_id}: {} frame(s), {} query/queries to {sku} {device_id} at {address}",
            frames.len(),
            queries.len()
        );

        let response = self.bridge.submit(state, job, timeout).await?;
        if !response.ok {
            let error = response
                .error
                .ok_or_else(|| anyhow::anyhow!("BLE job {job_id} failed without saying why"))?;
            anyhow::bail!("{:?}: {}", error.kind, error.message);
        }

        log::info!(
            "Using BLE to reach {sku} {device_id} ({} frame(s), {} query/queries, {}ms)",
            frames.len(),
            queries.len(),
            response.duration_ms
        );

        self.apply_notifications(state, sku, device_id, &response.results)
            .await;
        Ok(())
    }

    /// Fold status notifications into the device's state.
    ///
    /// Notifications identify themselves, so they are matched by content rather
    /// than by position: a device that answers out of order, or answers only
    /// some queries, still tells us something useful.
    async fn apply_notifications(
        &self,
        state: &StateHandle,
        sku: &str,
        device_id: &str,
        results: &[JobResult],
    ) {
        let mut changed = false;

        for result in results.iter().filter(|result| result.kind == "notify") {
            let Some(encoded) = &result.data else {
                continue;
            };
            let Ok(bytes) = data_encoding::BASE64.decode(encoded.as_bytes()) else {
                log::warn!("BLE notification from {device_id} was not valid base64");
                continue;
            };

            let packet = decode_notification(GENERIC_LIGHT, &bytes);
            let mut device = state.device_mut(sku, device_id).await;
            match packet {
                GoveeBlePacket::NotifyDevicePower(power) => {
                    changed |= device.update_ble_device_status(|status| status.on = power.on);
                }
                GoveeBlePacket::NotifyDeviceBrightness(brightness) => {
                    changed |= device.update_ble_device_status(|status| {
                        status.brightness = brightness.percent;
                    });
                }
                GoveeBlePacket::NotifyDeviceColor(color) => {
                    changed |= device.update_ble_device_status(|status| {
                        status.color = DeviceColor {
                            r: color.r,
                            g: color.g,
                            b: color.b,
                        };
                        // Zero is the device telling us it is showing an RGB
                        // colour rather than white.
                        status.color_temperature_kelvin =
                            color.kelvin.get().map(u32::from).unwrap_or(0);
                    });
                }
                other => {
                    log::debug!("unhandled BLE notification from {device_id}: {other:?}");
                }
            }
        }

        if !changed {
            return;
        }

        // The guard above must be out of scope before this: notifying Home
        // Assistant re-reads the device and would deadlock against it.
        if let Err(err) = state.notify_of_state_change(device_id).await {
            log::error!("failed to publish BLE state for {device_id}: {err:#}");
        }
    }

    async fn note_success(&self, device_id: &str) {
        let mut breakers = self.breakers.lock().await;
        if let Some(breaker) = breakers.get_mut(device_id) {
            if breaker.consecutive_failures > 0 || breaker.open_until.is_some() {
                log::info!("BLE recovered for {device_id}");
            }
            *breaker = Breaker::default();
        }
    }

    async fn note_failure(&self, device_id: &str, reason: &str) {
        let mut breakers = self.breakers.lock().await;
        let breaker = breakers.entry(device_id.to_string()).or_default();
        breaker.consecutive_failures += 1;

        if breaker.consecutive_failures >= self.config.breaker_threshold {
            // Only announce the transition. A device that stays unreachable
            // reopens the breaker every cooldown, and repeating the warning
            // forever would be noise rather than information.
            let was_open = breaker.open_until.is_some();
            breaker.open_until = Some(Instant::now() + self.config.breaker_cooldown);
            if !was_open {
                log::warn!(
                    "BLE failed {} times for {device_id} ({reason}); \
                     setting it aside for {:?}",
                    breaker.consecutive_failures,
                    self.config.breaker_cooldown
                );
            } else {
                log::debug!("BLE still unreachable for {device_id}: {reason}");
            }
        } else {
            log::debug!(
                "BLE attempt {} for {device_id} failed: {reason}",
                breaker.consecutive_failures
            );
        }
    }

    /// Note a failure reported by the executor, honouring its retry hint.
    #[allow(dead_code)]
    pub fn retry_delay(kind: ErrorKind, retry_after_ms: Option<u64>) -> Option<Duration> {
        if !kind.is_retryable() {
            return None;
        }
        Some(Duration::from_millis(retry_after_ms.unwrap_or(1000)))
    }
}

#[cfg(test)]
mod test {
    use super::*;

    fn frames_hex(pending: &PendingOps) -> Vec<String> {
        pending
            .frames()
            .unwrap()
            .into_iter()
            .map(|frame| {
                let raw = data_encoding::BASE64
                    .decode(&frame)
                    .expect("frames are base64");
                raw.iter().map(|b| format!("{b:02x}")).collect()
            })
            .collect()
    }

    #[test]
    fn commands_collapse_into_one_session() {
        let mut pending = PendingOps::default();
        pending.merge(&DeviceOp::LightPowerOn(true)).unwrap();
        pending.merge(&DeviceOp::SetBrightness(50)).unwrap();
        pending.merge(&DeviceOp::SetColorTemperature(2700)).unwrap();

        assert_eq!(
            frames_hex(&pending),
            vec![
                "3301010000000000000000000000000000000033",
                "3304320000000000000000000000000000000005",
                "33050dffffff0a8c000000000000000000000042",
            ]
        );
    }

    #[test]
    fn the_last_value_for_an_attribute_wins() {
        let mut pending = PendingOps::default();
        pending.merge(&DeviceOp::SetBrightness(10)).unwrap();
        pending.merge(&DeviceOp::SetBrightness(90)).unwrap();

        assert_eq!(pending.brightness, Some(90));
        assert_eq!(frames_hex(&pending).len(), 1);
    }

    #[test]
    fn colour_and_colour_temperature_displace_each_other() {
        let mut pending = PendingOps::default();
        pending.merge(&DeviceOp::SetColorTemperature(4000)).unwrap();
        pending
            .merge(&DeviceOp::SetColorRgb { r: 1, g: 2, b: 3 })
            .unwrap();

        assert_eq!(pending.kelvin, None);
        assert_eq!(
            frames_hex(&pending),
            vec!["33050d010203000000000000000000000000003b"]
        );
    }

    #[test]
    fn switching_off_sends_nothing_else() {
        let mut pending = PendingOps::default();
        pending.merge(&DeviceOp::SetBrightness(80)).unwrap();
        pending
            .merge(&DeviceOp::SetColorRgb { r: 1, g: 2, b: 3 })
            .unwrap();
        pending.merge(&DeviceOp::PowerOn(false)).unwrap();

        assert_eq!(
            frames_hex(&pending),
            vec!["3301000000000000000000000000000000000032"]
        );
    }

    #[test]
    fn zero_brightness_does_not_become_an_off_command() {
        let mut pending = PendingOps::default();
        pending.merge(&DeviceOp::SetBrightness(0)).unwrap();

        assert_eq!(
            frames_hex(&pending),
            vec!["3304010000000000000000000000000000000036"]
        );
    }

    #[test]
    fn unsupported_operations_are_rejected() {
        let mut pending = PendingOps::default();
        assert!(pending
            .merge(&DeviceOp::SetScene("Sunrise".to_string()))
            .is_err());
    }

    #[test]
    fn only_the_attributes_we_changed_are_read_back() {
        let mut pending = PendingOps::default();
        pending.merge(&DeviceOp::SetBrightness(50)).unwrap();

        assert_eq!(pending.verification_queries(), vec![Query::Brightness]);
    }

    #[test]
    fn colour_and_colour_temperature_share_one_query() {
        let mut pending = PendingOps::default();
        pending.merge(&DeviceOp::LightPowerOn(true)).unwrap();
        pending.merge(&DeviceOp::SetColorTemperature(3000)).unwrap();

        assert_eq!(
            pending.verification_queries(),
            vec![Query::Power, Query::Color]
        );
    }

    #[test]
    fn switching_off_only_reads_back_the_power_state() {
        let mut pending = PendingOps::default();
        pending.merge(&DeviceOp::SetBrightness(80)).unwrap();
        pending.merge(&DeviceOp::PowerOn(false)).unwrap();

        assert_eq!(pending.verification_queries(), vec![Query::Power]);
    }

    #[test]
    fn queries_are_single_frames() {
        for query in Query::ALL {
            let frame = query.frame().unwrap();
            let raw = data_encoding::BASE64.decode(&frame).unwrap();
            assert_eq!(raw.len(), 20, "{query:?}");
        }
    }

    fn device(sku: &str, id: &str) -> Device {
        Device::new(sku, id)
    }

    #[test]
    fn an_empty_exclusion_list_excludes_nothing() {
        let exclusions = BleExclusions::parse("");
        assert!(exclusions.is_empty());
        assert!(!exclusions.excludes(&device("H601B", "AA:BB:CC:DD:EE:FF:11:22")));
    }

    #[test]
    fn a_device_can_be_excluded_by_id() {
        let exclusions = BleExclusions::parse("AA:BB:CC:DD:EE:FF:11:22");
        assert!(exclusions.excludes(&device("H601B", "AA:BB:CC:DD:EE:FF:11:22")));
        assert!(!exclusions.excludes(&device("H601B", "AA:BB:CC:DD:EE:FF:11:23")));
    }

    #[test]
    fn a_whole_model_can_be_excluded_by_sku() {
        let exclusions = BleExclusions::parse("H601B");
        assert!(exclusions.excludes(&device("H601B", "AA:BB:CC:DD:EE:FF:11:22")));
        assert!(!exclusions.excludes(&device("H6127", "AA:BB:CC:DD:EE:FF:11:22")));
    }

    #[test]
    fn matching_ignores_case() {
        let exclusions = BleExclusions::parse("h601b");
        assert!(exclusions.excludes(&device("H601B", "AA:BB:CC:DD:EE:FF:11:22")));
    }

    #[test]
    fn the_computed_name_is_matched_too() {
        // What the user sees in Home Assistant before renaming.
        let dev = device("H601B", "AA:BB:CC:DD:EE:FF:11:22");
        let exclusions = BleExclusions::parse(&dev.computed_name());
        assert!(exclusions.excludes(&dev));
    }

    #[test]
    fn blank_entries_are_ignored() {
        // A trailing comma or a stray space must not exclude everything.
        let exclusions = BleExclusions::parse(" H601B , ,");
        assert_eq!(exclusions.entries(), ["h601b"]);
        assert!(!exclusions.excludes(&device("H6127", "AA:BB:CC:DD:EE:FF:11:22")));
    }

    #[test]
    fn a_breaker_opens_only_after_repeated_failures() {
        let mut breaker = Breaker::default();
        let now = Instant::now();
        assert!(!breaker.is_open(now));

        breaker.consecutive_failures = 3;
        breaker.open_until = Some(now + Duration::from_secs(300));
        assert!(breaker.is_open(now));
        assert!(!breaker.is_open(now + Duration::from_secs(301)));
    }
}
