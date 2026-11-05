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
    /// vendor:product ids hosts may use. Unset shares every device except hubs and storage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub share: Option<Vec<String>>,
    #[serde(default)]
    pub hosts: Vec<PairedHost>,
}

impl RelayState {
    /// Why a device stays on the relay even though it could be lent.
    pub fn withheld(&self, id: &str) -> Option<&'static str> {
        self.share.as_ref().is_some_and(|s| !s.iter().any(|d| d == id)).then_some("not shared by the relay")
    }

    /// One paired host by id or name.
    pub fn host(&self, who: &str) -> Option<&PairedHost> {
        self.hosts.iter().find(|h| h.id == who || h.name == who)
    }
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
    /// Names and kinds set with `plugover label`, for devices Plugover gets wrong.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub label: Vec<Label>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Label {
    /// vendor:product.
    pub device: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<crate::catalog::Kind>,
}

impl Label {
    pub fn apply(labels: &[Label], id: &str, identity: &mut crate::catalog::Identity) {
        if let Some(l) = labels.iter().find(|l| l.device == id) {
            identity.relabel(l.name.as_deref(), l.kind);
        }
    }
}

/// Sets or, with neither a name nor a kind, removes the label of one model.
pub fn set_label(labels: &mut Vec<Label>, device: String, name: Option<String>, kind: Option<crate::catalog::Kind>) -> Result<()> {
    let name = name.map(|n| n.trim().to_string()).filter(|n| !n.is_empty());
    anyhow::ensure!(name.as_ref().is_none_or(|n| n.chars().count() <= 60), "keep the name under 60 characters");
    labels.retain(|l| l.device != device);
    if name.is_some() || kind.is_some() {
        labels.push(Label { device, name, kind });
    }
    Ok(())
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
    /// The relay has encrypted before (or said it can when pairing), so it must from now on.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub encrypts: bool,
    /// You allowed connecting without encryption, to a relay that cannot encrypt.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub unencrypted: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AutoRule {
    pub relay: String,
    /// vendor:product.
    pub device: String,
    /// What the device was called when the rule was made, to show it while it is unplugged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Only the device on this port (busid), for when several of the same model are plugged in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<String>,
}

impl AutoRule {
    pub fn matches(&self, relay: &str, device: &crate::proto::DeviceInfo) -> bool {
        self.relay == relay && self.device == device.id && self.port.as_ref().is_none_or(|p| *p == device.busid)
    }
}

/// Checks and normalizes a vendor:product id such as `046D:C24F`.
pub fn device_id(input: &str) -> Result<String> {
    let id = input.trim().to_lowercase();
    let ok = id.len() == 9 && id.as_bytes()[4] == b':' && id.bytes().enumerate().all(|(i, b)| i == 4 || b.is_ascii_hexdigit());
    anyhow::ensure!(ok, "{input:?} is not a USB id; they look like 046d:c24f (see `plugover status`)");
    Ok(id)
}

/// Turns one auto-connect rule on or off. A rule for any port replaces every rule for that model;
/// a rule for one port replaces the any-port rule and that port's, and keeps other ports' rules.
pub fn set_auto(rules: &mut Vec<AutoRule>, rule: AutoRule, on: bool) {
    rules.retain(|a| {
        let same = a.relay == rule.relay && a.device == rule.device;
        !same || (rule.port.is_some() && a.port.is_some() && a.port != rule.port)
    });
    if on {
        rules.push(rule);
    }
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
    let mut file = opts.open(&tmp)?;
    // `mode` only applies to a new file; one left over from an older run may be looser.
    #[cfg(unix)]
    file.set_permissions(std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
    file.write_all(toml::to_string_pretty(value)?.as_bytes())?;
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
                encrypts: true,
                unencrypted: false,
            }],
            auto: vec![AutoRule { relay: "ab".into(), device: "046d:c24f".into(), title: Some("Logitech G29".into()), port: None }],
            label: Vec::new(),
        };
        let mut cfg = cfg;
        set_label(&mut cfg.label, "0fd9:0080".into(), Some(" Stream Deck ".into()), None).unwrap();
        set_label(&mut cfg.label, "0fd9:0080".into(), None, Some(crate::catalog::Kind::Other)).unwrap();
        set_label(&mut cfg.label, "3299:0009".into(), Some("Mic".into()), Some(crate::catalog::Kind::Audio)).unwrap();
        set_label(&mut cfg.label, "3299:0009".into(), Some("  ".into()), None).unwrap();
        assert!(set_label(&mut cfg.label, "1111:2222".into(), Some("x".repeat(61)), None).is_err());
        assert_eq!(cfg.label, [Label { device: "0fd9:0080".into(), name: None, kind: Some(crate::catalog::Kind::Other) }], "a new label replaces the old one; an empty one removes it");
        save(&path, &cfg).unwrap();
        let back: HostConfig = load(&path).unwrap();
        assert_eq!((back.auto, back.label), (cfg.auto, cfg.label));
        assert_eq!(decode_key(&back.relay[0].key).unwrap(), [0u8; 32]);
        #[cfg(unix)]
        assert_eq!(std::os::unix::fs::PermissionsExt::mode(&fs::metadata(&path).unwrap().permissions()) & 0o777, 0o600);
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn auto_rules_per_port() {
        let rule = |port: Option<&str>| AutoRule { relay: "r".into(), device: "045e:028e".into(), title: None, port: port.map(Into::into) };
        let dev = |busid: &str| crate::proto::DeviceInfo {
            busid: busid.into(),
            id: "045e:028e".into(),
            identity: crate::catalog::Identity {
                title: "Pad".into(),
                name: "Pad".into(),
                maker: "Microsoft".into(),
                kind: crate::catalog::Kind::Gamepad,
                source: crate::catalog::Source::Device,
                blocked: None,
            },
            lent_to: None,
        };
        let mut rules = Vec::new();
        set_auto(&mut rules, rule(None), true);
        assert!(rules[0].matches("r", &dev("1-2")) && rules[0].matches("r", &dev("1-3")), "any port");
        set_auto(&mut rules, rule(Some("1-2")), true);
        set_auto(&mut rules, rule(Some("1-3")), true);
        assert_eq!(rules.len(), 2, "port rules replace the any-port rule and keep each other");
        assert!(!rules[0].matches("r", &dev("1-4")) && !rules[0].matches("x", &dev("1-2")));
        set_auto(&mut rules, rule(Some("1-2")), false);
        assert_eq!(rules.iter().map(|r| r.port.as_deref()).collect::<Vec<_>>(), [Some("1-3")]);
        set_auto(&mut rules, rule(None), false);
        assert!(rules.is_empty());
    }

    #[test]
    fn device_ids() {
        assert_eq!(device_id(" 046D:C24F ").unwrap(), "046d:c24f");
        assert!(device_id("046d-c24f").is_err());
        assert!(device_id("46d:c24f").is_err());
        assert!(device_id("046g:c24f").is_err());
    }

    #[test]
    fn share_list_withholds_the_rest() {
        let mut st = RelayState::default();
        assert_eq!(st.withheld("046d:c24f"), None, "no list shares everything");
        st.share = Some(vec!["046d:c24f".into()]);
        assert_eq!(st.withheld("046d:c24f"), None);
        assert!(st.withheld("045e:028e").is_some());
        let back: RelayState = toml::from_str(&toml::to_string_pretty(&st).unwrap()).unwrap();
        assert_eq!(back.share, st.share);
    }
}
