//! Bluetooth LE, executed by the `govee_ble_executor` Home Assistant integration.
//!
//! Sits *last* in the default transport order (D2b): the proven paths are
//! faster and LAN is the only one that verifies its own work. Being last costs
//! nothing for a Bluetooth-only device, because every other transport declines
//! and this one serves it anyway.
//!
//! Unlike the other transports it opts into falling through on error: a radio
//! link that fails should hand over to the cloud, not surface as a failed
//! command.

use super::{DeviceOp, Handled, Transport, TransportId};
use crate::platform_api::DeviceType;
use crate::service::ble_scheduler::BleScheduler;
use crate::service::device::Device;
use crate::service::state::StateHandle;
use async_trait::async_trait;
use std::sync::Arc;

pub struct BleTransport;

impl BleTransport {
    /// The operations we have verified frame encodings for.
    fn handles(device: &Device, op: &DeviceOp) -> bool {
        if device.is_segmented() {
            // A segmented device ignores the whole-strip colour write, so this
            // used to decline everything but power and let the cloud have it.
            // The segment command is known now (CLAUDE.md §17) and the
            // scheduler sends colour as a mask over every segment — but only
            // when it knows how many there are, and colour temperature still
            // has no segment equivalent.
            return match op {
                DeviceOp::PowerOn(_) | DeviceOp::LightPowerOn(_) | DeviceOp::SetBrightness(_) => {
                    true
                }
                DeviceOp::SetColorRgb { .. } => device.segment_count().is_some_and(|n| n > 0),
                _ => false,
            };
        }

        matches!(
            op,
            DeviceOp::PowerOn(_)
                | DeviceOp::LightPowerOn(_)
                | DeviceOp::SetBrightness(_)
                | DeviceOp::SetColorRgb { .. }
                | DeviceOp::SetColorTemperature(_)
        )
    }

    /// What it takes to run a session, if Bluetooth is worth attempting at all.
    ///
    /// Being excluded by configuration, the executor being offline, or this
    /// device having failed repeatedly, is a routing decision rather than an
    /// error: say so quietly and let the next transport have it.
    async fn route(state: &StateHandle, device: &Device) -> Option<Route> {
        // Only lights speak the command set we have codecs for. Humidifiers and
        // the like keep their existing transports.
        if !matches!(device.device_type(), DeviceType::Light) {
            return None;
        }

        let scheduler = state.get_ble_scheduler().await?;
        let address = scheduler.address_for(device)?;

        if !scheduler.is_available_for(device).await {
            return None;
        }

        Some(Route { scheduler, address })
    }
}

/// A device we can reach right now, and the scheduler that will do it.
struct Route {
    scheduler: Arc<BleScheduler>,
    address: String,
}

#[async_trait]
impl Transport for BleTransport {
    fn id(&self) -> TransportId {
        TransportId::Ble
    }

    fn fallback_on_error(&self) -> bool {
        true
    }

    /// A single operation is just a batch of one; the work is the same.
    async fn try_execute(
        &self,
        state: &StateHandle,
        device: &Device,
        op: &DeviceOp,
    ) -> anyhow::Result<Handled> {
        self.try_execute_batch(state, device, std::slice::from_ref(op))
            .await
    }

    /// The whole point of the batch path: every operation in one radio session.
    async fn try_execute_batch(
        &self,
        state: &StateHandle,
        device: &Device,
        ops: &[DeviceOp],
    ) -> anyhow::Result<Handled> {
        if ops.iter().any(|op| !Self::handles(device, op)) {
            // Partially serving a batch would leave the light half configured
            // and the rest of it applied by a different transport.
            return Ok(Handled::NotSupported);
        }

        let Some(route) = Self::route(state, device).await else {
            return Ok(Handled::NotSupported);
        };

        route
            .scheduler
            .apply(state, device, &route.address, ops)
            .await?;
        Ok(Handled::Yes)
    }
}
