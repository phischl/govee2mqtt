//! The nightlight sub-device of a humidifier.
//!
//! Humidifiers expose their nightlight as a light entity, but it is not
//! addressable through the normal power/brightness/color commands: it needs a raw
//! BLE frame delivered over IoT `ptReal`. Upstream handled this with a
//! `try_humidifier_set_nightlight` call bolted onto the front of three separate
//! operations; modelling it as a transport keeps that special case out of the
//! routing logic.
//!
//! Selection is implicit: `encode_for_sku` only succeeds for SKUs that have a
//! nightlight codec registered in `ble.rs`, so every other device declines here.

use super::{DeviceOp, Handled, Transport, TransportId};
use crate::ble::{Base64HexBytes, SetHumidifierNightlightParams};
use crate::service::device::Device;
use crate::service::state::StateHandle;
use async_trait::async_trait;

pub struct NightlightTransport;

#[async_trait]
impl Transport for NightlightTransport {
    fn id(&self) -> TransportId {
        TransportId::Nightlight
    }

    async fn try_execute(
        &self,
        state: &StateHandle,
        device: &Device,
        op: &DeviceOp,
    ) -> anyhow::Result<Handled> {
        let mut params: SetHumidifierNightlightParams =
            device.nightlight_state.unwrap_or_default().into();

        match op {
            DeviceOp::LightPowerOn(on) => params.on = *on,
            DeviceOp::SetBrightness(percent) => {
                params.brightness = *percent;
                params.on = true;
            }
            DeviceOp::SetColorRgb { r, g, b } => {
                params.r = *r;
                params.g = *g;
                params.b = *b;
                params.on = true;
            }
            _ => return Ok(Handled::NotSupported),
        }

        let Ok(command) = Base64HexBytes::encode_for_sku(&device.sku, &params) else {
            return Ok(Handled::NotSupported);
        };
        let (Some(iot), Some(info)) = (state.get_iot_client().await, &device.undoc_device_info)
        else {
            return Ok(Handled::NotSupported);
        };

        log::info!("Using IoT API to set {device} nightlight");
        iot.send_real(&info.entry, command.base64()).await?;
        Ok(Handled::Yes)
    }
}
