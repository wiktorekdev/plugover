//! Setup on Linux: systemd services, kernel modules and the firewall. Root steps go through
//! `sudo -n` after one `sudo -v` up front, so a password is asked once and never under a spinner.

use super::*;
use crate::term::{GOOD, cmd};
use anstream::println;
use std::os::unix::fs::MetadataExt;

const RELAY_BIN: &str = "/usr/local/bin/plugover";
/// Where distribution packages (AUR, .deb) put the binary; setup then uses it in place.
const PACKAGED: &str = "/usr/bin/plugover";
const RELAY_UNIT: &str = "/etc/systemd/system/plugover-relay.service";
const RELAY_STATE: &str = "/var/lib/plugover";
/// What setup opened in the firewall, so uninstall removes exactly that.
const FIREWALL_RECORD: &str = "/var/lib/plugover/firewall";
const MODULES: &str = "/etc/modules-load.d/plugover.conf";
const NFT_CONF: &str = "/etc/nftables.conf";
const VHCI: &str = "/sys/devices/platform/vhci_hcd.0";
const PORT: u16 = crate::proto::DEFAULT_PORT;

fn packaged() -> bool {
    is_me(Path::new(PACKAGED))
}

fn relay_bin() -> &'static str {
    if packaged() { PACKAGED } else { RELAY_BIN }
}

fn host_bin() -> PathBuf {
    if packaged() { PACKAGED.into() } else { crate::home().join(".local/bin/plugover") }
}

/// The binary an installed unit runs.
fn unit_bin(unit: &Path) -> Option<PathBuf> {
    let text = fs::read_to_string(unit).ok()?;
    let exec = text.lines().find_map(|l| l.strip_prefix("ExecStart="))?.split_whitespace().next()?;
    Some(PathBuf::from(exec.replace("%h", &crate::home().to_string_lossy())))
}

pub fn os_name() -> String {
    fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|t| t.lines().find_map(|l| l.strip_prefix("PRETTY_NAME=").map(|v| v.trim_matches('"').to_string())))
        .unwrap_or_else(|| "Linux".into())
}

/// Shows the service's log through journalctl.
pub fn logs(role: Role, follow: bool, lines: usize) -> Result<()> {
    let mut c = Command::new("journalctl");
    match role {
        Role::Relay => c.args(["-u", "plugover-relay"]),
        _ => c.args(["--user", "-u", "plugover-host"]),
    };
    c.args(["--no-pager", "-o", "short-iso", "-n", &lines.to_string()]);
    if follow {
        c.arg("-f");
    }
    let status = c.status().context("running journalctl")?;
    anyhow::ensure!(status.success(), "journalctl could not show the log (for the relay's, you may need sudo or the systemd-journal group)");
    Ok(())
}

/// Which roles are installed here.
pub fn installed() -> (bool, bool) {
    (Path::new(RELAY_UNIT).exists(), host_unit().exists())
}

pub fn is_packaged() -> bool {
    packaged()
}

fn user_units() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| crate::home().join(".config")).join("systemd/user")
}

fn host_unit() -> PathBuf {
    user_units().join("plugover-host.service")
}

fn plugin_vendor() -> PathBuf {
    crate::home().join(".config/punktfunk/plugins/vendor")
}

fn is_root() -> bool {
    fs::metadata("/proc/self").is_ok_and(|m| m.uid() == 0)
}

fn root(program: &str, args: &[&str]) -> Result<String> {
    if is_root() { exec(Command::new(program).args(args)) } else { exec(Command::new("sudo").arg("-n").arg(program).args(args)) }
}

fn root_write(path: &str, contents: &str) -> Result<()> {
    if let Some(dir) = Path::new(path).parent() {
        root("mkdir", &["-p", &dir.to_string_lossy()])?;
    }
    let mut tee = if is_root() { Command::new("tee") } else { Command::new("sudo") };
    if !is_root() {
        tee.args(["-n", "tee"]);
    }
    exec_with(tee.arg(path), Some(contents.as_bytes())).map(drop)
}

fn root_read(path: &str) -> Option<String> {
    fs::read_to_string(path).ok().or_else(|| root("cat", &[path]).ok())
}

pub fn prepare(_args: &Args) -> Result<()> {
    Ok(())
}

