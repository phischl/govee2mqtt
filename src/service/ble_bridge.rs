//! Client for the `govee_ble_executor` Home Assistant integration.
//!
//! The add-on has no Bluetooth stack of its own, and it cannot talk to the
//! ESPHome proxies directly either: a proxy accepts exactly one advertisement
//! subscriber, so we would spend our time stealing the subscription from Home
//! Assistant. Instead a companion integration executes GATT operations inside
//! Home Assistant and we exchange jobs with it over the MQTT connection we
//! already hold.
//!
//! The wire format is specified in `component/README.md`.

use crate::service::state::StateHandle;
use arc_swap::ArcSwap;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{oneshot, Mutex};

pub const DEFAULT_TOPIC_PREFIX: &str = "gv2mqtt/ble";

#[derive(clap::Parser, Debug, Default)]
pub struct BleArguments {
    /// MQTT topic prefix shared with the govee_ble_executor Home Assistant
    /// integration. Must match the value configured there.
    /// You may also set this via the GOVEE_BLE_TOPIC_PREFIX environment variable.
    #[arg(long, global = true)]
    ble_topic_prefix: Option<String>,

    /// Disable the BLE transport entirely, even if the executor is online.
    /// You may also set this via the GOVEE_BLE_DISABLE environment variable.
    #[arg(long, global = true)]
    no_ble: bool,

    /// How many Bluetooth sessions may run at once, across all devices.
    /// One is the safest default; raising it speeds up scenes that touch
    /// several lights, at the cost of holding more proxy connection slots.
    /// You may also set this via the GOVEE_BLE_MAX_CONCURRENT environment variable.
    #[arg(long, global = true)]
    ble_max_concurrent: Option<usize>,

    /// Correct the Bluetooth address for specific devices, as a comma separated
    /// list of `device-id=AA:BB:CC:DD:EE:FF` pairs. Only needed when Govee's
    /// metadata reports an address the device does not answer on.
    /// You may also set this via the GOVEE_BLE_ADDRESS_MAP environment variable.
    #[arg(long, global = true)]
    ble_address_map: Option<String>,

    /// Keep individual devices off Bluetooth while leaving it enabled for
    /// everything else. Comma separated; each entry matches a device id, SKU or
    /// name, so `H601B` excludes a whole model and
    /// `15:25:60:74:F4:2B:2E:A4` a single light.
    /// You may also set this via the GOVEE_BLE_EXCLUDE environment variable.
    #[arg(long, global = true)]
    ble_exclude: Option<String>,
}

impl BleArguments {
    pub fn topic_prefix(&self) -> anyhow::Result<String> {
        Ok(match &self.ble_topic_prefix {
            Some(prefix) => prefix.clone(),
            None => crate::opt_env_var("GOVEE_BLE_TOPIC_PREFIX")?
                .unwrap_or_else(|| DEFAULT_TOPIC_PREFIX.to_string()),
        })
    }

    pub fn max_concurrent(&self) -> anyhow::Result<Option<usize>> {
        Ok(match self.ble_max_concurrent {
            Some(value) => Some(value),
            None => crate::opt_env_var::<usize>("GOVEE_BLE_MAX_CONCURRENT")?,
        })
    }

    pub fn address_map(&self) -> anyhow::Result<Option<String>> {
        Ok(match &self.ble_address_map {
            Some(spec) => Some(spec.clone()),
            None => crate::opt_env_var::<String>("GOVEE_BLE_ADDRESS_MAP")?,
        })
    }

    /// Raw exclusion spec, from the flag or the environment.
    pub fn exclude_spec(&self) -> anyhow::Result<Option<String>> {
        Ok(match &self.ble_exclude {
            Some(spec) => Some(spec.clone()),
            None => crate::opt_env_var::<String>("GOVEE_BLE_EXCLUDE")?,
        })
    }

    pub fn is_disabled(&self) -> anyhow::Result<bool> {
        if self.no_ble {
            return Ok(true);
        }
        Ok(crate::opt_env_var::<bool>("GOVEE_BLE_DISABLE")?.unwrap_or(false))
    }
}

// ---------------------------------------------------------------- wire format

