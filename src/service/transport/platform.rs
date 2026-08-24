//! Govee's official Platform HTTP API — the fallback that works for the widest
//! range of devices, at the cost of latency and a request quota.

use super::{DeviceOp, Handled, Transport, TransportId};
use crate::service::device::Device;
use crate::service::state::StateHandle;
use async_trait::async_trait;

pub struct PlatformTransport;

#[async_trait]
impl Transport for PlatformTransport {
    fn id(&self) -> TransportId {
        TransportId::Platform
    }

    async fn try_execute(
        &self,
        state: &StateHandle,
        device: &Device,
        op: &DeviceOp,
    ) -> anyhow::Result<Handled> {
        let (Some(client), Some(info)) =
            (state.get_platform_client().await, &device.http_device_info)
        else {
            return Ok(Handled::NotSupported);
        };

        match op {
            DeviceOp::PowerOn(on) => {
                log::info!("Using Platform API to set {device} power state");
                client.set_power_state(info, *on).await?;
            }
            DeviceOp::LightPowerOn(on) => {
                let Some(instance_name) = device.get_light_power_toggle_instance_name() else {
                    return Ok(Handled::NotSupported);
                };
                log::info!("Using Platform API to set {device} light {instance_name} state");
                client.set_toggle_state(info, instance_name, *on).await?;
            }
            DeviceOp::SetBrightness(percent) => {
                log::info!("Using Platform API to set {device} brightness");
                client.set_brightness(info, *percent).await?;
            }
            DeviceOp::SetColorRgb { r, g, b } => {
                log::info!("Using Platform API to set {device} color");
                client.set_color_rgb(info, *r, *g, *b).await?;
            }
            DeviceOp::SetColorTemperature(kelvin) => {
                log::info!("Using Platform API to set {device} color temperature");
                client.set_color_temperature(info, *kelvin).await?;
            }
            DeviceOp::SetScene(scene) => {
                // Some devices report capabilities the Platform API then refuses to
                // honour; the quirk table steers those to the LAN encoding instead.
                if device.avoid_platform_api() {
                    return Ok(Handled::NotSupported);
                }
                log::info!("Using Platform API to set {device} to scene {scene}");
                client.set_scene_by_name(info, scene).await?;
            }
            DeviceOp::SetHumidifierParameter { work_mode, value } => {
                log::info!("Using Platform API to set {device} work mode {work_mode}");
                client.set_work_mode(info, *work_mode, *value).await?;
            }
            DeviceOp::SetTargetTemperature { instance, target } => {
                log::info!("Using Platform API to set {device} target temperature to {target}");
                client
                    .set_target_temperature(info, instance, *target)
                    .await?;
            }
        }

        Ok(Handled::Yes)
    }
}