pub fn get_root() -> Result<()> {
    if is_root() {
        return Ok(());
    }
    probe("sudo", &["--version"]).context("this needs root and sudo is not installed; run setup as root")?;
    println!("\n  {}", paint(DIM, "Some steps need root; sudo may ask for your password."));
    let ok = Command::new("sudo").arg("-v").status().is_ok_and(|s| s.success());
    anyhow::ensure!(ok, "sudo was refused");
    Ok(())
}

fn has_module(name: &str) -> bool {
    Path::new("/sys/module").join(name.replace('-', "_")).exists() || probe("modinfo", &[name]).is_some()
}

#[derive(Clone, Copy, PartialEq)]
enum Firewall {
    None,
    Firewalld,
    Ufw,
    Nftables,
}

impl Firewall {
    fn detect() -> Self {
        let active = |unit: &str| Command::new("systemctl").args(["is-active", "-q", unit]).status().is_ok_and(|s| s.success());
        if active("firewalld") {
            Self::Firewalld
        } else if active("ufw") {
            Self::Ufw
        } else if active("nftables") {
            Self::Nftables
        } else {
            Self::None
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Firewalld => "firewalld",
            Self::Ufw => "ufw",
            Self::Nftables => "nftables",
        }
    }

    fn parse(name: &str) -> Self {
        [Self::Firewalld, Self::Ufw, Self::Nftables].into_iter().find(|f| f.name() == name).unwrap_or(Self::None)
    }
}

/// The local network, from the default route's interface.
fn guess_lan() -> String {
    let dev = probe("ip", &["route", "show", "default"]).and_then(|l| l.split_whitespace().skip_while(|w| *w != "dev").nth(1).map(str::to_string));
    dev.and_then(|d| probe("ip", &["-o", "-4", "route", "show", "dev", &d, "scope", "link"]))
        .and_then(|l| l.split_whitespace().next().map(str::to_string))
        .unwrap_or_else(|| "192.168.1.0/24".into())
}

pub fn detect() -> Result<Found> {
    let os = os_name();
    let systemd = probe("systemctl", &["--version"]).map(|l| l.split_whitespace().take(2).collect::<Vec<_>>().join(" "));
    let punktfunk = probe("punktfunk-host", &["--version"]).and_then(|l| l.split_whitespace().nth(1).map(str::to_string));
    let relay = unit_bin(Path::new(RELAY_UNIT)).and_then(|b| version_of(&b));
    let host = unit_bin(&host_unit()).and_then(|b| version_of(&b));

    let mut lines = vec![line(Mark::Ok, os, format!("{}{}", std::env::consts::ARCH, systemd.as_ref().map(|s| format!(" · {s}")).unwrap_or_default()))];
    if systemd.is_none() {
        bail!("Plugover runs as a systemd service, and systemd is not running here");
    }
    lines.push(match &punktfunk {
        Some(v) => line(Mark::Ok, format!("punktfunk-host {v}"), ""),
        None => line(Mark::Todo, "punktfunk-host", "not found"),
    });
    for (role, v) in [("relay", &relay), ("host", &host)] {
        if let Some(v) = v {
            lines.push(line(Mark::Ok, format!("Plugover {role} {v}"), "installed"));
        }
    }
    if relay.is_none() && host.is_none() {
        lines.push(line(Mark::Todo, "Plugover", "not installed"));
    }
    Ok(Found { lines, punktfunk: punktfunk.is_some(), relay, host })
}

pub fn plan(role: Role, found: &Found, ctx: &Ctx) -> Result<Plan> {
    match role {
        Role::Relay => plan_relay(found, ctx),
        Role::Host => plan_host(found),
        Role::Uninstall => plan_uninstall(found, ctx),
    }
}

fn install_line(installed: &Option<String>, what: &str, detail: &str) -> Line {
    match installed {
        Some(v) if v == VERSION => line(Mark::Todo, format!("Reinstall the {what} {VERSION}"), "already this version"),
        Some(v) => line(Mark::Todo, format!("Update the {what}"), format!("{v} → {VERSION}")),
        None => line(Mark::Todo, format!("Install the {what} {VERSION}"), detail),
    }
}

/// Copies this binary into place; nothing to do when it already runs from there.
fn install_self(target: &Path, as_root: bool) -> Result<()> {
    if is_me(target) {
        return Ok(());
    }
    let me = std::env::current_exe()?;
    let (me, target) = (me.to_string_lossy(), target.to_string_lossy());
    let args = ["-Dm755", me.as_ref(), target.as_ref()];
    if as_root { root("install", &args).map(drop) } else { exec(Command::new("install").args(args)).map(drop) }
}

