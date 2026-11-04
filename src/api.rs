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
    /// Plugover on the relay, once it answered; none before 0.5.
    #[serde(default)]
    pub version: Option<String>,
    /// Why it is offline.
    #[serde(default)]
    pub error: Option<String>,
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
