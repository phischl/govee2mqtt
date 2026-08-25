//! Bluetooth LE, executed by the `govee_ble_executor` Home Assistant integration.
//!
//! Sits first in the transport order for the operations it understands, so that
//! a device within range of a proxy is driven locally rather than through
//! Govee's cloud. Unlike the other transports it opts into falling through on
//! error: a radio link that fails should hand over to the cloud, not surface as
//! a failed command.

use super::{DeviceOp, Handled, Transport, TransportId};
use crate::platform_api::DeviceType;
use crate::service::device::Device;
use crate::service::state::StateHandle;
use async_trait::async_trait;

pub struct BleTransport;

impl BleTransport {
    /// The operations we have verified frame encodings for.
    fn handles(device: &Device, op: &DeviceOp) -> bool {
        if device.is_segmented() {
            // Power is the only thing a segmented device honours from the
            // generic light command set; see Device::is_segmented. Declining
            // the rest sends it to the cloud instead of silently doing nothing.
            return matches!(op, DeviceOp::PowerOn(_) | DeviceOp::LightPowerOn(_));
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

    /// The scheduler and address, if Bluetooth is worth attempting at all.
    ///
    /// Being excluded by configuration, the executor being offline, or this
    /// device having failed repeatedly, is a routing decision rather than an
    /// error: say so quietly and let the next transport have it.
    async fn context(
        &self,
        state: &StateHandle,
        device: &Device,
    ) -> Option<(
        std::sync::Arc<crate::service::ble_scheduler::BleScheduler>,
        String,
    )> {
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

        Some((scheduler, address))
    }
}

#[async_trait]
impl Transport for BleTransport {
    fn id(&self) -> TransportId {
        TransportId::Ble
    }

    fn fallback_on_error(&self) -> bool {
        true
    }

    async fn try_execute(
        &self,
        state: &StateHandle,
        device: &Device,
        op: &DeviceOp,
    ) -> anyhow::Result<Handled> {
        if !Self::handles(device, op) {
            return Ok(Handled::NotSupported);
        }

        let Some((scheduler, address)) = self.context(state, device).await else {
            return Ok(Handled::NotSupported);
        };

        scheduler
            .apply(state, device, &address, std::slice::from_ref(op))
            .await?;
        Ok(Handled::Yes)
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

        let Some(context) = self.context(state, device).await else {
            return Ok(Handled::NotSupported);
        };
        let (scheduler, address) = context;

        scheduler.apply(state, device, &address, ops).await?;
        Ok(Handled::Yes)
    }
}