#[derive(Serialize, Debug)]
pub struct JobRequest {
    pub id: String,
    pub address: String,
    pub priority: &'static str,
    pub keep_open_ms: u64,
    pub deadline_ms: u64,
    pub ops: Vec<JobOp>,
}

/// Serde's external tagging gives us exactly the shapes the executor expects:
/// `{"write": {...}}`, `{"delay_ms": 200}`, `{"query": {...}}`.
///
/// The whole wire format is modelled here even where the add-on does not use it
/// yet, so that the contract lives in one place and the executor cannot drift
/// away from it unnoticed. Query ops arrive with BLE status reads.
#[derive(Serialize, Debug)]
#[allow(dead_code)]
pub enum JobOp {
    #[serde(rename = "write")]
    Write(WriteSpec),
    #[serde(rename = "delay_ms")]
    Delay(u64),
    #[serde(rename = "query")]
    Query(QuerySpec),
}

#[derive(Serialize, Debug)]
pub struct WriteSpec {
    pub char: &'static str,
    pub data: String,
    pub response: bool,
}

#[derive(Serialize, Debug)]
#[allow(dead_code)]
pub struct QuerySpec {
    pub write_char: &'static str,
    pub notify_char: &'static str,
    pub data: String,
    pub timeout_ms: u64,
    /// Whether silence is an acceptable answer.
    ///
    /// A speculative question — "do you have segments?" — is answered by a
    /// device that does and ignored by one that does not. Without this the
    /// silence failed the whole job and the circuit breaker took a working
    /// Bluetooth-only light out of service on every poll.
    ///
    /// Skipped when false so that an executor predating this field sees
    /// exactly what it saw before.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub optional: bool,
}

#[derive(Deserialize, Debug, Clone)]
#[allow(dead_code)]
pub struct JobResponse {
    pub id: String,
    pub ok: bool,
    #[serde(default)]
    pub results: Vec<JobResult>,
    #[serde(default)]
    pub duration_ms: u64,
    #[serde(default)]
    pub error: Option<JobError>,
}

#[derive(Deserialize, Debug, Clone)]
#[allow(dead_code)]
pub struct JobResult {
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub data: Option<String>,
}

#[derive(Deserialize, Debug, Clone)]
#[allow(dead_code)]
pub struct JobError {
    pub kind: ErrorKind,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub retry_after_ms: Option<u64>,
}

/// Why a job failed. The scheduler branches on this, so an unrecognised value
/// must not be fatal.
#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    BadRequest,
    NotFound,
    OutOfSlots,
    ConnectFailed,
    GattError,
    Timeout,
    Internal,
    #[serde(other)]
    Unknown,
}

impl ErrorKind {
    /// Whether trying the same device again shortly is worthwhile. A malformed
    /// request is our own bug and will fail identically next time.
    pub fn is_retryable(&self) -> bool {
        !matches!(self, Self::BadRequest)
    }
}

#[derive(Deserialize, Debug, Clone, Default)]
#[allow(dead_code)]
pub struct ProxySlots {
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub slots: u32,
    #[serde(default)]
    pub free: u32,
    #[serde(default)]
    pub allocated: Vec<String>,
}

#[derive(Deserialize, Debug, Clone, Default)]
#[allow(dead_code)]
pub struct BridgeStatus {
    #[serde(default)]
    pub online: bool,
    #[serde(default)]
    pub max_concurrent: usize,
    #[serde(default)]
    pub queue_depth: usize,
    #[serde(default)]
    pub proxies: Vec<ProxySlots>,
}

impl BridgeStatus {
    /// Total free connection slots across every proxy that has reported its
    /// capacity. Proxies reporting `slots == 0` have not told us anything yet,
    /// and the executor already filters those out.
    pub fn free_slots(&self) -> u32 {
        self.proxies.iter().map(|proxy| proxy.free).sum()
    }
}

// ---------------------------------------------------------------- bridge

pub struct BleBridge {
    prefix: String,
    pending: Mutex<HashMap<String, oneshot::Sender<JobResponse>>>,
    status: ArcSwap<BridgeStatus>,
}

impl BleBridge {
    pub fn new(prefix: String) -> Self {
        Self {
            prefix: prefix.trim_end_matches('/').to_string(),
            pending: Mutex::new(HashMap::new()),
            status: ArcSwap::from_pointee(BridgeStatus::default()),
        }
    }

