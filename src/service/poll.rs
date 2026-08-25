//! How often each transport may be asked for a device's state.
//!
//! Polling is not one cost but four. A LAN query is a UDP packet on your own
//! network; an AWS IoT status request rides a connection we already hold; a
//! Platform API call spends a share of a daily quota Govee does not let us
//! measure — it returns no rate-limit headers at all; and a Bluetooth poll
//! occupies a proxy connection slot for a second or two. A single number would
//! force those four against each other, so each gets its own.

use once_cell::sync::OnceCell;
use std::time::Duration;

/// Applies to AWS IoT, the Platform API and Bluetooth.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(900);

/// The LAN default is much shorter on purpose: it costs nothing, and some Govee
/// firmware flickers about a minute after a poll, which regular polling keeps
/// predictable rather than random (upstream issue #250).
pub const DEFAULT_LAN_INTERVAL: Duration = Duration::from_secs(30);

/// The poll loop cannot honour an interval shorter than its own tick, so the
/// tick follows the shortest configured interval — down to this floor.
pub const MIN_TICK: Duration = Duration::from_secs(5);

/// The tick never grows beyond this, so a device that becomes due is noticed
/// promptly even when every interval is long.
pub const MAX_TICK: Duration = Duration::from_secs(30);

#[derive(clap::Parser, Debug, Default)]
pub struct PollArguments {
    /// How many seconds a device's state may be stale before it is polled
    /// again. Applies to AWS IoT, the Platform API and Bluetooth; the LAN has
    /// its own setting because it costs nothing. Default 900 (15 minutes).
    /// You may also set this via the GOVEE_POLL_INTERVAL environment variable.
    #[arg(long, global = true, value_name = "SECONDS")]
    poll_interval: Option<u64>,

    /// Seconds between LAN status queries. Cheap and local, so this defaults to
    /// 30 -- every tick of the poll loop.
    /// You may also set this via the GOVEE_POLL_INTERVAL_LAN environment variable.
    #[arg(long, global = true, value_name = "SECONDS")]
    poll_interval_lan: Option<u64>,

    /// Seconds between AWS IoT status requests. This is the channel that
    /// carries per-segment colours, so lowering it refreshes segment entities
    /// sooner. Defaults to --poll-interval.
    /// You may also set this via the GOVEE_POLL_INTERVAL_IOT environment variable.
    #[arg(long, global = true, value_name = "SECONDS")]
    poll_interval_iot: Option<u64>,

    /// Seconds between Platform API polls. The one to raise if you are worried
    /// about Govee's daily request quota: every device without an AWS IoT path
    /// costs one request per interval. Defaults to --poll-interval.
    /// You may also set this via the GOVEE_POLL_INTERVAL_PLATFORM environment variable.
    #[arg(long, global = true, value_name = "SECONDS")]
    poll_interval_platform: Option<u64>,

    /// Seconds between Bluetooth polls of Bluetooth-only devices. Each one
    /// occupies a proxy connection slot while it runs. Defaults to
    /// --poll-interval.
    /// You may also set this via the GOVEE_POLL_INTERVAL_BLE environment variable.
    #[arg(long, global = true, value_name = "SECONDS")]
    poll_interval_ble: Option<u64>,
}

impl PollArguments {
    pub fn intervals(&self) -> anyhow::Result<PollIntervals> {
        let general =
            resolve(self.poll_interval, "GOVEE_POLL_INTERVAL")?.unwrap_or(DEFAULT_INTERVAL);

        Ok(PollIntervals {
            lan: resolve(self.poll_interval_lan, "GOVEE_POLL_INTERVAL_LAN")?
                .unwrap_or(DEFAULT_LAN_INTERVAL),
            iot: resolve(self.poll_interval_iot, "GOVEE_POLL_INTERVAL_IOT")?.unwrap_or(general),
            platform: resolve(self.poll_interval_platform, "GOVEE_POLL_INTERVAL_PLATFORM")?
                .unwrap_or(general),
            ble: resolve(self.poll_interval_ble, "GOVEE_POLL_INTERVAL_BLE")?.unwrap_or(general),
        })
    }
}

fn resolve(flag: Option<u64>, var: &str) -> anyhow::Result<Option<Duration>> {
    let seconds = match flag {
        Some(value) => Some(value),
        None => crate::opt_env_var::<u64>(var)?,
    };

    match seconds {
        // Zero would mean "poll continuously", which is never what someone
        // means by it, and for the Platform API it would empty the quota in
        // minutes. Refuse rather than interpret.
        Some(0) => anyhow::bail!("{var} must be at least 1 second"),
        Some(value) => Ok(Some(Duration::from_secs(value))),
        None => Ok(None),
    }
}