fn load_module(name: &str) -> Result<()> {
    root("modprobe", &[name])?;
    let conf = fs::read_to_string(MODULES).unwrap_or_default();
    if !conf.lines().any(|l| l.trim() == name) {
        root_write(MODULES, &format!("{conf}{name}\n"))?;
    }
    Ok(())
}

// --- Relay -------------------------------------------------------------------------------------

const RELAY_SERVICE: &str = "[Unit]
Description=Plugover relay: lends USB devices to paired hosts
After=network-online.target
Wants=network-online.target

[Service]
ExecStart=@BIN@ relay
Restart=on-failure
RestartSec=2
StateDirectory=plugover
ProtectSystem=strict
ProtectHome=true
PrivateTmp=true
NoNewPrivileges=true
RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX AF_NETLINK

[Install]
WantedBy=multi-user.target
";

fn plan_relay(found: &Found, ctx: &Ctx) -> Result<Plan> {
    if !has_module("usbip-host") {
        bail!("this kernel has no usbip-host module, which the relay needs (some distros ship it in a separate linux-*-extra or usbip package)");
    }
    let mut lines = vec![install_line(&found.relay, "relay", &format!("{}, systemd service", relay_bin()))];
    let loaded = Path::new("/sys/bus/usb/drivers/usbip-host").is_dir();
    lines.push(if loaded { line(Mark::Ok, "usbip-host", "loaded") } else { line(Mark::Todo, "Load usbip-host", "now and at every boot") });
    let firewall = Firewall::detect();
    let lan = match (firewall, &ctx.args.lan) {
        (Firewall::None, _) => None,
        (_, Some(l)) => Some(l.clone()),
        _ => Some(ctx.ask("Let hosts in from which network?", &guess_lan())?),
    };
    match &lan {
        None => lines.push(line(Mark::Ok, "Firewall", format!("none running, port {PORT} is reachable"))),
        Some(l) => lines.push(line(Mark::Todo, format!("Open TCP {PORT} and mDNS in {}", firewall.name()), format!("for {l}"))),
    }
    let new_code = found.relay.is_none() || (ctx.interactive && !ctx.args.yes && ctx.confirm("Print a new pairing code at the end?", false)?);
    if new_code {
        lines.push(line(Mark::Todo, "Print a pairing code", "you type it on the host"));
    }

    let mut steps: Vec<(String, Action)> = vec![
        (format!("Installing plugover {VERSION}"), Box::new(|| install_self(Path::new(relay_bin()), true))),
        ("Loading usbip-host".into(), Box::new(|| load_module("usbip-host"))),
        ("Starting the relay service".into(), Box::new(start_relay)),
    ];
    if let Some(lan) = lan.clone() {
        steps.push((format!("Opening the firewall for {lan}"), Box::new(move || open_firewall(firewall, &lan))));
    }
    steps.push(("Checking that the relay answers".into(), Box::new(|| wait_for("the relay", relay_answers))));

    let after: Action = Box::new(move || {
        if new_code {
            let bin = relay_bin();
            let status = if is_root() { Command::new(bin).arg("code").status() } else { Command::new("sudo").args(["-n", bin, "code"]).status() };
            anyhow::ensure!(status.is_ok_and(|s| s.success()), "could not open pairing; run `sudo plugover code`");
        } else {
            ok("The relay is up to date", "pairings are kept");
        }
        println!("\n  {} on the host, open USB devices in the punktfunk console, or run {}.", paint(DIM, "Next:"), cmd("plugover pair"));
        Ok(())
    });
    Ok(Plan { lines, steps, root: true, after })
}

fn start_relay() -> Result<()> {
    // A relay started by hand would hold the port. The pattern cannot match sudo's own command line.
    let _ = root("pkill", &["-f", "^[^ ]*plugover relay"]);
    root_write(RELAY_UNIT, &RELAY_SERVICE.replace("@BIN@", relay_bin()))?;
    root("systemctl", &["daemon-reload"])?;
    root("systemctl", &["enable", "plugover-relay"])?;
    root("systemctl", &["restart", "plugover-relay"]).map(drop)
}

fn firewalld_rules(lan: &str) -> [String; 2] {
    [
        format!("rule family=ipv4 source address={lan} port port={PORT} protocol=tcp accept"),
        format!("rule family=ipv4 source address={lan} service name=mdns accept"),
    ]
}