    pub fn request_topic(&self) -> String {
        format!("{}/req", self.prefix)
    }

    pub fn response_topic(&self) -> String {
        format!("{}/res", self.prefix)
    }

    pub fn status_topic(&self) -> String {
        format!("{}/status", self.prefix)
    }

    /// Full status, for diagnostics.
    #[allow(dead_code)]
    pub fn status(&self) -> Arc<BridgeStatus> {
        self.status.load_full()
    }

    pub fn is_online(&self) -> bool {
        self.status.load().online
    }

    pub fn set_status(&self, status: BridgeStatus) {
        let was_online = self.status.load().online;
        if was_online != status.online {
            log::info!(
                "BLE executor is {}",
                if status.online { "online" } else { "offline" }
            );
        }
        self.status.store(Arc::new(status));
    }

    /// Mark the executor offline without waiting for it to say so, e.g. when our
    /// own MQTT connection drops and we can no longer trust the retained status.
    pub fn mark_offline(&self) {
        let mut status = (**self.status.load()).clone();
        status.online = false;
        self.set_status(status);
    }

    /// Route a response to whoever is waiting for it.
    pub async fn handle_response(&self, response: JobResponse) {
        match self.pending.lock().await.remove(&response.id) {
            Some(waiter) => {
                let _ = waiter.send(response);
            }
            None => {
                // Late arrival after our own timeout, or a reply to a job from a
                // previous run of the add-on. Nothing to do but note it.
                log::debug!("no waiter for BLE job {}", response.id);
            }
        }
    }

    /// Publish a job and wait for its response.
    pub async fn submit(
        &self,
        state: &StateHandle,
        job: JobRequest,
        timeout: Duration,
    ) -> anyhow::Result<JobResponse> {
        let hass = state
            .get_hass_client()
            .await
            .ok_or_else(|| anyhow::anyhow!("MQTT client is not available"))?;

        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(job.id.clone(), tx);
        let job_id = job.id.clone();

        if let Err(err) = hass.publish_obj(self.request_topic(), &job).await {
            self.pending.lock().await.remove(&job_id);
            return Err(err.context("publishing BLE job"));
        }

        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(_)) => {
                anyhow::bail!("BLE job {job_id} was cancelled before it completed")
            }
            Err(_) => {
                self.pending.lock().await.remove(&job_id);
                anyhow::bail!(
                    "BLE executor did not answer job {job_id} within {:?}. \
                     Is the govee_ble_executor integration running?",
                    timeout
                )
            }
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn job_ops_serialize_to_the_shapes_the_executor_expects() {
        let ops = vec![
            JobOp::Write(WriteSpec {
                char: "write-char",
                data: "AQI=".to_string(),
                response: false,
            }),
            JobOp::Delay(200),
            JobOp::Query(QuerySpec {
                write_char: "write-char",
                notify_char: "notify-char",
                data: "qgE=".to_string(),
                timeout_ms: 5000,
                optional: false,
            }),
        ];

        k9::snapshot!(
            serde_json::to_string_pretty(&ops).unwrap(),
            r#"
[
  {
    "write": {
      "char": "write-char",
      "data": "AQI=",
      "response": false
    }
  },
  {
    "delay_ms": 200
  },
  {
    "query": {
      "write_char": "write-char",
      "notify_char": "notify-char",
      "data": "qgE=",
      "timeout_ms": 5000
    }
  }
]
"#
        );
    }

    #[test]
    fn an_unknown_error_kind_does_not_break_parsing() {
        let response: JobResponse = serde_json::from_str(
            r#"{"id":"x","ok":false,"error":{"kind":"something_new","message":"?"}}"#,
        )
        .unwrap();
        assert_eq!(response.error.unwrap().kind, ErrorKind::Unknown);
    }

    #[test]
    fn free_slots_sums_across_proxies() {
        let status: BridgeStatus = serde_json::from_str(
            r#"{"online":true,"proxies":[
                 {"source":"a","slots":3,"free":2,"allocated":[]},
                 {"source":"b","slots":3,"free":1,"allocated":[]}]}"#,
        )
        .unwrap();
        assert_eq!(status.free_slots(), 3);
    }
}
