use crate::lan_api::Client as LanClient;
use crate::platform_api::{DeviceType, GoveeApiClient};
use crate::service::ble_bridge::BleBridge;
use crate::service::ble_scheduler::{
    BleAddressOverrides, BleExclusions, BleScheduler, BleSchedulerConfig,
};
use crate::service::device::{BleAddressSource, Device};
use crate::service::hass::spawn_hass_integration;
use crate::service::http::run_http_server;
use crate::service::iot::start_iot_client;
use crate::service::poll::PollIntervals;
use crate::service::state::StateHandle;
use crate::undoc_api::GoveeUndocumentedApi;
use crate::version_info::govee_version;
use crate::UndocApiArguments;
use anyhow::Context;
use chrono::Utc;
use once_cell::sync::Lazy;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::time::{sleep, Duration};

/// Only a fallback for the `chrono` conversion below; the real defaults and
/// the configuration live in `service::poll`.
static POLL_INTERVAL: Lazy<chrono::Duration> = Lazy::new(|| chrono::Duration::seconds(900));

#[derive(clap::Parser, Debug)]
pub struct ServeCommand {
    /// The port on which the HTTP API will listen
    #[arg(long, default_value_t = 8056)]
    http_port: u16,
}

/// Point out devices whose Bluetooth address we had to guess.
///
/// Govee usually states the address; when it does not, we take the last six
/// octets of the device id. That is a guess, and a measured one at that: an
/// H601B's stated address is its device id plus one, so the guess would be
/// wrong for that whole family. Saying so at startup beats letting someone
/// puzzle over a device that advertises fine and never connects.
async fn report_derived_ble_addresses(state: &StateHandle, overrides: &BleAddressOverrides) {
    for device in state.devices().await {
        if overrides.get(&device.id).is_some() {
            continue;
        }
        if let Some((address, BleAddressSource::DerivedFromId)) = device.ble_address_with_source() {
            log::info!(
                "{device}: Govee states no Bluetooth address; guessing {address} from the \
                 device id. Correct it with ble_exclude or ble_address_map if it does not answer."
            );
        }
    }
}

/// Log what the Bluetooth exclusion list actually matched.
///
/// An entry that matches nothing is almost always a typo, and it would
/// otherwise fail silently: the device would simply keep using Bluetooth and
/// the user would be left wondering why their exclusion had no effect.
async fn report_ble_exclusions(state: &StateHandle, exclusions: &BleExclusions) {
    if exclusions.is_empty() {
        return;
    }

    let devices = state.devices().await;
    for entry in exclusions.entries() {
        let matched: Vec<String> = devices
            .iter()
            .filter(|device| BleExclusions::entry_matches(entry, device))
            .map(|device| device.to_string())
            .collect();

        if matched.is_empty() {
            log::warn!(
                "BLE exclusion {entry:?} does not match any known device; \
                 check the spelling against a device id, SKU or name"
            );
        } else {
            log::info!("BLE is disabled for {} by {entry:?}", matched.join(", "));
        }
    }
}

/// Restore segment counts learned in earlier runs.
///
/// Discovery lives in memory, so without this it starts from nothing after
/// every restart. For a device whose count comes only from its own frames —
/// every Bluetooth-only one, and an H6054 that Govee's API never mentions — the
/// segment entities go unavailable and return over the following polls while it
/// re-converges. Restoring the count skips all of that.
async fn restore_remembered_segments(state: &StateHandle) {
    for device in state.devices().await {
        let Some(count) = crate::cache::recall::<u32>(&format!("segments/{}", device.id)) else {
            continue;
        };
        if count == 0 {
            continue;
        }

        log::info!("{device} had {count} segment(s) when we last spoke to it");
        state
            .device_mut(&device.sku, &device.id)
            .await
            .set_remembered_segment_count(count);
    }
}

