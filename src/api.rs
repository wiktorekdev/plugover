//! What the host daemon reports, shared by the CLI, `doctor` and the console page. `cargo test`
//! writes the console's TypeScript types from these into plugin/ui/src/generated/.

use crate::proto::DeviceInfo;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS), ts(export))]
pub struct Status {
    pub host: String,
    /// Plugover on this host; empty from daemons before 0.5.
    #[serde(default)]
    pub version: String,
    /// A punktfunk stream is running.
    pub session: bool,
    pub relays: Vec<RelayStatus>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS), ts(export))]
pub struct RelayStatus {
    pub id: String,
    pub name: String,
    pub address: String,
    pub online: bool,
    /// The relay encrypts; once it has, it always must.
    #[serde(default)]
    pub encrypted: bool,
    /// It cannot encrypt, and you allowed connecting it without.
    #[serde(default)]
    pub unencrypted: bool,
    /// Plugover on the relay, once it answered; none before 0.5.
    #[serde(default)]
    pub version: Option<String>,
    /// Why it is offline.
    #[serde(default)]
    pub error: Option<String>,
    /// How quickly the relay answers; none until it has answered a few times (or before 0.5.1).
    #[serde(default)]
    pub latency: Option<Latency>,
    pub devices: Vec<DeviceStatus>,
    /// Devices chosen for auto-connect that the relay does not have right now.
    #[serde(default)]
    pub missing: Vec<Missing>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS), ts(export))]
pub struct DeviceStatus {
    #[serde(flatten)]
    pub info: DeviceInfo,
    pub state: DeviceState,
    /// Connects whenever a stream runs.
    pub auto: bool,
    /// The auto-connect rule names this port, not only the model.
    #[serde(default)]
    pub auto_port: bool,
    /// Why the last try to connect it failed, while it keeps failing.
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub history: History,
}

/// What happened to a device since the host daemon started. Times are Unix seconds.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS), ts(export))]
pub struct History {
    /// Times it vanished while plugged in here (the network, or the relay, let go of it).
    pub drops: u32,
    #[cfg_attr(test, ts(type = "number | null"))]
    pub last_drop: Option<u64>,
    /// The last time it could not connect, and why, even if it connected since.
    pub last_error: Option<String>,
    #[cfg_attr(test, ts(type = "number | null"))]
    pub last_error_at: Option<u64>,
}

/// Round trips over the network to the relay, in milliseconds, over the last few seconds.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS), ts(export))]
pub struct Latency {
    pub average: f64,
    /// Slowest minus fastest: how steady the link is, which matters as much for force feedback.
    pub jitter: f64,
}

impl Latency {
    /// None until there are enough samples to say anything.
    pub fn of(samples: &[std::time::Duration]) -> Option<Self> {
        if samples.len() < 3 {
            return None;
        }
        let ms: Vec<f64> = samples.iter().map(|d| d.as_secs_f64() * 1000.0).collect();
        let (lo, hi) = ms.iter().fold((f64::MAX, 0f64), |(lo, hi), &m| (lo.min(m), hi.max(m)));
        Some(Self { average: ms.iter().sum::<f64>() / ms.len() as f64, jitter: hi - lo })
    }

    /// Steady and quick enough for a wheel; spikes hurt force feedback more than the average.
    pub fn good(&self) -> bool {
        self.average < 5.0 && self.jitter < 10.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "lowercase")]
pub enum DeviceState {
    /// Plugged in here.
    Here,
    Free,
    /// Another host, or another USB/IP client, has it.
    Elsewhere,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS), ts(export))]
pub struct Missing {
    /// vendor:product.
    pub id: String,
    pub title: String,
    /// Set when the rule is for one port.
    #[serde(default)]
    pub port: Option<String>,
}

/// A relay announced on the network.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS), ts(export))]
pub struct Found {
    pub id: String,
    pub name: String,
    pub paired: bool,
    /// The announced address that answers; none if no address does.
    #[serde(default)]
    pub address: Option<String>,
    pub addresses: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS), ts(export))]
pub struct Discovered {
    pub relays: Vec<Found>,
}
