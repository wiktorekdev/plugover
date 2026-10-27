// Relay-side code only runs on Linux; on Windows it is compiled for the shared types and left unused.
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

mod auth;
mod catalog;
mod discovery;
mod host;
mod hostd;
mod net;
mod proto;
#[cfg(target_os = "linux")]
mod relay;
mod state;
mod sysfs;
mod usbip;
mod vhci;

use anyhow::Result;
use clap::{Parser, Subcommand};
use hostd::Request;
use serde::Serialize;
use serde_json::Value;
use std::{
    env,
    path::{Path, PathBuf},
};

const SYS: &str = "/sys";
const RELAY_STATE: &str = "/var/lib/plugover/relay.toml";

#[derive(Parser)]
#[command(version, about = "Hands USB devices plugged into one machine to a punktfunk host")]
struct Cli {
    /// Where the host daemon listens: its socket on Linux, its token file on Windows.
    #[arg(long, global = true, default_value_os_t = endpoint())]
    socket: PathBuf,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// List the USB devices on this machine.
    Devices {
        #[arg(long)]
        json: bool,
    },
    /// Lend devices to paired hosts (run where the devices are plugged in, as root).
    Relay {
        #[arg(long, default_value_t = proto::DEFAULT_PORT)]
        port: u16,
        #[arg(long, default_value = RELAY_STATE)]
        state: PathBuf,
    },
    /// Print a code that pairs one host with this relay (run on the relay, as root).
    Code {
        #[arg(long, default_value = RELAY_STATE)]
        state: PathBuf,
    },
    /// Run the host daemon (on the punktfunk host).
    Host {
        #[arg(long, default_value_os_t = config_dir().join("host.toml"))]
        config: PathBuf,
    },
    /// Show paired relays and their devices.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Find relays on the network.
    Discover,
    /// Pair with a relay using the code from `plugover code` on it. Without an address, pairs
    /// with the only relay found on the network; without --code, asks for it.
    Pair {
        address: Option<String>,
        #[arg(long)]
        code: Option<String>,
    },
    /// Forget a paired relay.
    Forget { relay: String },
    /// Pass a device (vendor:product) through whenever a stream runs.
    Auto {
        device: String,
        #[arg(value_parser = ["on", "off"])]
        state: String,
        #[arg(long)]
        relay: Option<String>,
    },
    /// Tell the daemon a stream started or stopped (the punktfunk plugin does this for you).
    Session {
        #[arg(value_parser = ["start", "stop"])]
        state: String,
    },
    /// Plug a device from the relay in now.
    Attach {
        busid: String,
        #[arg(long)]
        relay: Option<String>,
    },
    /// Unplug a device plugged in with `attach`.
    Detach {
        busid: String,
        #[arg(long)]
        relay: Option<String>,
    },
}

#[cfg(not(windows))]
fn home() -> PathBuf {
    env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

/// The Windows daemon runs as SYSTEM at boot, so it keeps everything machine-wide.
#[cfg(windows)]
fn program_data() -> PathBuf {
    env::var_os("ProgramData").map(PathBuf::from).unwrap_or_else(|| r"C:\ProgramData".into()).join("Plugover")
}

#[cfg(windows)]
fn config_dir() -> PathBuf {
    program_data()
}

#[cfg(not(windows))]
fn config_dir() -> PathBuf {
    env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| home().join(".config")).join("plugover")
}

/// Its own folder: the installer lets the punktfunk plugin (LOCAL SERVICE) read this one, not the
/// keys next to it.
#[cfg(windows)]
fn endpoint() -> PathBuf {
    program_data().join("api").join("api-token")
}

/// Outside /run on purpose: the punktfunk plugin sandbox can be given this directory.
#[cfg(not(windows))]
fn endpoint() -> PathBuf {
    env::var_os("XDG_STATE_HOME").map(PathBuf::from).unwrap_or_else(|| home().join(".local/state")).join("plugover/host.sock")
}

#[derive(Serialize)]
struct DeviceView {
    busid: String,
    id: String,
    driver: Option<String>,
    #[serde(flatten)]
    identity: catalog::Identity,
}

