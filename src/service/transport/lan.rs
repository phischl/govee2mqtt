//! Govee's LAN UDP protocol.
//!
//! This is the only transport that verifies its own work: after issuing a command
//! it polls the device until the reported state matches, which is why each arm
//! carries an acceptor closure.

use super::{DeviceOp, Handled, Transport, TransportId};
use crate::lan_api::DeviceColor;
use crate::service::device::Device;
use crate::service::state::StateHandle;
use async_trait::async_trait;

pub struct LanTransport;

#[async_trait]
impl Transport for LanTransport {
    fn id(&self) -> TransportId {
        TransportId::Lan
    }

    async fn try_execute(
        &self,
        state: &StateHandle,
        device: &Device,
        op: &DeviceOp,
    ) -> anyhow::Result<Handled> {
        let Some(lan_dev) = &device.lan_device else {
            return Ok(Handled::NotSupported);
        };

        match op {
            DeviceOp::LightPowerOn(on)
                if device.get_light_power_toggle_instance_name().is_none() =>
            {
                // Upstream refused to touch a device whose light toggle it could
                // not name, on every transport rather than just the Platform one.
                let _ = on;
                return Ok(Handled::NotSupported);
            }
            DeviceOp::PowerOn(on) | DeviceOp::LightPowerOn(on) => {
                let on = *on;
                log::info!("Using LAN API to set {device} power state");
                lan_dev.send_turn(on).await?;
                state
                    .poll_lan_api(lan_dev, |status| status.on == on)
                    .await?;
            }
            DeviceOp::SetBrightness(percent) => {
                let percent = *percent;
                log::info!("Using LAN API to set {device} brightness");
                lan_dev.send_brightness(percent).await?;
                state
                    .poll_lan_api(lan_dev, |status| status.brightness == percent)
                    .await?;
            }
            DeviceOp::SetColorRgb { r, g, b } => {
                let color = DeviceColor {
                    r: *r,
                    g: *g,
                    b: *b,
                };
                log::info!("Using LAN API to set {device} color");
                lan_dev.send_color_rgb(color).await?;
                state
                    .poll_lan_api(lan_dev, |status| status.color == color)
                    .await?;
            }
            DeviceOp::SetColorTemperature(kelvin) => {
                let kelvin = *kelvin;
                log::info!("Using LAN API to set {device} color temperature");
                lan_dev.send_color_temperature_kelvin(kelvin).await?;
                state
                    .poll_lan_api(lan_dev, |status| status.color_temperature_kelvin == kelvin)
                    .await?;
            }
            DeviceOp::SetScene(scene) => {
                log::info!("Using LAN API to set {device} to scene {scene}");
                lan_dev.set_scene_by_name(scene).await?;
            }
            DeviceOp::SetHumidifierParameter { .. } | DeviceOp::SetTargetTemperature { .. } => {
                return Ok(Handled::NotSupported);
            }
        }

        Ok(Handled::Yes)
    }
}
