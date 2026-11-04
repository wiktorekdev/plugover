//! `plugover doctor`: checks whatever role this machine has and says what to fix.

#[cfg(not(windows))]
use crate::SYS;
use crate::{
    api::{RelayStatus, Status},
    hostd, proto,
    state::{self, PairingWindow, RelayState},
    term::{ACCENT, BAD, BOLD, DIM, GOOD, cmd, fail, ok, paint, warn},
};
use anstream::println;
use std::{
    net::{Ipv4Addr, SocketAddr, TcpStream},
    path::Path,
    time::Duration,
};

/// Returns false if something is broken.
pub fn run(endpoint: &Path, host_config: &Path, relay_state: &Path) -> bool {
    println!(
        "{} {}",
        paint(ACCENT.bold(), "◆ plugover"),
        paint(DIM, format!("{} · {}", proto::VERSION, state::hostname()))
    );
    checks(endpoint, host_config, relay_state)
}

/// The checks without the header, for the end of `plugover setup`.
pub fn checks(endpoint: &Path, host_config: &Path, relay_state: &Path) -> bool {
    let host = endpoint.exists() || host_config.exists();
    let relay = relay_state.exists();
    if !host && !relay {
        println!("\nPlugover is not set up here yet. Run {}, which asks whether this computer is the relay or", cmd("plugover setup"));
        println!("the host.");
        return false;
    }
    let mut healthy = true;
    if relay {
        println!("\n{}", paint(BOLD, "Relay"));
        healthy &= check_relay(relay_state);
    }
    if host {
        println!("\n{}", paint(BOLD, "Host"));
        healthy &= check_host(endpoint);
    }
    println!();
    let warnings = crate::term::WARNINGS.load(std::sync::atomic::Ordering::Relaxed);
    match (healthy, warnings) {
        (true, 0) => println!("{}", paint(GOOD, "Everything looks fine.")),
        (true, _) => println!("{}", paint(DIM, "Nothing is broken; the warnings above are worth a look.")),
        (false, _) => println!("{}", paint(BAD, "Fix the ✗ lines above, then run plugover doctor again.")),
    }
    healthy
}

fn check_relay(state_path: &Path) -> bool {
    let mut healthy = true;
    let local = SocketAddr::from((Ipv4Addr::LOCALHOST, proto::DEFAULT_PORT));
    if TcpStream::connect_timeout(&local, Duration::from_secs(1)).is_ok() {
        ok("Relay service", format!("listening on port {}", proto::DEFAULT_PORT));
    } else {
        healthy = false;
        let hint = if cfg!(windows) { "Get-ScheduledTask 'Plugover relay' (or run the installer again)" } else { "systemctl status plugover-relay" };
        fail("Relay service", "not listening on port 7275", Some(hint));
    }

    #[cfg(windows)]
    if crate::usbipd::exe().exists() {
        ok("usbipd-win", "installed");
    } else {
        healthy = false;
        fail("usbipd-win", "not found", Some("run the installer again"));
    }
    #[cfg(not(windows))]
    if crate::sysfs::export_available(SYS.as_ref()) {
        ok("usbip-host", "loaded");
    } else {
        healthy = false;
        fail("usbip-host", "kernel module not loaded", Some("sudo modprobe usbip-host"));
    }

    match state::load::<RelayState>(state_path) {
        Ok(st) => {
            let names: Vec<&str> = st.hosts.iter().map(|h| h.name.as_str()).collect();
            if names.is_empty() {
                warn("Paired hosts", "none yet", Some(&format!("{} and type the code on the host", cmd(&sudo("plugover code")))));
            } else {
                ok("Paired hosts", names.join(", "));
            }
            match &st.share {
                None => ok("Sharing", "every device except hubs and storage"),
                Some(l) if l.is_empty() => warn("Sharing", "nothing", Some(&format!("{} to share a device", cmd(&sudo("plugover share <id> on"))))),
                Some(l) => ok("Sharing", format!("only {}", l.join(", "))),
            }
        }
        Err(_) => warn("Paired hosts", "cannot read the relay's settings", Some(&format!("{} to see them", cmd(&sudo("plugover doctor"))))),
    }
    if let Ok(w) = state::load::<PairingWindow>(&PairingWindow::path(state_path))
        && w.is_open()
    {
        let left = w.expires.saturating_sub(state::now()).div_ceil(60);
        ok("Pairing", format!("open for {left} more min, {} tries left", w.attempts));
    }
    healthy
}