fn devices(json: bool) -> Result<()> {
    let ids = catalog::UsbIds::load();
    let views: Vec<DeviceView> = sysfs::list(Path::new(SYS))?
        .iter()
        .map(|d| DeviceView {
            busid: d.busid.clone(),
            id: format!("{:04x}:{:04x}", d.vid, d.pid),
            driver: d.driver.clone(),
            identity: catalog::identify(d, ids.as_ref()),
        })
        .collect();
    if json {
        println!("{}", serde_json::to_string_pretty(&views)?);
        return Ok(());
    }
    for v in &views {
        let i = &v.identity;
        let kind = match &i.blocked {
            Some(why) => format!("never shared ({why})"),
            None => i.kind.label().to_string(),
        };
        println!("{:<10} {:<10} {:<30} {}", v.busid, v.id, kind, i.title);
    }
    Ok(())
}

fn print_status(v: &Value) {
    println!("{} · stream {}", v["host"].as_str().unwrap_or("?"), if v["session"] == true { "running" } else { "not running" });
    let relays = v["relays"].as_array().cloned().unwrap_or_default();
    if relays.is_empty() {
        println!("No relay paired yet. Run `plugover pair`.");
    }
    for r in relays {
        let online = if r["online"] == true { "online" } else { "offline" };
        println!("\n{} ({}) {online}", r["name"].as_str().unwrap_or("?"), r["address"].as_str().unwrap_or("?"));
        for d in r["devices"].as_array().into_iter().flatten().filter(|d| d["blocked"].is_null()) {
            let auto = if d["auto"] == true { "auto" } else { "" };
            println!(
                "  {:<8} {:<10} {:<34} {:<9} {auto}",
                d["busid"].as_str().unwrap_or(""),
                d["id"].as_str().unwrap_or(""),
                d["title"].as_str().unwrap_or(""),
                d["state"].as_str().unwrap_or("")
            );
        }
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let sys = PathBuf::from(SYS);
    let call = |req: Request| hostd::call(&cli.socket, &req);
    let show = |v: Value| println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
    match cli.cmd {
        Cmd::Devices { json } => devices(json)?,
        #[cfg(target_os = "linux")]
        Cmd::Relay { port, state } => std::sync::Arc::new(relay::Relay::open(sys, state)?).serve(port)?,
        #[cfg(target_os = "linux")]
        Cmd::Code { state } => {
            let code = relay::open_pairing(&state)?;
            println!("Pairing code for {}: {} {}", state::hostname(), &code[..3], &code[3..]);
            println!(
                "Type it in the punktfunk console under USB devices, or run `plugover pair` on the host. It works once, for {} minutes.",
                relay::PAIR_MINUTES
            );
        }
        #[cfg(not(target_os = "linux"))]
        Cmd::Relay { .. } | Cmd::Code { .. } => anyhow::bail!("the relay runs on Linux only; this machine can be a host"),
        Cmd::Host { config } => hostd::Daemon::open(sys, config)?.run(&cli.socket)?,
        Cmd::Status { json } => {
            let v = call(Request::Status)?;
            if json { show(v) } else { print_status(&v) }
        }
        Cmd::Discover => show(call(Request::Discover)?),
        Cmd::Pair { address, code } => {
            let address = match address {
                Some(a) => a,
                None => {
                    let found = discovery::browse(std::time::Duration::from_secs(3))?;
                    let [only] = found.as_slice() else {
                        anyhow::bail!("found {} relays; pass an address (see `plugover discover`)", found.len());
                    };
                    discovery::reachable(&only.addresses)
                        .ok_or_else(|| anyhow::anyhow!("{} was announced but does not answer", only.name))?
                        .to_string()
                }
            };
            let code = match code {
                Some(c) => c,
                None => {
                    print!("Code from `sudo plugover code` on the relay ({address}): ");
                    std::io::Write::flush(&mut std::io::stdout())?;
                    let mut line = String::new();
                    std::io::stdin().read_line(&mut line)?;
                    line
                }
            };
            let v = call(Request::Pair { address, code })?;
            println!("Paired with {}.", v["relay"]["name"].as_str().unwrap_or("the relay"));
        }
        Cmd::Forget { relay } => show(call(Request::Forget { relay })?),
        Cmd::Auto { device, state, relay } => show(call(Request::Auto { relay, device, on: state == "on" })?),
        Cmd::Session { state } => show(call(Request::Session { active: state == "start" })?),
        Cmd::Attach { busid, relay } => show(call(Request::Attach { relay, busid })?),
        Cmd::Detach { busid, relay } => show(call(Request::Detach { relay, busid })?),
    }
    Ok(())
}
