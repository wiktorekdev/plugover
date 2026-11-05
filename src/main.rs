// Linux-only code is compiled on Windows for the shared types and left unused.
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

mod api;
mod auth;
mod catalog;
mod cli;
mod discovery;
mod doctor;
mod host;
mod hostd;
mod net;
mod proto;
mod relay;
mod setup;
mod state;
mod sysfs;
mod term;
mod tls;
mod usbip;
mod usbipd;
mod vhci;

use anstream::{eprintln, println};
use anyhow::Result;
use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use hostd::Request;
use std::{env, path::PathBuf, process::ExitCode};
use cli::{ask, choose_relay, print_code, print_discovered, print_hosts, print_share, print_status};
use term::{ACCENT, BAD, BOLD, DIM, GOOD, cmd, paint};

const SYS: &str = "/sys";
#[cfg(windows)]
const SUDO: &str = "run it again as administrator";
#[cfg(not(windows))]
const SUDO: &str = "run it again with sudo";

#[derive(Parser)]
#[command(version, styles = term::CLAP, about = "Hands USB devices plugged into one machine to a punktfunk host")]
struct Cli {
    /// Where the host daemon listens: its socket on Linux, its token file on Windows.
    #[arg(long, global = true, default_value_os_t = endpoint())]
    socket: PathBuf,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Install, update or remove Plugover on this computer, as the relay or the host.
    Setup(setup::Args),
    /// Show paired relays and their devices (on the host).
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Find relays on the network.
    Discover,
    /// Pair with a relay using the code from `plugover code` on it. Without an address, finds
    /// the relay on the network; without --code, asks for it.
    Pair {
        /// The relay's IP address or name; port 7275 unless you add one.
        address: Option<String>,
        /// The six digits `plugover code` printed on the relay.
        #[arg(long)]
        code: Option<String>,
    },
    /// Forget a paired relay.
    Forget {
        /// Its name or id, see `plugover status`.
        relay: String,
    },
    /// Connect a relay's devices without encryption, for a relay whose Plugover is too old to
    /// encrypt. Anyone on the network could then watch them, or fake input.
    Unencrypted {
        /// Its name or id, see `plugover status`.
        relay: String,
        #[arg(value_name = "on|off", value_parser = ["on", "off"])]
        state: String,
    },
    /// Plug a device in whenever a stream runs.
    Auto {
        /// vendor:product, see `plugover status`.
        device: String,
        #[arg(value_name = "on|off", value_parser = ["on", "off"])]
        state: String,
        /// Which relay, by name or id; needed only when several are paired.
        #[arg(long)]
        relay: Option<String>,
        /// Only the one on this port (busid, see `plugover status`), when several of the same
        /// model are plugged in.
        #[arg(long)]
        port: Option<String>,
    },
    /// Name a device model or correct its kind, when Plugover gets it wrong.
    Label {
        /// vendor:product, see `plugover status`.
        device: String,
        /// What the console and `plugover status` call it.
        #[arg(long)]
        name: Option<String>,
        /// What kind of device it is (sets its icon).
        #[arg(long, value_enum)]
        kind: Option<catalog::Kind>,
        /// Go back to what Plugover detects.
        #[arg(long, conflicts_with_all = ["name", "kind"])]
        reset: bool,
    },
    /// Plug a device from the relay in now.
    Attach {
        /// Its port on the relay, the second column of `plugover status` (such as 1-4).
        busid: String,
        /// Which relay, by name or id; needed only when several are paired.
        #[arg(long)]
        relay: Option<String>,
    },
    /// Unplug a device plugged in with `attach`.
    Detach {
        /// Its port on the relay, the second column of `plugover status` (such as 1-4).
        busid: String,
        /// Which relay, by name or id; needed only when several are paired.
        #[arg(long)]
        relay: Option<String>,
    },
    /// Check this machine's setup and say what to fix.
    Doctor {
        /// Print a plain-text report with the checks and recent logs, for a bug report.
        #[arg(long)]
        report: bool,
        #[arg(long, default_value_os_t = relay_state())]
        state: PathBuf,
    },
    /// Show the relay's or the host daemon's log.
    Logs {
        /// The relay's log.
        #[arg(long, conflicts_with = "host")]
        relay: bool,
        /// The host daemon's log (the default when both run here).
        #[arg(long)]
        host: bool,
        /// Keep showing new lines.
        #[arg(long, short)]
        follow: bool,
        /// How many lines to show.
        #[arg(long, short = 'n', default_value_t = 100)]
        lines: usize,
    },
    /// Install the newest Plugover release over this one; pairings and settings stay.
    Update {
        /// Only say whether there is a newer release.
        #[arg(long)]
        check: bool,
        /// Do not ask for confirmation.
        #[arg(long, short)]
        yes: bool,
    },
    /// List the USB devices on this machine.
    Devices {
        #[arg(long)]
        json: bool,
    },
    /// Print a code that pairs one host with this relay (on the relay, as root or administrator).
    Code {
        #[arg(long, default_value_os_t = relay_state())]
        state: PathBuf,
    },
    /// List the hosts paired with this relay (on the relay, as root or administrator).
    Hosts {
        #[arg(long, default_value_os_t = relay_state())]
        state: PathBuf,
    },
    /// Remove a paired host; it loses its devices within a second (on the relay).
    Unpair {
        /// Its name or id, from `plugover hosts`.
        host: String,
        #[arg(long, default_value_os_t = relay_state())]
        state: PathBuf,
    },
    /// Show or limit which devices this relay lends out (on the relay). Once a device is on the
    /// list, only listed devices are shared.
    Share {
        /// vendor:product, see `plugover devices`.
        #[arg(requires = "toggle")]
        device: Option<String>,
        #[arg(value_name = "on|off", value_parser = ["on", "off"], requires = "device")]
        toggle: Option<String>,
        /// Share every device again (except hubs and storage, which never are).
        #[arg(long, conflicts_with = "device")]
        all: bool,
        #[arg(long, default_value_os_t = relay_state())]
        state: PathBuf,
    },
    /// Lend devices to paired hosts (the relay service runs this).
    #[command(hide = true)]
    Relay {
        #[arg(long, default_value_t = proto::DEFAULT_PORT)]
        port: u16,
        #[arg(long, default_value_os_t = relay_state())]
        state: PathBuf,
    },
    /// Run the host daemon (the host service runs this).
    #[command(hide = true)]
    Host {
        #[arg(long, default_value_os_t = config_dir().join("host.toml"))]
        config: PathBuf,
    },
    /// Tell the daemon a stream started or stopped (the punktfunk plugin does this for you).
    #[command(hide = true)]
    Session {
        #[arg(value_parser = ["start", "stop"])]
        state: String,
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

#[cfg(windows)]
fn relay_state() -> PathBuf {
    program_data().join("relay.toml")
}

#[cfg(not(windows))]
fn relay_state() -> PathBuf {
    "/var/lib/plugover/relay.toml".into()
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

fn run(cli: Cli) -> Result<()> {
    let call = |req: Request| hostd::call(&cli.socket, &req);
    let done = |msg: String| println!("{} {msg}", paint(GOOD.bold(), "✓"));
    match cli.cmd {
        Cmd::Setup(args) => setup::run(args)?,
        Cmd::Devices { json } => cli::devices(json, &state::load::<state::HostConfig>(&config_dir().join("host.toml")).unwrap_or_default().label)?,
        Cmd::Relay { port, state } => std::sync::Arc::new(relay::Relay::open(SYS.into(), state)?).serve(port)?,
        Cmd::Code { state } => print_code(&relay::open_pairing(&state)?),
        Cmd::Hosts { state } => print_hosts(&state::load(&state)?),
        Cmd::Unpair { host, state } => {
            let h = relay::unpair(&state, &host)?;
            done(format!("Unpaired {}. Devices it had come back within a second.", paint(BOLD, &h.name)));
        }
        Cmd::Share { device, toggle, all, state } => {
            let st = match (device, all) {
                (Some(d), _) => relay::share(&state, Some((&d, toggle.as_deref() == Some("on"))))?,
                (None, true) => relay::share(&state, None)?,
                (None, false) => state::load(&state)?,
            };
            print_share(&st);
        }
        Cmd::Host { config } => hostd::Daemon::open(SYS.into(), config)?.run(&cli.socket)?,
        Cmd::Doctor { report: true, state } => setup::report(&cli.socket, &config_dir().join("host.toml"), &state),
        Cmd::Doctor { report: false, state } => {
            if !doctor::run(&cli.socket, &config_dir().join("host.toml"), &state) {
                std::process::exit(1);
            }
        }
        Cmd::Logs { relay, host, follow, lines } => {
            let role = if relay { Some(setup::Role::Relay) } else if host { Some(setup::Role::Host) } else { None };
            setup::logs(role, follow, lines)?
        }
        Cmd::Update { check, yes } => setup::update(check, yes)?,
        Cmd::Status { json } => {
            if json {
                println!("{}", serde_json::to_string_pretty(&call(Request::Status)?)?)
            } else {
                print_status(&hostd::ask(&cli.socket, &Request::Status)?)
            }
        }
        Cmd::Discover => print_discovered(&hostd::ask(&cli.socket, &Request::Discover)?),
        Cmd::Pair { address, code } => {
            let address = match address {
                Some(a) => a,
                None => choose_relay()?,
            };
            let code = match code {
                Some(c) => c,
                None => ask(&format!("Code from {} on the relay: ", cmd(if cfg!(windows) { "plugover code" } else { "sudo plugover code" })))?,
            };
            let v = call(Request::Pair { address, code })?;
            done(format!("Paired with {}.", paint(BOLD, v["relay"]["name"].as_str().unwrap_or("the relay"))));
            println!("{} {}", paint(DIM, "Next, pick devices:"), cmd("plugover status"));
        }
        Cmd::Forget { relay } => {
            call(Request::Forget { relay: relay.clone() })?;
            done(format!("Forgot {}.", paint(BOLD, relay)));
        }
        Cmd::Unencrypted { relay, state } => {
            let on = state == "on";
            call(Request::Unencrypted { relay: relay.clone(), on })?;
            done(if on {
                format!("{}'s devices now connect without encryption. Update Plugover on it to encrypt them again.", paint(BOLD, relay))
            } else {
                format!("{}'s devices connect only encrypted again.", paint(BOLD, relay))
            });
        }
        Cmd::Label { device, name, kind, reset } => {
            if !reset && name.is_none() && kind.is_none() {
                anyhow::bail!("pass --name, --kind or --reset");
            }
            call(Request::Label { device: device.clone(), name: name.clone(), kind })?;
            let what = match (name, kind) {
                (None, None) => "what Plugover detects".to_string(),
                (Some(n), None) => paint(BOLD, n),
                (None, Some(k)) => paint(BOLD, k.label()),
                (Some(n), Some(k)) => format!("{} ({})", paint(BOLD, n), k.label()),
            };
            done(format!("{} now shows as {what}.", paint(ACCENT, &device)));
        }
        Cmd::Auto { device, state, relay, port } => {
            let on = state == "on";
            call(Request::Auto { relay, device: device.clone(), on, port: port.clone() })?;
            let what = match &port {
                Some(p) => format!("{} on port {}", paint(ACCENT, &device), paint(BOLD, p)),
                None => paint(ACCENT, &device),
            };
            done(if on { format!("{what} connects whenever a stream runs.") } else { format!("Auto-connect is off for {what}.") });
        }
        Cmd::Session { state } => {
            call(Request::Session { active: state == "start" })?;
        }
        Cmd::Attach { busid, relay } => {
            let v = call(Request::Attach { relay, busid: busid.clone() })?;
            done(format!("Connected {} (virtual port {}).", paint(BOLD, busid), v["port"]));
        }
        Cmd::Detach { busid, relay } => {
            call(Request::Detach { relay, busid: busid.clone() })?;
            done(format!("Disconnected {}. It stays unplugged until the next stream.", paint(BOLD, busid)));
        }
    }
    Ok(())
}

/// Started from Explorer rather than a terminal: the console window is ours alone.
#[cfg(windows)]
fn double_clicked() -> bool {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetConsoleProcessList(list: *mut u32, count: u32) -> u32;
    }
    let mut ids = [0u32; 2];
    // SAFETY: the buffer is valid for the two entries we say it holds.
    unsafe { GetConsoleProcessList(ids.as_mut_ptr(), 2) == 1 }
}

/// The command line, listing only the commands for the roles set up here; the others still work.
fn command() -> clap::Command {
    let host = endpoint().exists() || config_dir().join("host.toml").exists();
    let relay = relay_state().exists();
    let hide: &[&str] = match (host, relay) {
        (true, false) => &["code", "hosts", "unpair", "share"],
        (false, true) => &["status", "discover", "pair", "forget", "unencrypted", "auto", "label", "attach", "detach"],
        _ => &[],
    };
    hide.iter().fold(Cli::command(), |c, name| c.mut_subcommand(name, |s| s.hide(true)))
}

fn main() -> ExitCode {
    #[cfg(windows)]
    if env::args_os().len() == 1 && double_clicked() {
        let args = setup::Args { pause: true, ..Default::default() };
        return if setup::run(args).is_ok() { ExitCode::SUCCESS } else { ExitCode::FAILURE };
    }
    let cli = Cli::from_arg_matches(&command().get_matches()).unwrap_or_else(|e| e.exit());
    let Err(e) = run(cli) else { return ExitCode::SUCCESS };
    if e.is::<setup::Reported>() {
        return ExitCode::FAILURE;
    }
    eprintln!("{} {e:#}", paint(BAD.bold(), "error:"));
    let denied = e.chain().any(|c| c.downcast_ref::<std::io::Error>().is_some_and(|io| io.kind() == std::io::ErrorKind::PermissionDenied));
    if denied {
        eprintln!("  {} this needs more rights; {SUDO}", paint(DIM, "→"));
    }
    ExitCode::FAILURE
}