/// Read a Bluetooth-only device's state, if it is due and reachable.
async fn poll_via_ble(
    state: &StateHandle,
    device: &Device,
    now: chrono::DateTime<Utc>,
    interval: chrono::Duration,
) -> anyhow::Result<()> {
    let Some(scheduler) = state.get_ble_scheduler().await else {
        return Ok(());
    };
    let Some(address) = scheduler.address_for(device) else {
        return Ok(());
    };

    // Only lights have a command set we understand.
    if !matches!(device.device_type(), DeviceType::Light) {
        return Ok(());
    }

    // An offline executor or an open circuit breaker means a poll would just
    // burn a connection attempt.
    if !scheduler.is_available_for(device).await {
        return Ok(());
    }

    let due = match &device.last_polled {
        None => true,
        Some(last) => now - last > interval,
    };
    if !due {
        return Ok(());
    }

    // Recorded before the attempt, so that an unreachable device is retried on
    // the usual interval rather than on every tick of the poll loop.
    state
        .device_mut(&device.sku, &device.id)
        .await
        .set_last_polled();

    // A poll that cannot reach the device is an ordinary fact of life for
    // Bluetooth: the light may be switched off at the wall, or simply out of
    // range of every proxy. Reporting that as an error every poll interval
    // would bury the log. The circuit breaker escalates for us once a device
    // has failed repeatedly.
    if let Err(err) = scheduler
        .poll(
            state,
            &device.id,
            &device.sku,
            &address,
            device.segment_count(),
        )
        .await
    {
        log::debug!("polling {device} over BLE failed: {err:#}");
    }
    Ok(())
}

async fn poll_single_device(
    state: &StateHandle,
    device: &Device,
    intervals: &PollIntervals,
) -> anyhow::Result<()> {
    let now = Utc::now();

    if device.is_ble_only_device() == Some(true) {
        // Bluetooth-only devices have no other source to poll, so this is the
        // only way their state ever reaches Home Assistant. Devices that also
        // have a cloud or LAN presence keep using those; their Bluetooth writes
        // are verified within the session that issued them.
        return poll_via_ble(state, device, now, as_chrono(intervals.ble)).await;
    }

    // Collect the device status via the LAN API, if possible.
    // This is partially redundant with the LAN discovery task,
    // but the timing of that is not as regular and predictable
    // because it employs exponential backoff.
    // Some Govee devices have bad firmware that will cause the
    // lights to flicker about a minute after polling, so it
    // is desirable to keep polling on a regular basis.
    // <https://github.com/wez/govee2mqtt/issues/250>
    let lan_is_stale = match &device.last_lan_device_status_update {
        None => true,
        Some(last) => now - last > as_chrono(intervals.lan),
    };
    if lan_is_stale {
        if let Some(lan_device) = &device.lan_device {
            if let Some(client) = state.get_lan_client().await {
                if let Ok(status) = client.query_status(lan_device).await {
                    state
                        .device_mut(&lan_device.sku, &lan_device.device)
                        .await
                        .set_lan_device_status(status);
                    state.notify_of_state_change(&lan_device.device).await.ok();
                }
            }
        }
    }

    // The interval belonging to the transport we are about to use. If AWS IoT is
    // tried and fails we fall through to the Platform API still on the IoT
    // interval; the two share a default, and a failed IoT poll is rare enough
    // not to warrant a second gate.
    let needs_platform = device.needs_platform_poll();
    let poll_interval = device.preferred_poll_interval(as_chrono(if needs_platform {
        intervals.platform
    } else {
        intervals.iot
    }));

    let can_update = match &device.last_polled {
        None => true,
        Some(last) => now - last > poll_interval,
    };

    if !can_update {
        return Ok(());
    }

    let device_state = device.device_state();
    let needs_update = match &device_state {
        None => true,
        Some(state) => now - state.updated > poll_interval,
    };

    if !needs_update {
        return Ok(());
    }

    // Don't interrogate via HTTP if we can use the LAN.
    // If we have LAN and the device is stale, it is likely
    // offline and there is little sense in burning up request
    // quota to the platform API for it
    if device.lan_device.is_some() && !needs_platform {
        log::trace!("LAN-available device {device} needs a status update; it's likely offline.");
        return Ok(());
    }

    if !needs_platform && state.poll_iot_api(device).await? {
        return Ok(());
    }

    state.poll_platform_api(device).await?;

    Ok(())
}

