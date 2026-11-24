//! Sunshine and the hosts built from it (Apollo, Vibeshine, Vibepollo, Foundation Sunshine,
//! LuminalShine, Polaris, Hermes, SolarFlare and others). All of them run the `global_prep_cmd`
//! entries in their config file when a game starts and ends, before the game launches, so one hook
//! there tells the host daemon about streams in every one of them, early enough that devices are
//! plugged in before the game looks for them. Polling them is no substitute: Apollo, Hermes and
//! Polaris tell only paired clients which game runs.

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
};

/// What a hook command passes to `plugover session`, to find its entries again.
const MARK: &str = "session start --hook";
/// The port Sunshine and its forks listen on unless their config says otherwise.
const DEFAULT_PORT: u16 = 47989;

/// One installed host's config file.
#[derive(Debug, Clone, PartialEq)]
pub struct Host {
    /// Lowercase name of its config folder (sunshine, apollo, polaris); also the session source.
    pub name: String,
    pub config: PathBuf,
    /// The Flatpak build, whose commands run inside its sandbox.
    pub flatpak: bool,
}

impl Host {
    pub fn title(&self) -> String {
        if self.flatpak {
            return "Sunshine (Flatpak)".into();
        }
        let mut c = self.name.chars();
        c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
    }
}

/// The installed hosts, found by their config files.
pub fn find() -> Vec<Host> {
    let mut hosts = Vec::new();
    #[cfg(windows)]
    {
        // Each fork installs into its own folder, with its config in config\sunshine.conf.
        let programs = std::env::var_os("ProgramFiles").map(PathBuf::from).unwrap_or_else(|| r"C:\Program Files".into());
        for dir in fs::read_dir(programs).into_iter().flatten().flatten() {
            let config = dir.path().join(r"config\sunshine.conf");
            if config.is_file() {
                hosts.push(Host { name: dir.file_name().to_string_lossy().to_lowercase(), config, flatpak: false });
            }
        }
    }
    #[cfg(not(windows))]
    {
        let config_home = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| crate::home().join(".config"));
        // Each fork keeps its own folder; most name the file after themselves, some keep sunshine.conf.
        for name in ["sunshine", "apollo", "vibeshine", "polaris", "hermes"] {
            let dir = config_home.join(name);
            if let Some(config) = [format!("{name}.conf"), "sunshine.conf".into()].iter().map(|f| dir.join(f)).find(|p| p.is_file()) {
                hosts.push(Host { name: name.into(), config, flatpak: false });
            }
        }
        // Its config stays behind when the Flatpak is uninstalled, so it counts only while installed.
        const APP: &str = "dev.lizardbyte.app.Sunshine";
        let flatpak = crate::home().join(".var/app").join(APP).join("config/sunshine/sunshine.conf");
        let installed = [crate::home().join(".local/share/flatpak/app"), PathBuf::from("/var/lib/flatpak/app")].iter().any(|d| d.join(APP).is_dir());
        if installed && flatpak.is_file() {
            hosts.push(Host { name: "sunshine-flatpak".into(), config: flatpak, flatpak: true });
        }
    }
    hosts
}

/// The port the host listens on: `port` in its config, or the default.
pub fn port(conf: &str) -> u16 {
    conf.lines().filter_map(setting).find(|(k, _)| *k == "port").and_then(|(_, v)| v.parse().ok()).unwrap_or(DEFAULT_PORT)
}

/// `name = value` from one config line, if it is one. As in Sunshine's parser, a `#` anywhere
/// starts a comment, even inside a value.
fn setting(line: &str) -> Option<(&str, &str)> {
    let line = line.split('#').next().unwrap_or_default();
    let (k, v) = line.split_once('=')?;
    let k = k.trim();
    (!k.is_empty()).then(|| (k, v.trim()))
}

/// Whether the config has Plugover's hook.
pub fn hooked(conf: &str) -> bool {
    conf.lines().filter_map(setting).any(|(k, v)| k == "global_prep_cmd" && v.contains(MARK))
}

/// The prep command entry that tells the host daemon about this host's streams.
fn entry(host: &Host, bin: &Path, port: u16) -> Value {
    let bin = bin.to_string_lossy();
    let mut exe = if bin.contains(' ') { format!("\"{bin}\"") } else { bin.into_owned() };
    if host.flatpak {
        exe = format!("flatpak-spawn --host {exe}");
    }
    let source = &host.name;
    json!({
        "do": format!("{exe} {MARK} --source {source} --port {port}"),
        "undo": format!("{exe} session stop --hook --source {source}"),
        // On Windows only an elevated command may reach the host daemon.
        "elevated": cfg!(windows),
    })
}

/// The config with Plugover's entry in `global_prep_cmd` replaced by `entry`, or removed with
/// None. Other lines stay as they were; other prep commands keep their meaning, though their keys
/// may come out in another order.
fn with_entry(conf: &str, entry: Option<Value>) -> Result<String> {
    let mut found = false;
    let mut out: Vec<String> = Vec::new();
    for line in conf.lines() {
        match setting(line) {
            Some(("global_prep_cmd", value)) if !found => {
                found = true;
                let mut cmds: Vec<Value> = serde_json::from_str(value).context("global_prep_cmd is not a JSON list; Plugover leaves it alone")?;
                cmds.retain(|c| !c["do"].as_str().is_some_and(|d| d.contains(MARK)));
                cmds.extend(entry.clone());
                out.push(format!("global_prep_cmd = {}", Value::Array(cmds)));
            }
            Some(("global_prep_cmd", _)) => bail!("global_prep_cmd is set twice; Plugover leaves it alone"),
            _ => out.push(line.to_string()),
        }
    }
    let added = !found && entry.is_some();
    if let (true, Some(e)) = (added, entry) {
        out.push(format!("global_prep_cmd = {}", Value::Array(vec![e])));
    }
    let mut text = out.join("\n");
    if conf.ends_with('\n') || added {
        text.push('\n');
    }
    Ok(text)
}

