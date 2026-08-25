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
        if !matches!(
            op,
            DeviceOp::PowerOn(_)
                | DeviceOp::LightPowerOn(_)
                | DeviceOp::SetBrightness(_)
                | DeviceOp::SetColorRgb { .. }
                | DeviceOp::SetColorTemperature(_)
        ) {
            return Ok(Handled::NotSupported);
        }

        // Only lights speak the command set we have codecs for. Humidifiers and
        // the like keep their existing transports.
        if !matches!(device.device_type(), DeviceType::Light) {
            return Ok(Handled::NotSupported);
        }

        let Some(scheduler) = state.get_ble_scheduler().await else {
            return Ok(Handled::NotSupported);
        };
        let Some(address) = device.ble_address() else {
            return Ok(Handled::NotSupported);
        };

        // Being excluded by configuration, the executor being offline, or this
        // device having failed repeatedly, is a routing decision rather than an
        // error: say so quietly and let the next transport have it.
        if !scheduler.is_available_for(device).await {
            return Ok(Handled::NotSupported);
        }

        scheduler.apply(state, device, &address, op).await?;
        Ok(Handled::Yes)
    }
}