async fn periodic_state_poll(state: StateHandle, intervals: PollIntervals) -> anyhow::Result<()> {
    sleep(Duration::from_secs(20)).await;
    let tick = intervals.tick();
    loop {
        for d in state.devices().await {
            if let Err(err) = poll_single_device(&state, &d, &intervals).await {
                log::error!("while polling {d}: {err:#}");
            }
        }

        sleep(tick).await;
    }
}

/// Poll bookkeeping is in `chrono` time while configuration is in `std::time`;
/// this is the one place the two meet.
fn as_chrono(duration: Duration) -> chrono::Duration {
    chrono::Duration::from_std(duration).unwrap_or(*POLL_INTERVAL)
}

async fn enumerate_devices_via_platform_api(
    state: StateHandle,
    client: Option<GoveeApiClient>,
) -> anyhow::Result<()> {
    let client = match client {
        Some(client) => client,
        None => match state.get_platform_client().await {
            Some(client) => client,
            None => return Ok(()),
        },
    };

    log::info!("Querying platform API for device list");
    for info in client.get_devices().await? {
        let mut device = state.device_mut(&info.sku, &info.device).await;
        device.set_http_device_info(info);
    }
    Ok(())
}

async fn enumerate_devices_via_undo_api(
    state: StateHandle,
    client: Option<GoveeUndocumentedApi>,
    args: &UndocApiArguments,
) -> anyhow::Result<()> {
    let (client, needs_start) = match client {
        Some(client) => (client, true),
        None => match state.get_undoc_client().await {
            Some(client) => (client, false),
            None => return Ok(()),
        },
    };

    log::info!("Querying undocumented API for device + room list");
    let acct = client.login_account_cached().await?;
    let info = client.get_device_list(&acct.token).await?;
    let mut group_by_id = HashMap::new();
    for group in info.groups {
        group_by_id.insert(group.group_id, group.group_name);
    }
    for entry in info.devices {
        let mut device = state.device_mut(&entry.sku, &entry.device).await;
        let room_name = group_by_id.get(&entry.group_id).map(|name| name.as_str());
        device.set_undoc_device_info(entry, room_name);
    }

    if needs_start {
        start_iot_client(args, state.clone(), Some(acct)).await?;
    }
    Ok(())
}

const ISSUE_76_EXPLANATION: &str = "Startup cannot automatically continue because entity names\n\
    could become inconsistent especially across frequent similar\n\
    intermittent issues if/as they occur on an ongoing basis.\n\
    Please see https://github.com/wez/govee2mqtt/issues/76\n\
    A workaround is to remove the Govee API credentials from your\n\
    configuration, which will cause this govee2mqtt to use only\n\
    the LAN API. Two consequences of that will be loss of control\n\
    over devices that do not support the LAN API, and also devices\n\
    changing entity ID to less descriptive names due to lack of\n\
    metadata availability via the LAN API.";