/// Adds or removes the hook; true if the file changed. The first change keeps a copy of the file
/// as it was next to it. Running hosts read the file again only when they restart.
pub fn hook(host: &Host, bin: &Path, on: bool) -> Result<bool> {
    let conf = fs::read_to_string(&host.config).with_context(|| format!("reading {}", host.config.display()))?;
    let entry = on.then(|| entry(host, bin, port(&conf)));
    let new = with_entry(&conf, entry)?;
    if new == conf {
        return Ok(false);
    }
    let backup = host.config.with_extension("conf.before-plugover");
    if !backup.exists() {
        fs::copy(&host.config, &backup).with_context(|| format!("keeping a copy as {}", backup.display()))?;
    }
    let tmp = host.config.with_extension("conf.plugover-tmp");
    fs::write(&tmp, new)?;
    fs::set_permissions(&tmp, fs::metadata(&host.config)?.permissions())?;
    fs::rename(&tmp, &host.config).with_context(|| format!("writing {}", host.config.display()))?;
    Ok(true)
}

/// Tells the host daemon about a hooked host before its first stream, so devices in auto mode
/// wait for streams right away.
pub fn announce(host: &Host) {
    let Ok(conf) = fs::read_to_string(&host.config) else { return };
    let req = crate::hostd::Request::Session { active: false, source: host.name.clone(), ttl: None, port: Some(port(&conf)) };
    let _ = crate::hostd::call(&crate::endpoint(), &req);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host() -> Host {
        Host { name: "sunshine".into(), config: PathBuf::from("/x/sunshine.conf"), flatpak: false }
    }

    #[test]
    fn the_hook_joins_other_prep_commands_and_leaves_with_nothing_else() {
        let bin = Path::new("/home/me/.local/bin/plugover");
        let e = || Some(entry(&host(), bin, 47989));
        let mine = r#"{"do":"/home/me/.local/bin/plugover session start --hook --source sunshine --port 47989","elevated":false,"undo":"/home/me/.local/bin/plugover session stop --hook --source sunshine"}"#;

        // No prep commands yet: one line is added, and removing it leaves an empty list.
        let conf = "sunshine_name = rig\n# a comment = here\n";
        let on = with_entry(conf, e()).unwrap();
        assert_eq!(on, format!("sunshine_name = rig\n# a comment = here\nglobal_prep_cmd = [{mine}]\n"));
        assert!(hooked(&on) && !hooked(conf));
        assert_eq!(with_entry(&on, e()).unwrap(), on, "adding twice changes nothing");
        assert_eq!(with_entry(&on, None).unwrap(), "sunshine_name = rig\n# a comment = here\nglobal_prep_cmd = []\n");

        // Someone else's command stays, in front of ours.
        let theirs = r#"global_prep_cmd = [{"do":"setres 1920","undo":"setres back","elevated":false}]"#;
        let on = with_entry(&format!("port = 48000\n{theirs}\n"), Some(entry(&host(), bin, 48000))).unwrap();
        assert!(on.starts_with("port = 48000\nglobal_prep_cmd = [{\"do\":\"setres 1920\""), "{on}");
        assert!(on.contains("--port 48000") && port(&on) == 48000);
        let off = with_entry(&on, None).unwrap();
        let list = |c: &str| serde_json::from_str::<Value>(setting(c.lines().nth(1).unwrap()).unwrap().1).unwrap();
        assert_eq!(list(&off), list(&format!("port = 48000\n{theirs}")), "theirs is back as it was");
        assert_eq!(with_entry("a = b", None).unwrap(), "a = b", "nothing to remove changes nothing");

        assert!(with_entry("global_prep_cmd = not json\n", e()).is_err(), "a list it cannot read is left alone");
        assert!(with_entry("global_prep_cmd = []\nglobal_prep_cmd = []\n", e()).is_err());
        assert_eq!(port("sunshine_name = rig"), 47989);
        assert_eq!(port("port = 48000  # moved off the default\n"), 48000);
        assert_eq!(port("# port = 48000\n"), 47989);
    }

    #[test]
    fn flatpak_and_spaces_are_quoted_for_the_shell() {
        let flatpak = Host { flatpak: true, name: "sunshine-flatpak".into(), ..host() };
        let e = entry(&flatpak, Path::new("/opt/my apps/plugover"), 47989);
        assert_eq!(e["do"], "flatpak-spawn --host \"/opt/my apps/plugover\" session start --hook --source sunshine-flatpak --port 47989");
    }

    #[test]
    fn hooking_a_real_file_keeps_a_copy_once() {
        let dir = std::env::temp_dir().join(format!("plugover-sunshine-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let h = Host { config: dir.join("sunshine.conf"), ..host() };
        fs::write(&h.config, "sunshine_name = rig\n").unwrap();
        let bin = Path::new("/usr/bin/plugover");
        assert!(hook(&h, bin, true).unwrap());
        assert!(!hook(&h, bin, true).unwrap(), "already there");
        assert!(hooked(&fs::read_to_string(&h.config).unwrap()));
        assert!(hook(&h, bin, false).unwrap());
        assert_eq!(fs::read_to_string(dir.join("sunshine.conf.before-plugover")).unwrap(), "sunshine_name = rig\n");
        let _ = fs::remove_dir_all(dir);
    }
}
