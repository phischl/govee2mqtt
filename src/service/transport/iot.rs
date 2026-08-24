//! Govee's undocumented AWS IoT MQTT API.
//!
//! Most operations are gated on `Device::iot_api_supported()`, which is driven by
//! the quirk table and defaults to false. Humidifier parameters are the exception:
//! upstream always attempts them over IoT because they are carried as a raw BLE
//! frame in a `ptReal` payload, which the Platform API cannot express.

use super::{DeviceOp, Handled, Transport, TransportId};
use crate::ble::{Base64HexBytes, SetHumidifierMode};
use crate::service::device::Device;
use crate::service::state::StateHandle;
use async_trait::async_trait;

pub struct IotTransport;

#[async_trait]
impl Transport for IotTransport {
    fn id(&self) -> TransportId {
        TransportId::Iot
    }

    async fn try_execute(
        &self,
        state: &StateHandle,
        device: &Device,
        op: &DeviceOp,
    ) -> anyhow::Result<Handled> {
        // Humidifier parameters bypass the `iot_api_supported` gate.
        if let DeviceOp::SetHumidifierParameter { work_mode, value } = op {
            let Ok(command) = Base64HexBytes::encode_for_sku(
                &device.sku,
                &SetHumidifierMode {
                    mode: *work_mode as u8,
                    param: *value as u8,
                },
            ) else {
                return Ok(Handled::NotSupported);
            };
            let (Some(iot), Some(info)) = (state.get_iot_client().await, &device.undoc_device_info)
            else {
                return Ok(Handled::NotSupported);
            };
            log::info!("Using IoT API to set {device} humidifier mode");
            iot.send_real(&info.entry, command.base64()).await?;
            return Ok(Handled::Yes);
        }

        if !device.iot_api_supported() {
            return Ok(Handled::NotSupported);
        }
        let (Some(iot), Some(info)) = (state.get_iot_client().await, &device.undoc_device_info)
        else {
            return Ok(Handled::NotSupported);
        };

        match op {
            DeviceOp::LightPowerOn(_)
                if device.get_light_power_toggle_instance_name().is_none() =>
            {
                return Ok(Handled::NotSupported);
            }
            DeviceOp::PowerOn(on) | DeviceOp::LightPowerOn(on) => {
                log::info!("Using IoT API to set {device} power state");
                iot.set_power_state(&info.entry, *on).await?;
            }
            DeviceOp::SetBrightness(percent) => {
                log::info!("Using IoT API to set {device} brightness");
                iot.set_brightness(&info.entry, *percent).await?;
            }
            DeviceOp::SetColorRgb { r, g, b } => {
                log::info!("Using IoT API to set {device} color");
                iot.set_color_rgb(&info.entry, *r, *g, *b).await?;
            }
            DeviceOp::SetColorTemperature(kelvin) => {
                log::info!("Using IoT API to set {device} color temperature");
                iot.set_color_temperature(&info.entry, *kelvin).await?;
            }
            DeviceOp::SetScene(_)
            | DeviceOp::SetHumidifierParameter { .. }
            | DeviceOp::SetTargetTemperature { .. } => return Ok(Handled::NotSupported),
        }

        Ok(Handled::Yes)
    }
}