fn open_firewall(firewall: Firewall, lan: &str) -> Result<()> {
    root("mkdir", &["-p", RELAY_STATE])?;
    root_write(FIREWALL_RECORD, &format!("{} {lan}\n", firewall.name()))?;
    match firewall {
        Firewall::None => {}
        Firewall::Firewalld => {
            for rule in firewalld_rules(lan) {
                root("firewall-cmd", &["-q", "--permanent", &format!("--add-rich-rule={rule}")])?;
            }
            root("firewall-cmd", &["-q", "--reload"])?;
        }
        Firewall::Ufw => {
            for (port, proto) in [(PORT.to_string(), "tcp"), ("5353".into(), "udp")] {
                root("ufw", &["allow", "from", lan, "to", "any", "port", &port, "proto", proto, "comment", "plugover"])?;
            }
        }
        Firewall::Nftables => open_nftables(lan)?,
    }
    Ok(())
}

fn open_nftables(lan: &str) -> Result<()> {
    let chain = root("nft", &["list", "chain", "inet", "filter", "input"])
        .context("there is no nftables 'inet filter input' chain; open TCP 7275 and UDP 5353 yourself")?;
    let rules = [format!("tcp dport {PORT}"), "udp dport 5353".to_string()];
    for rule in &rules {
        if !chain.contains(&format!("{rule} ip saddr {lan} accept comment \"plugover\"")) {
            let mut args = vec!["insert", "rule", "inet", "filter", "input"];
            args.extend(rule.split(' '));
            args.extend(["ip", "saddr", lan, "accept", "comment", "\"plugover\""]);
            root("nft", &args)?;
        }
    }
    // The live rules are gone after a reboot; the config keeps them.
    let Some(conf) = root_read(NFT_CONF) else { return Ok(()) };
    if conf.contains("comment \"plugover\"") {
        return Ok(());
    }
    let added = with_nft_rules(&conf, lan).context("could not find the input hook in /etc/nftables.conf")?;
    root_write(&format!("{NFT_CONF}.plugover-backup"), &conf)?;
    root_write(NFT_CONF, &added)?;
    if let Err(e) = root("nft", &["-c", "-f", NFT_CONF]) {
        root_write(NFT_CONF, &conf)?;
        return Err(e.context("the changed /etc/nftables.conf did not check out, so it was left as it was"));
    }
    Ok(())
}

/// The config with our two rules right after the first input hook, so nothing earlier in the chain
/// can drop the traffic.
fn with_nft_rules(conf: &str, lan: &str) -> Option<String> {
    let mut out = String::new();
    let mut done = false;
    for l in conf.lines() {
        out.push_str(l);
        out.push('\n');
        if !done && l.contains("hook input") {
            out.push_str(&format!("    tcp dport {PORT} ip saddr {lan} accept comment \"plugover\"\n"));
            out.push_str(&format!("    udp dport 5353 ip saddr {lan} accept comment \"plugover\"\n"));
            done = true;
        }
    }
    done.then_some(out)
}

fn close_firewall() -> Result<()> {
    let Some(record) = root_read(FIREWALL_RECORD) else { return Ok(()) };
    let mut words = record.split_whitespace();
    let (firewall, lan) = (Firewall::parse(words.next().unwrap_or_default()), words.next().unwrap_or_default());
    match firewall {
        Firewall::None => {}
        Firewall::Firewalld => {
            for rule in firewalld_rules(lan) {
                let _ = root("firewall-cmd", &["-q", "--permanent", &format!("--remove-rich-rule={rule}")]);
            }
            root("firewall-cmd", &["-q", "--reload"])?;
        }
        Firewall::Ufw => {
            let status = root("ufw", &["status", "numbered"])?;
            // Highest first, so deleting one does not renumber the rest.
            let mut numbers: Vec<u32> = status
                .lines()
                .filter(|l| l.trim_end().ends_with("# plugover"))
                .filter_map(|l| l.trim_start_matches('[').split(']').next()?.trim().parse().ok())
                .collect();
            numbers.sort_unstable_by(|a, b| b.cmp(a));
            for n in numbers {
                root("ufw", &["--force", "delete", &n.to_string()])?;
            }
        }
        Firewall::Nftables => {
            if let Ok(chain) = root("nft", &["-a", "list", "chain", "inet", "filter", "input"]) {
                for l in chain.lines().filter(|l| l.contains("comment \"plugover\"")) {
                    if let Some(handle) = l.rsplit("handle ").next() {
                        root("nft", &["delete", "rule", "inet", "filter", "input", "handle", handle.trim()])?;
                    }
                }
            }
            if let Some(conf) = root_read(NFT_CONF) {
                let kept: String = conf.lines().filter(|l| !l.contains("comment \"plugover\"")).map(|l| format!("{l}\n")).collect();
                root_write(NFT_CONF, &kept)?;
            }
        }
    }
    Ok(())
}