fn check_host(endpoint: &Path) -> bool {
    let mut healthy = true;
    #[cfg(not(windows))]
    {
        let sys: &Path = SYS.as_ref();
        match crate::sysfs::vhci_free(sys) {
            Ok((usb2, usb3)) => ok("vhci-hcd", format!("loaded, {usb2} USB 2 and {usb3} USB 3 ports free")),
            Err(_) => {
                healthy = false;
                fail("vhci-hcd", "kernel module not loaded", Some("sudo modprobe vhci-hcd"));
            }
        }
        if crate::sysfs::vhci_writable(sys) {
            ok("Plugging devices in", "allowed for this user");
        } else {
            healthy = false;
            fail(
                "Plugging devices in",
                "not allowed for this user",
                Some("punktfunk-host's udev rule allows the punktfunk group: sudo usermod -aG punktfunk $USER, then log in again"),
            );
        }
    }
    #[cfg(windows)]
    if crate::vhci::usbip_exe().exists() {
        ok("usbip-win2", "installed");
    } else {
        healthy = false;
        fail("usbip-win2", "not found", Some("run the installer again"));
    }

    let status: Status = match hostd::ask(endpoint, &hostd::Request::Status) {
        Ok(v) => v,
        Err(e) => {
            let hint = if cfg!(windows) { "Get-ScheduledTask 'Plugover host' (or run the installer again)" } else { "systemctl --user status plugover-host" };
            fail("Host daemon", format!("{e:#}"), Some(hint));
            return false;
        }
    };
    let daemon = (!status.version.is_empty()).then_some(status.version.as_str());
    if daemon == Some(proto::VERSION) {
        ok("Host daemon", if status.session { "running, a stream is on" } else { "running, no stream" });
    } else {
        let hint = if cfg!(windows) {
            "run the installer again to update it (or restart the 'Plugover host' scheduled task)"
        } else {
            "run the installer again to update it (or systemctl --user restart plugover-host)"
        };
        warn("Host daemon", format!("runs Plugover {}, this command {}", daemon.unwrap_or("older than 0.5"), proto::VERSION), Some(hint));
    }
    if status.relays.is_empty() {
        warn("Relays", "none paired", Some(&format!("{}, or USB devices in the punktfunk console", cmd("plugover pair"))));
    }
    for r in &status.relays {
        healthy &= check_paired(r, daemon);
    }
    healthy
}

fn check_paired(r: &RelayStatus, ours: Option<&str>) -> bool {
    let name = format!("Relay {}", r.name);
    if !r.online {
        let why = r.error.as_deref().unwrap_or("no answer yet");
        let hint = if why.contains("pair again") {
            format!("{} on the relay, then {} here", cmd(&sudo("plugover code")), cmd("plugover pair"))
        } else {
            format!("check that it runs ({} on it) and that TCP 7275 is open to this machine", cmd("plugover doctor"))
        };
        fail(&name, format!("offline at {}: {why}", r.address), Some(&hint));
        return false;
    }
    match (r.version.as_deref(), ours) {
        (theirs, Some(ours)) if theirs != Some(ours) => warn(
            &name,
            format!("online, but runs Plugover {} and this host {ours}", theirs.unwrap_or("older than 0.5")),
            // Only 0.5 and later have `update`.
            Some(&if theirs.is_some() { format!("{} on {}", cmd("plugover update"), r.name) } else { format!("run the installer on {}", r.name) }),
        ),
        _ => ok(&name, format!("online at {}", r.address)),
    }
    let waiting: Vec<&str> = r.missing.iter().map(|m| m.title.as_str()).collect();
    if !waiting.is_empty() {
        warn("Auto-connect", format!("not plugged into the relay: {}", waiting.join(", ")), None);
    }
    true
}

fn sudo(c: &str) -> String {
    if cfg!(windows) { c.to_string() } else { format!("sudo {c}") }
}
