//! What survives a restart: the relay's identity and paired hosts, the host's paired relays and
//! which devices it uses when you play. Stored as TOML, written atomically, readable only by
//! the owner because it holds keys.

use crate::auth;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{fs, io::Write, path::Path};

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct RelayState {
    /// Random id announced over mDNS, so hosts find the relay again after an address change.
    pub id: String,
    #[serde(default)]
    pub hosts: Vec<PairedHost>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairedHost {
    pub id: String,
    pub name: String,
    pub key: String,
}

/// Open while a pairing code is valid; written by `plugover code`, read by the relay service.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct PairingWindow {
    pub code: String,
    /// Unix seconds.
    pub expires: u64,
    /// Wrong codes left before the window closes.
    pub attempts: u8,
}

impl PairingWindow {
    pub fn path(relay_state: &Path) -> std::path::PathBuf {
        relay_state.with_file_name("pairing.toml")
    }

    pub fn is_open(&self) -> bool {
        !self.code.is_empty() && self.attempts > 0 && now() < self.expires
    }
}

pub fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct HostConfig {
    #[serde(default)]
    pub relay: Vec<PairedRelay>,
    /// Devices passed through whenever a stream runs.
    #[serde(default)]
    pub auto: Vec<AutoRule>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairedRelay {
    pub id: String,
    pub name: String,
    /// Last address that worked, ip:port.
    pub address: String,
    /// Our id towards this relay.
    pub host_id: String,
    pub key: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AutoRule {
    pub relay: String,
    /// vendor:product.
    pub device: String,
}

pub fn decode_key(hex_key: &str) -> Result<auth::Key> {
    hex::decode(hex_key).ok().and_then(|b| b.try_into().ok()).context("stored key is malformed")
}

pub fn load<T: DeserializeOwned + Default>(path: &Path) -> Result<T> {
    match fs::read_to_string(path) {
        Ok(text) => toml::from_str(&text).with_context(|| format!("reading {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

pub fn save<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let dir = path.parent().context("state path has no directory")?;
    fs::create_dir_all(dir)?;
    let tmp = path.with_extension("tmp");
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    opts.open(&tmp)?.write_all(toml::to_string_pretty(value)?.as_bytes())?;
    fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))
}

/// This machine's name, for pairing and the UI.
pub fn hostname() -> String {
    #[cfg(windows)]
    let name = std::env::var("COMPUTERNAME").ok();
    #[cfg(not(windows))]
    let name = fs::read_to_string("/proc/sys/kernel/hostname").ok().map(|s| s.trim().to_string());
    name.filter(|n| !n.is_empty()).unwrap_or_else(|| "unknown".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saves_and_loads_host_config() {
        let path = std::env::temp_dir().join(format!("plugover-state-{}/host.toml", std::process::id()));
        assert!(load::<HostConfig>(&path).unwrap().relay.is_empty(), "missing file means empty config");
        let cfg = HostConfig {
            relay: vec![PairedRelay {
                id: "ab".into(),
                name: "simrig".into(),
                address: "192.168.1.50:7275".into(),
                host_id: "01".into(),
                key: "00".repeat(32),
            }],
            auto: vec![AutoRule { relay: "ab".into(), device: "046d:c24f".into() }],
        };
        save(&path, &cfg).unwrap();
        let back: HostConfig = load(&path).unwrap();
        assert_eq!(back.auto, cfg.auto);
        assert_eq!(decode_key(&back.relay[0].key).unwrap(), [0u8; 32]);
        #[cfg(unix)]
        assert_eq!(std::os::unix::fs::PermissionsExt::mode(&fs::metadata(&path).unwrap().permissions()) & 0o777, 0o600);
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
}