// --- Host --------------------------------------------------------------------------------------

const HOST_SERVICE: &str = "[Unit]
Description=Plugover host daemon: plugs relay devices in while you stream
After=network-online.target

[Service]
ExecStart=@BIN@ host
Restart=on-failure
RestartSec=2

[Install]
WantedBy=default.target
";

fn user_in_group(group: &str) -> bool {
    probe("id", &["-nG"]).is_some_and(|g| g.split_whitespace().any(|x| x == group))
}

fn plan_host(found: &Found) -> Result<Plan> {
    anyhow::ensure!(!is_root(), "install the host as your normal user, not root: it runs next to punktfunk in your session");
    anyhow::ensure!(has_module("vhci-hcd"), "this kernel has no vhci-hcd module, which the host needs");
    let mut lines = vec![install_line(&found.host, "host", &format!("{}, systemd user service", host_bin().display()))];
    let loaded = Path::new(VHCI).is_dir();
    lines.push(if loaded { line(Mark::Ok, "vhci-hcd", "loaded") } else { line(Mark::Todo, "Load vhci-hcd", "now and at every boot") });
    let plugin = found.punktfunk && !PLUGIN.is_empty();
    lines.push(match (found.punktfunk, PLUGIN.is_empty()) {
        (true, false) => line(Mark::Todo, "Add the Plugover page to the punktfunk console", "and turn its plugin runner on"),
        (true, true) => line(Mark::Warn, "This build has no console plugin inside", "release builds from scripts/build.sh do"),
        (false, _) => line(Mark::Warn, "No punktfunk-host here", "run setup again after installing it to add the console plugin"),
    });
    // punktfunk-host's udev rule lets its group plug devices in.
    let writable = fs::OpenOptions::new().write(true).open(Path::new(VHCI).join("attach")).is_ok();
    let join_group = loaded && !writable && probe("getent", &["group", "punktfunk"]).is_some() && !user_in_group("punktfunk");
    if join_group {
        lines.push(line(Mark::Todo, "Add you to the punktfunk group", "so you may plug devices in; takes effect at your next login"));
    } else if loaded && !writable {
        lines.push(line(Mark::Warn, "You may not plug devices in", format!("{VHCI}/attach is not writable for you")));
    }

    let mut steps: Vec<(String, Action)> = Vec::new();
    if !loaded {
        steps.push(("Loading vhci-hcd".into(), Box::new(|| load_module("vhci-hcd"))));
    }
    if join_group {
        let user = std::env::var("USER").unwrap_or_default();
        steps.push(("Adding you to the punktfunk group".into(), Box::new(move || root("usermod", &["-aG", "punktfunk", &user]).map(drop))));
    }
    steps.push((format!("Installing plugover {VERSION}"), Box::new(|| install_self(&host_bin(), false))));
    steps.push(("Starting the host service".into(), Box::new(start_host)));
    if plugin {
        steps.push(("Adding the console plugin".into(), Box::new(add_plugin)));
    }
    steps.push((
        "Checking that the host daemon answers".into(),
        Box::new(|| wait_for("the host daemon", || crate::hostd::call(&crate::endpoint(), &crate::hostd::Request::Status).is_ok())),
    ));

    let after: Action = Box::new(|| {
        crate::doctor::checks(&crate::endpoint(), &crate::config_dir().join("host.toml"), &crate::relay_state());
        let bin_dir = host_bin().parent().map(Path::to_path_buf).unwrap_or_default();
        if !std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).any(|p| p == bin_dir) {
            warn(&format!("{} is not on your PATH", bin_dir.display()), "add it to run plugover from a terminal", None);
        }
        next_steps_host();
        Ok(())
    });
    Ok(Plan { lines, root: !loaded || join_group, steps, after })
}

fn start_host() -> Result<()> {
    fs::create_dir_all(user_units())?;
    let bin = if packaged() { PACKAGED.to_string() } else { "%h/.local/bin/plugover".into() };
    fs::write(host_unit(), HOST_SERVICE.replace("@BIN@", &bin))?;
    let user = |args: &[&str]| exec(Command::new("systemctl").arg("--user").args(args)).map(drop);
    user(&["daemon-reload"])?;
    user(&["enable", "plugover-host"])?;
    user(&["restart", "plugover-host"])
}