/// The resolved intervals, for the handful of places that cannot be handed
/// them. The entity layer is one: a diagnostic sensor several calls below the
/// point where configuration is read needs to know how stale is too stale.
static CONFIGURED: OnceCell<PollIntervals> = OnceCell::new();

#[derive(Clone, Copy, Debug)]
pub struct PollIntervals {
    pub lan: Duration,
    pub iot: Duration,
    pub platform: Duration,
    pub ble: Duration,
}

impl Default for PollIntervals {
    fn default() -> Self {
        Self {
            lan: DEFAULT_LAN_INTERVAL,
            iot: DEFAULT_INTERVAL,
            platform: DEFAULT_INTERVAL,
            ble: DEFAULT_INTERVAL,
        }
    }
}

impl PollIntervals {
    /// Publish these as the process-wide intervals. Called once, at startup.
    pub fn install(self) {
        let _ = CONFIGURED.set(self);
    }

    /// What was installed at startup, or the defaults before that has happened.
    pub fn configured() -> Self {
        CONFIGURED.get().copied().unwrap_or_default()
    }

    /// How old a device's state may be before we call it missing rather than
    /// merely unpolled.
    ///
    /// Keyed to the *longest* interval, because a device polled on that one is
    /// legitimately quiet for that long, plus a grace period for the poll
    /// itself to run. Keying it to anything shorter would mark healthy devices
    /// missing the moment someone raised an interval to save quota.
    pub fn staleness_threshold(&self) -> chrono::Duration {
        let longest = [self.lan, self.iot, self.platform, self.ble]
            .into_iter()
            .max()
            .unwrap_or(DEFAULT_INTERVAL);

        chrono::Duration::from_std(longest).unwrap_or(chrono::Duration::seconds(900))
            + chrono::Duration::seconds(30)
    }

    /// How long the poll loop should sleep between passes.
    ///
    /// Nothing can be polled more often than the loop runs, so a short interval
    /// would silently round up to the tick without this.
    pub fn tick(&self) -> Duration {
        [self.lan, self.iot, self.platform, self.ble]
            .into_iter()
            .min()
            .unwrap_or(MAX_TICK)
            .clamp(MIN_TICK, MAX_TICK)
    }

    pub fn describe(&self) -> String {
        format!(
            "lan={}s iot={}s platform={}s ble={}s (loop tick {}s)",
            self.lan.as_secs(),
            self.iot.as_secs(),
            self.platform.as_secs(),
            self.ble.as_secs(),
            self.tick().as_secs()
        )
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn the_tick_follows_the_shortest_interval() {
        let intervals = PollIntervals {
            lan: Duration::from_secs(10),
            ..Default::default()
        };
        assert_eq!(intervals.tick(), Duration::from_secs(10));
    }

    /// A one-second LAN interval must not turn the loop into a busy wait.
    #[test]
    fn the_tick_has_a_floor() {
        let intervals = PollIntervals {
            lan: Duration::from_secs(1),
            ..Default::default()
        };
        assert_eq!(intervals.tick(), MIN_TICK);
    }

    /// Long intervals must not stop the loop noticing that a device is due.
    #[test]
    fn the_tick_has_a_ceiling() {
        let intervals = PollIntervals {
            lan: Duration::from_secs(3600),
            iot: Duration::from_secs(3600),
            platform: Duration::from_secs(3600),
            ble: Duration::from_secs(3600),
        };
        assert_eq!(intervals.tick(), MAX_TICK);
    }

    #[test]
    fn defaults_match_the_behaviour_before_this_was_configurable() {
        let intervals = PollIntervals::default();
        assert_eq!(intervals.lan, Duration::from_secs(30));
        assert_eq!(intervals.iot, Duration::from_secs(900));
        assert_eq!(intervals.platform, Duration::from_secs(900));
        assert_eq!(intervals.ble, Duration::from_secs(900));
        assert_eq!(intervals.tick(), Duration::from_secs(30));
    }

    /// Raising an interval to save quota must not make every device report
    /// itself missing.
    #[test]
    fn the_staleness_threshold_follows_the_longest_interval() {
        let intervals = PollIntervals {
            platform: Duration::from_secs(3600),
            ..Default::default()
        };
        assert_eq!(
            intervals.staleness_threshold(),
            chrono::Duration::seconds(3630)
        );
    }

    #[test]
    fn zero_is_rejected_rather_than_interpreted() {
        assert!(resolve(Some(0), "GOVEE_POLL_INTERVAL").is_err());
        assert_eq!(
            resolve(Some(60), "GOVEE_POLL_INTERVAL").unwrap(),
            Some(Duration::from_secs(60))
        );
    }
}
