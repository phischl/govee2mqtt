//! Transport-agnostic device operations.
//!
//! Upstream govee2mqtt repeated a hand-written if-cascade in every `State::device_*`
//! method to decide which API should carry a command. This module turns that into a
//! [`DeviceOp`] describing *what* should happen and a set of [`Transport`]
//! implementations describing *how*, so that a new transport (BLE) can be slotted in
//! without touching every operation.

pub mod ble;
pub mod iot;
pub mod lan;
pub mod nightlight;
pub mod platform;

use crate::service::device::Device;
use crate::service::state::StateHandle;
use crate::temperature::TemperatureValue;
use async_trait::async_trait;
use once_cell::sync::Lazy;
use std::sync::Arc;

/// Identifies a transport. The ordering of the variants is not meaningful;
/// priority is decided per operation by [`DeviceOp::default_transport_order`]
/// and may be overridden by configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, clap::ValueEnum)]
pub enum TransportId {
    /// Bluetooth LE, executed by the companion Home Assistant component.
    Ble,
    /// The nightlight sub-device of a humidifier, addressed via IoT `ptReal`.
    Nightlight,
    /// Govee's LAN UDP protocol.
    Lan,
    /// Govee's undocumented AWS IoT MQTT API.
    Iot,
    /// Govee's official Platform HTTP API.
    Platform,
}

impl std::fmt::Display for TransportId {
    fn fmt(&self, fmt: &mut std::fmt::Formatter) -> std::fmt::Result {
        let s = match self {
            Self::Ble => "BLE",
            Self::Nightlight => "Nightlight",
            Self::Lan => "LAN API",
            Self::Iot => "IoT API",
            Self::Platform => "Platform API",
        };
        fmt.write_str(s)
    }
}

#[derive(clap::Parser, Debug, Default)]
pub struct TransportArguments {
    /// Override which transports are preferred, as a comma separated list of
    /// `ble`, `nightlight`, `lan`, `iot`, `platform`.
    ///
    /// Acts as a priority prefix: the transports named here are tried first, in
    /// the order given, followed by whatever else the operation allows. It can
    /// reorder preferences but never enables a transport an operation does not
    /// support.
    #[arg(long, global = true, value_delimiter = ',')]
    pub transport_order: Vec<TransportId>,
}

/// A single, transport-agnostic thing to do to a device.
#[derive(Clone, Debug, PartialEq)]
pub enum DeviceOp {
    /// Turn the whole device on or off.
    PowerOn(bool),
    /// Turn only the light portion of a device on or off.
    LightPowerOn(bool),
    SetBrightness(u8),
    SetColorRgb {
        r: u8,
        g: u8,
        b: u8,
    },
    SetColorTemperature(u32),
    SetScene(String),
    SetHumidifierParameter {
        work_mode: i64,
        value: i64,
    },
    SetTargetTemperature {
        instance: String,
        target: TemperatureValue,
    },
}

impl DeviceOp {
    /// The transports to try, in order. These orderings intentionally reproduce
    /// upstream's per-operation preferences; `Ble` is prepended wherever it makes
    /// sense so that it wins once a BLE transport is registered.
    pub fn default_transport_order(&self) -> &'static [TransportId] {
        use TransportId::*;
        match self {
            Self::PowerOn(_) => &[Ble, Lan, Iot, Platform],
            Self::LightPowerOn(_) | Self::SetBrightness(_) | Self::SetColorRgb { .. } => {
                &[Nightlight, Ble, Lan, Iot, Platform]
            }
            Self::SetColorTemperature(_) => &[Ble, Lan, Iot, Platform],
            // Scenes prefer the Platform API because the LAN encoding is separate;
            // `PlatformTransport` declines when `avoid_platform_api()` is set.
            Self::SetScene(_) => &[Ble, Platform, Lan],
            Self::SetHumidifierParameter { .. } => &[Iot, Platform],
            Self::SetTargetTemperature { .. } => &[Platform],
        }
    }

    /// Human readable phrase used in log and error messages, e.g.
    /// "control brightness" or "set scene".
    pub fn describe(&self) -> String {
        match self {
            Self::PowerOn(_) => "control power state".to_string(),
            Self::LightPowerOn(_) => "control light power state".to_string(),
            Self::SetBrightness(_) => "control brightness".to_string(),
            Self::SetColorRgb { .. } => "control color".to_string(),
            Self::SetColorTemperature(_) => "control color temperature".to_string(),
            Self::SetScene(_) => "set scene".to_string(),
            Self::SetHumidifierParameter { work_mode, .. } => {
                format!("control humidifier parameter work_mode={work_mode}")
            }
            Self::SetTargetTemperature { .. } => "set temperature".to_string(),
        }
    }
}

/// Whether a transport took responsibility for an operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Handled {
    /// The transport carried out the operation.
    Yes,
    /// The transport cannot serve this device/operation; try the next one.
    NotSupported,
}

#[async_trait]
pub trait Transport: Send + Sync {
    fn id(&self) -> TransportId;

    /// Attempt the operation. Returning `Ok(Handled::NotSupported)` means
    /// "not my job" and lets the router continue with the next transport.
    async fn try_execute(
        &self,
        state: &StateHandle,
        device: &Device,
        op: &DeviceOp,
    ) -> anyhow::Result<Handled>;

