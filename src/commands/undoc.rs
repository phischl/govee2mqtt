use crate::ble::Base64HexBytes;
use crate::service::iot::start_iot_client;
use std::sync::Arc;

#[derive(clap::Parser, Debug)]
pub struct UndocCommand {
    #[command(subcommand)]
    cmd: SubCommand,
}

#[derive(clap::Parser, Debug)]
#[allow(clippy::enum_variant_names)]
enum SubCommand {
    DumpOneClick {},
    ShowOneClick {},
    OneClick {
        name: String,
    },
    /// Send raw 20-byte frames to a device over the AWS IoT `ptReal` channel,
    /// then ask for a status so the effect can be read back.
    ///
    /// A protocol bench: the same frames travel over Bluetooth, so this is the
    /// safe way to try an unverified one — no radio, no connection slot, and
    /// the reply arrives on the account topic either way.
    ///
    ///     govee undoc pt-real --device "Flur oben Stehlampe" 33a501640000ff
    ///
    /// Frames are hex, with or without separators, and are padded and
    /// checksummed for you.
    PtReal {
        /// Device name or id, as Govee's account metadata spells it.
        #[arg(long)]
        device: String,
        /// Seconds to wait for the device to answer before exiting.
        #[arg(long, default_value_t = 10)]
        wait: u64,
        /// One or more frames, in hex.
        frames: Vec<String>,
    },
}

impl UndocCommand {
    pub async fn run(&self, args: &crate::Args) -> anyhow::Result<()> {
        match &self.cmd {
            SubCommand::DumpOneClick {} => {
                let client = args.undoc_args.api_client()?;
                let token = client.login_community().await?;
                let res = client.get_saved_one_click_shortcuts(&token).await?;

                println!("{res:#?}");
            }
            SubCommand::ShowOneClick {} => {
                let client = args.undoc_args.api_client()?;
                let items = client.parse_one_clicks().await?;
                println!("{items:#?}");
            }
            SubCommand::OneClick { name } => {
                let client = args.undoc_args.api_client()?;
                let items = client.parse_one_clicks().await?;
                let item = items
                    .iter()
                    .find(|item| &item.name == name)
                    .ok_or_else(|| anyhow::anyhow!("didn't find item {name}"))?;

                let state = Arc::new(crate::service::state::State::new());
                start_iot_client(&args.undoc_args, state.clone(), None).await?;
                let iot = state.get_iot_client().await.expect("just started iot");

                iot.activate_one_click(item).await?;
            }
            SubCommand::PtReal {
                device,
                wait,
                frames,
            } => {
                anyhow::ensure!(!frames.is_empty(), "give me at least one frame");

                let client = args.undoc_args.api_client()?;
                let account = client.login_account_cached().await?;
                let entry = client
                    .get_device_list(&account.token)
                    .await?
                    .devices
                    .into_iter()
                    .find(|d| {
                        d.device_name.eq_ignore_ascii_case(device)
                            || d.device.eq_ignore_ascii_case(device)
                    })
                    .ok_or_else(|| anyhow::anyhow!("no device matching '{device}'"))?;

                let mut encoded = vec![];
                for frame in frames {
                    let packet = Base64HexBytes::with_bytes(parse_hex(frame)?);
                    println!("-> {packet:?}");
                    encoded.extend(packet.base64());
                }

                let state = Arc::new(crate::service::state::State::new());
                start_iot_client(&args.undoc_args, state.clone(), None).await?;
                let iot = state.get_iot_client().await.expect("just started iot");

                iot.send_real(&entry, encoded).await?;

                // The device answers on the account topic, which the IoT client
                // logs. Give it a moment, then ask outright.
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                iot.request_status_update(&entry).await?;
                tokio::time::sleep(std::time::Duration::from_secs(*wait)).await;
            }
        }
        Ok(())
    }
}

/// Hex, with or without separators.
fn parse_hex(text: &str) -> anyhow::Result<Vec<u8>> {
    let digits: String = text.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    anyhow::ensure!(
        digits.len() % 2 == 0,
        "'{text}' has an odd number of hex digits"
    );
    (0..digits.len())
        .step_by(2)
        .map(|n| {
            u8::from_str_radix(&digits[n..n + 2], 16)
                .map_err(|err| anyhow::anyhow!("bad hex in '{text}': {err}"))
        })
        .collect()
}