impl ServeCommand {
    pub async fn run(&self, args: &crate::Args) -> anyhow::Result<()> {
        log::info!("Starting service. version {}", govee_version());
        let state = Arc::new(crate::service::state::State::new());

        // First, use the HTTP APIs to determine the list of devices and
        // their names.

        if let Ok(client) = args.api_args.api_client() {
            if let Err(err) =
                enumerate_devices_via_platform_api(state.clone(), Some(client.clone())).await
            {
                anyhow::bail!(
                    "Error during initial platform API discovery: {err:#}\n{ISSUE_76_EXPLANATION}"
                );
            }

            // only record the client after we've completed the
            // initial platform disco attempt
            state.set_platform_client(client).await;

            // spawn periodic discovery task
            let state = state.clone();
            tokio::spawn(async move {
                loop {
                    sleep(Duration::from_secs(600)).await;
                    if let Err(err) = enumerate_devices_via_platform_api(state.clone(), None).await
                    {
                        log::error!("Error during periodic platform API discovery: {err:#}");
                    }
                }
            });
        }
        if let Ok(client) = args.undoc_args.api_client() {
            if let Err(err) = enumerate_devices_via_undo_api(
                state.clone(),
                Some(client.clone()),
                &args.undoc_args,
            )
            .await
            {
                anyhow::bail!(
                    "Error during initial undoc API discovery: {err:#}\n{ISSUE_76_EXPLANATION}"
                );
            }

            // only record the client after we've completed the
            // initial undoc disco attempt
            state.set_undoc_client(client).await;

            // spawn periodic discovery task
            let state = state.clone();
            let args = args.undoc_args.clone();
            tokio::spawn(async move {
                loop {
                    sleep(Duration::from_secs(600)).await;
                    if let Err(err) =
                        enumerate_devices_via_undo_api(state.clone(), None, &args).await
                    {
                        log::error!("Error during periodic undoc API discovery: {err:#}");
                    }
                }
            });
        }

        // Now start LAN discovery

        let options = args.lan_disco_args.to_disco_options()?;
        if !options.is_empty() {
            log::info!("Starting LAN discovery");
            let state = state.clone();
            let (client, mut scan) = LanClient::new(options).await?;

            state.set_lan_client(client.clone()).await;

            tokio::spawn(async move {
                while let Some(lan_device) = scan.recv().await {
                    log::trace!("LAN disco: {lan_device:?}");
                    state
                        .device_mut(&lan_device.sku, &lan_device.device)
                        .await
                        .set_lan_device(lan_device.clone());

                    let state = state.clone();
                    let client = client.clone();
                    tokio::spawn(async move {
                        if let Ok(status) = client.query_status(&lan_device).await {
                            state
                                .device_mut(&lan_device.sku, &lan_device.device)
                                .await
                                .set_lan_device_status(status);

                            log::trace!("LAN disco: update and notify {}", lan_device.device);
                            state.notify_of_state_change(&lan_device.device).await.ok();
                        }
                    });
                }
            });

            // I don't love that this is 10 seconds but since our timeout
            // for query_status is 10 seconds, and we show a warning for
            // devices that didn't respond in the section below, in the
            // interest of reducing false positives we need to wait long
            // enough to provide high-signal warnings.
            log::info!("Waiting 10 seconds for LAN API discovery");
            sleep(Duration::from_secs(10)).await;
        }

        log::info!("Devices returned from Govee's APIs");
        for device in state.devices().await {
            log::info!("{device}");
            if let Some(lan) = &device.lan_device {
                log::info!("  LAN API: ip={:?}", lan.ip);
            }
            if let Some(http_info) = &device.http_device_info {
                let kind = &http_info.device_type;
                let rgb = http_info.supports_rgb();
                let bright = http_info.supports_brightness();
                let color_temp = http_info.get_color_temperature_range();
                let segment_rgb = http_info.supports_segmented_rgb();
                log::info!(
                    "  Platform API: {kind}. supports_rgb={rgb} supports_brightness={bright}"
                );
                log::info!("                color_temp={color_temp:?} segment_rgb={segment_rgb:?}");
                log::trace!("{http_info:#?}");
            }
            if let Some(undoc) = &device.undoc_device_info {
                let room = &undoc.room_name;
                let supports_iot = undoc.entry.device_ext.device_settings.topic.is_some();
                let ble_only = undoc.entry.device_ext.device_settings.wifi_name.is_none();
                log::info!(
                    "  Undoc: room={room:?} supports_iot={supports_iot} ble_only={ble_only}"
                );
                log::trace!("{undoc:#?}");
            }
            if let Some(quirk) = device.resolve_quirk() {
                log::info!("  {quirk:?}");

                // Sanity check for LAN devices: if we don't see an API for it,
                // it may indicate a networking issue
                if quirk.lan_api_capable && device.lan_device.is_none() {
                    log::warn!(
                        "  This device should be available via the LAN API, \
                        but didn't respond to probing yet. Possible causes:"
                    );
                    log::warn!("  1) LAN API needs to be enabled in the Govee Home App.");
                    log::warn!("  2) The device is offline.");
                    log::warn!("  3) A network configuration issue is preventing communication.");
                    log::warn!(
                        "  4) The device needs a firmware update before it can enable LAN API."
                    );
                    log::warn!(
                        "  5) The hardware version of the device is too old to enable the LAN API."
                    );
                }
            } else if device.http_device_info.is_none() {
                log::warn!("  Unknown device type. Cannot map to Home Assistant.");
                if state.get_platform_client().await.is_none() {
                    log::warn!(
                        "  Recommendation: configure your Govee API Key so that \
                                  metadata can be fetched from Govee"
                    );
                }
            }

            log::info!("");
        }

        // Start periodic status polling
        {
            let intervals = args.poll_args.intervals()?;
            log::info!("Poll intervals: {}", intervals.describe());
            // Also published process-wide: the diagnostic sensor decides what
            // counts as stale from these, and it is nowhere near this call.
            intervals.install();
            let state = state.clone();
            tokio::spawn(async move {
                if let Err(err) = periodic_state_poll(state, intervals).await {
                    log::error!("periodic_state_poll: {err:#}");
                }
            });
        }

        // start advertising on local mqtt
        let transport_order = args.transport_args.order()?;
        if !transport_order.is_empty() {
            log::info!("Preferred transport order: {transport_order:?}");
            state.set_transport_order(Some(transport_order)).await;
        }

        // Outside the Bluetooth block below on purpose: a segment count can be
        // learned over AWS IoT just as well, and an H6054's only comes from
        // there.
        restore_remembered_segments(&state).await;

        // Set up before the MQTT loop starts: the loop registers routes for the
        // executor's topics only if a scheduler exists, and the retained status
        // message arrives as soon as we subscribe.
        if args.ble_args.is_disabled()? {
            log::info!("BLE transport is disabled by --no-ble");
        } else {
            let bridge = Arc::new(BleBridge::new(args.ble_args.topic_prefix()?));
            log::info!(
                "BLE transport will talk to the govee_ble_executor integration on {}",
                bridge.request_topic()
            );

            let exclusions = args
                .ble_args
                .exclude_spec()?
                .as_deref()
                .map(BleExclusions::parse)
                .unwrap_or_default();
            report_ble_exclusions(&state, &exclusions).await;

            let mut config = BleSchedulerConfig {
                exclusions,
                ..BleSchedulerConfig::default()
            };
            if let Some(spec) = args.ble_args.address_map()? {
                config.address_overrides = BleAddressOverrides::parse(&spec)?;
                log::info!(
                    "Using {} hand-corrected BLE address(es)",
                    config.address_overrides.entries()
                );
            }
            if let Some(max_concurrent) = args.ble_args.max_concurrent()? {
                config.max_concurrent = max_concurrent.max(1);
                log::info!(
                    "Allowing {} concurrent BLE session(s)",
                    config.max_concurrent
                );
            }

            report_derived_ble_addresses(&state, &config.address_overrides).await;

            state
                .set_ble_scheduler(Arc::new(BleScheduler::new(bridge, config)))
                .await;
        }

        spawn_hass_integration(state.clone(), &args.hass_args).await?;

        run_http_server(state.clone(), self.http_port)
            .await
            .with_context(|| format!("Starting HTTP service on port {}", self.http_port))
    }
}