fn add_plugin() -> Result<()> {
    // The plugin runner re-resolves packages from inside its own tree, so the package lives there.
    let path = plugin_vendor().join(plugin_file());
    fs::create_dir_all(plugin_vendor())?;
    fs::write(&path, PLUGIN)?;
    exec(Command::new("punktfunk-host").args(["plugins", "add"]).arg(&path).arg("--allow-public-registry"))?;
    exec(Command::new("punktfunk-host").args(["plugins", "enable"])).map(drop)
}

// --- Uninstall ---------------------------------------------------------------------------------

fn plan_uninstall(found: &Found, ctx: &Ctx) -> Result<Plan> {
    anyhow::ensure!(found.relay.is_some() || found.host.is_some(), "Plugover is not installed here");
    let mut lines = Vec::new();
    if found.relay.is_some() {
        lines.push(line(Mark::Todo, "Remove the relay", "service, binary, module setting and firewall rules"));
    }
    if found.host.is_some() {
        lines.push(line(Mark::Todo, "Remove the host", "service, binary and console plugin"));
    }
    let purge = ctx.args.purge || (!ctx.args.yes && ctx.interactive && ctx.confirm("Also delete pairings and settings?", false)?);
    if purge {
        lines.push(line(Mark::Todo, "Delete pairings and settings", "you pair again after reinstalling"));
    }
    let mut steps: Vec<(String, Action)> = Vec::new();
    if found.relay.is_some() {
        steps.push(("Removing the relay".into(), Box::new(move || remove_relay(purge))));
    }
    if found.host.is_some() {
        steps.push(("Removing the host".into(), Box::new(move || remove_host(purge))));
    }
    let after: Action = Box::new(|| {
        println!("  {} {}", paint(GOOD.bold(), "Plugover is gone."), paint(DIM, "The usbip kernel modules stay; they come with your kernel."));
        Ok(())
    });
    Ok(Plan { lines, steps, root: found.relay.is_some(), after })
}

fn remove_relay(purge: bool) -> Result<()> {
    // A package's binary stays for the package manager to remove.
    let ours = unit_bin(Path::new(RELAY_UNIT)).is_some_and(|b| b == Path::new(RELAY_BIN));
    let _ = root("systemctl", &["disable", "--now", "plugover-relay"]);
    close_firewall()?;
    root("rm", &["-f", RELAY_UNIT, MODULES])?;
    if ours {
        root("rm", &["-f", RELAY_BIN])?;
    }
    root("systemctl", &["daemon-reload"])?;
    if purge {
        root("rm", &["-rf", RELAY_STATE])?;
    } else {
        root("rm", &["-f", FIREWALL_RECORD])?;
    }
    Ok(())
}

fn remove_host(purge: bool) -> Result<()> {
    let bin = unit_bin(&host_unit());
    let _ = exec(Command::new("systemctl").args(["--user", "disable", "--now", "plugover-host"]));
    let _ = fs::remove_file(host_unit());
    if let Some(b) = bin.filter(|b| b != Path::new(PACKAGED)) {
        let _ = fs::remove_file(b);
    }
    exec(Command::new("systemctl").args(["--user", "daemon-reload"]))?;
    if probe("punktfunk-host", &["--version"]).is_some() {
        let _ = exec(Command::new("punktfunk-host").args(["plugins", "remove", "punktfunk-plugin-plugover"]));
    }
    for e in fs::read_dir(plugin_vendor()).into_iter().flatten().flatten() {
        if e.file_name().to_string_lossy().starts_with("punktfunk-plugin-plugover-") {
            let _ = fs::remove_file(e.path());
        }
    }
    if purge {
        let _ = fs::remove_dir_all(crate::config_dir());
        if let Some(state) = crate::endpoint().parent() {
            let _ = fs::remove_dir_all(state);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nft_rules_go_right_after_the_input_hook() {
        let conf = "table inet filter {\n  chain input {\n    type filter hook input priority 0; policy drop;\n    ct state established accept\n  }\n}\n";
        let out = with_nft_rules(conf, "192.168.1.0/24").unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines[3].contains("tcp dport 7275 ip saddr 192.168.1.0/24 accept comment \"plugover\""), "{out}");
        assert!(lines[4].contains("udp dport 5353"));
        assert_eq!(lines[5].trim(), "ct state established accept");
        assert!(with_nft_rules("table inet filter {}\n", "x").is_none());
    }
}