    /// Carry out several operations as one unit.
    ///
    /// The default runs them in order, which is what the cloud and LAN APIs
    /// need anyway. The BLE transport overrides it to merge them into a single
    /// radio session: "on, 60%, warm white" becomes one connection carrying
    /// three frames instead of three connections.
    ///
    /// A transport that declines before doing anything lets the router move on.
    /// One that declines *after* something has already taken effect is an
    /// error, not a fallback: retrying elsewhere would apply the earlier
    /// operations twice.
    async fn try_execute_batch(
        &self,
        state: &StateHandle,
        device: &Device,
        ops: &[DeviceOp],
    ) -> anyhow::Result<Handled> {
        let mut applied = 0;
        for op in ops {
            match self.try_execute(state, device, op).await? {
                Handled::Yes => applied += 1,
                Handled::NotSupported if applied == 0 => return Ok(Handled::NotSupported),
                Handled::NotSupported => anyhow::bail!(
                    "{} applied {applied} of {} changes to {device} and then could not {}",
                    self.id(),
                    ops.len(),
                    op.describe()
                ),
            }
        }
        Ok(Handled::Yes)
    }

    /// Whether an error should fall through to the next transport rather than
    /// aborting. The cloud and LAN transports deliberately return `false`, which
    /// preserves upstream behaviour: a failing LAN command surfaces as an error
    /// instead of silently taking a slower path. BLE will opt in to `true`, since
    /// falling back to the cloud is exactly what we want when a radio link fails.
    fn fallback_on_error(&self) -> bool {
        false
    }
}

static TRANSPORTS: Lazy<Vec<Arc<dyn Transport>>> = Lazy::new(|| {
    vec![
        Arc::new(nightlight::NightlightTransport) as Arc<dyn Transport>,
        Arc::new(ble::BleTransport),
        Arc::new(lan::LanTransport),
        Arc::new(iot::IotTransport),
        Arc::new(platform::PlatformTransport),
    ]
});

fn transport_by_id(id: TransportId) -> Option<&'static Arc<dyn Transport>> {
    TRANSPORTS.iter().find(|t| t.id() == id)
}

/// Resolve the transport order for an operation, applying an optional
/// configured override.
///
/// The override acts as a priority prefix: transports named in it that are also
/// valid for this operation come first, in the configured order; any remaining
/// defaults follow in their original order. Transports that the operation does
/// not allow are never introduced by an override.
pub fn resolve_order(op: &DeviceOp, override_order: Option<&[TransportId]>) -> Vec<TransportId> {
    let default = op.default_transport_order();
    let Some(override_order) = override_order else {
        return default.to_vec();
    };

    let mut order: Vec<TransportId> = override_order
        .iter()
        .copied()
        .filter(|id| default.contains(id))
        .collect();
    order.dedup();
    for id in default {
        if !order.contains(id) {
            order.push(*id);
        }
    }
    order
}

/// Run `op` against `device`, trying each eligible transport in priority order.
pub async fn execute_op(
    state: &StateHandle,
    device: &Device,
    op: &DeviceOp,
    override_order: Option<&[TransportId]>,
) -> anyhow::Result<()> {
    execute_ops(state, device, std::slice::from_ref(op), override_order).await
}

/// Run several operations against `device` as one unit.
///
/// The transport order is decided by the first operation; a batch is only ever
/// built from operations that belong together, and splitting one across two
/// transports would leave a light half configured.
pub async fn execute_ops(
    state: &StateHandle,
    device: &Device,
    ops: &[DeviceOp],
    override_order: Option<&[TransportId]>,
) -> anyhow::Result<()> {
    let Some(first) = ops.first() else {
        return Ok(());
    };
    let order = resolve_order(first, override_order);
    let mut declined = vec![];

    for id in order {
        let Some(transport) = transport_by_id(id) else {
            // Not registered in this build; e.g. BLE before milestone 4.
            continue;
        };

        match transport.try_execute_batch(state, device, ops).await {
            Ok(Handled::Yes) => return Ok(()),
            Ok(Handled::NotSupported) => declined.push(id),
            Err(err) if transport.fallback_on_error() => {
                log::warn!(
                    "{id} failed to {} for {device}: {err:#}; trying next transport",
                    first.describe()
                );
                declined.push(id);
            }
            Err(err) => return Err(err),
        }
    }

    let tried = declined
        .iter()
        .map(|id| id.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    anyhow::bail!(
        "Unable to {} for {device} (no transport available; declined: {tried})",
        first.describe()
    );
}

#[cfg(test)]
mod test {
    use super::TransportId::*;
    use super::*;

    #[test]
    fn order_without_override_is_the_default() {
        let op = DeviceOp::PowerOn(true);
        assert_eq!(resolve_order(&op, None), vec![Ble, Lan, Iot, Platform]);
    }

    #[test]
    fn override_promotes_listed_transports() {
        let op = DeviceOp::PowerOn(true);
        assert_eq!(
            resolve_order(&op, Some(&[Platform, Iot])),
            vec![Platform, Iot, Ble, Lan]
        );
    }

    #[test]
    fn override_cannot_introduce_an_invalid_transport() {
        // Target temperature is Platform-only; naming BLE must not enable it.
        let op = DeviceOp::SetTargetTemperature {
            instance: "targetTemperature".to_string(),
            target: crate::temperature::TemperatureValue::with_celsius(20.),
        };
        assert_eq!(resolve_order(&op, Some(&[Ble])), vec![Platform]);
    }

    #[test]
    fn scenes_keep_platform_ahead_of_lan() {
        let op = DeviceOp::SetScene("Sunrise".to_string());
        assert_eq!(resolve_order(&op, None), vec![Ble, Platform, Lan]);
    }
}
