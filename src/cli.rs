//! What the CLI prints: tables and messages for people, in color on a terminal.

use crate::{
    api::{DeviceState, DeviceStatus, Discovered, RelayStatus, Status},
    catalog, discovery, relay, state, sysfs,
};
use crate::term::{ACCENT, BAD, BOLD, DIM, GOOD, WARN, cell, cmd, paint, widest};
use anstream::{print, println};
use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::{
    io::{IsTerminal, Write},
    time::Duration,
};

#[derive(Serialize)]
struct DeviceView {
    busid: String,
    id: String,
    driver: Option<String>,
    #[serde(flatten)]
    identity: catalog::Identity,
}

/// USB devices plugged into this machine.
fn local_devices() -> Result<Vec<sysfs::UsbDevice>> {
    #[cfg(windows)]
    return Ok(crate::usbipd::list()?.into_iter().map(|d| d.dev).collect());
    #[cfg(not(windows))]
    Ok(sysfs::list(crate::SYS.as_ref())?)
}

fn local_views(labels: &[state::Label]) -> Result<Vec<DeviceView>> {
    let ids = catalog::UsbIds::load();
    Ok(local_devices()?
        .iter()
        .map(|d| {
            let id = format!("{:04x}:{:04x}", d.vid, d.pid);
            let mut identity = catalog::identify(d, ids.as_ref());
            state::Label::apply(labels, &id, &mut identity);
            DeviceView { busid: d.busid.clone(), id, driver: d.driver.clone(), identity }
        })
        .collect())
}

pub fn devices(json: bool, labels: &[state::Label]) -> Result<()> {
    let views = local_views(labels)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&views)?);
        return Ok(());
    }
    if views.is_empty() {
        println!("No USB devices plugged in.");
        return Ok(());
    }
    let w = |f: fn(&DeviceView) -> &str| widest(views.iter().map(f));
    let (wb, wk) = (w(|v| &v.busid), w(|v| v.identity.kind.label()).max("never shared".len()));
    println!("{}  {}  {}  {}", cell(DIM, "PORT", wb), cell(DIM, "ID", 9), cell(DIM, "KIND", wk), paint(DIM, "NAME"));
    for v in &views {
        let i = &v.identity;
        let (style, kind) = match &i.blocked {
            Some(_) => (DIM, "never shared"),
            None => (BOLD, i.kind.label()),
        };
        // "ASM1074 hub (USB hub)" says it twice.
        let why = i
            .blocked
            .as_deref()
            .filter(|b| !i.title.to_lowercase().contains(b.rsplit(' ').next().unwrap_or(b)))
            .map(|b| paint(DIM, format!(" ({b})")))
            .unwrap_or_default();
        println!("{}  {}  {}  {}{why}", cell(DIM, &v.busid, wb), cell(ACCENT, &v.id, 9), cell(DIM, kind, wk), paint(style, &i.title));
    }
    Ok(())
}

/// Keeps long USB product names from pushing the other columns off screen.
fn short(title: &str) -> String {
    const MAX: usize = 40;
    if title.chars().count() <= MAX { title.into() } else { format!("{}…", title.chars().take(MAX - 1).collect::<String>().trim_end()) }
}

pub fn print_status(st: &Status) {
    let version = if st.version.is_empty() { "daemon older than 0.5" } else { &st.version };
    println!(
        "{} {} {}",
        paint(BOLD, &st.host),
        paint(DIM, format!("· Plugover {version} ·")),
        if st.session { paint(GOOD, "● stream running") } else { paint(DIM, "○ no stream") }
    );
    if st.relays.is_empty() {
        println!("\nNo relay paired yet. Run {} here, or open Plugover in the punktfunk console.", cmd("plugover pair"));
    }
    for r in &st.relays {
        print_relay(r, &st.version);
    }
    let mut shareable = st.relays.iter().flat_map(|r| &r.devices).filter(|d| d.info.identity.blocked.is_none()).peekable();
    if shareable.peek().is_some() && !shareable.any(|d| d.auto) {
        println!("\n{} pick what connects when you stream: {}", paint(DIM, "Tip:"), cmd("plugover auto <id> on"));
    }
}

fn print_relay(r: &RelayStatus, ours: &str) {
    let latency = r.latency.map(|l| paint(if l.good() { DIM } else { WARN }, format!("  {:.1} ms", l.average))).unwrap_or_default();
    println!(
        "\n{}  {}  {}{}{latency}{}",
        paint(BOLD, &r.name),
        if r.online { paint(GOOD, "● online") } else { paint(BAD, "● offline") },
        paint(DIM, &r.address),
        r.version.as_ref().map(|v| paint(DIM, format!("  v{v}"))).unwrap_or_default(),
        if r.encrypted { paint(DIM, "  encrypted") } else { String::new() }
    );
    if r.unencrypted {
        println!("  {} not encrypted: anyone on your network could watch its devices or fake their input", paint(WARN, "!"));
        println!("    {} update Plugover on {} to encrypt, or stop with {}", paint(DIM, "→"), r.name, cmd(&format!("plugover unencrypted {} off", r.name)));
    } else if r.online && !r.encrypted {
        println!("  {} {} cannot encrypt, so its devices do not connect", paint(BAD, "✗"), r.name);
        println!(
            "    {} update Plugover on it, or connect without encryption: {}",
            paint(DIM, "→"),
            cmd(&format!("plugover unencrypted {} on", r.name))
        );
    }
    if let Some(l) = r.latency.filter(|l| !l.good()) {
        println!(
            "  {} the link is slow or uneven ({:.1} ms, varies by {:.1} ms); force feedback needs a steady one, so wire both machines if you can",
            paint(WARN, "!"),
            l.average,
            l.jitter
        );
    }
    if !r.online {
        let why = r.error.as_deref().unwrap_or("no answer yet");
        println!("  {} {why}", paint(BAD, "✗"));
        if why.contains("pair again") {
            let code = cmd(if cfg!(windows) { "plugover code" } else { "sudo plugover code" });
            println!("    {} {code} on {}, then {} here", paint(DIM, "→"), r.name, cmd("plugover pair"));
        } else {
            println!("    {} run {} on {} to see why", paint(DIM, "→"), cmd("plugover doctor"), r.name);
        }
    } else if !ours.is_empty() && r.version.as_deref() != Some(ours) {
        match r.version.as_deref() {
            Some(theirs) if crate::setup::newer(theirs, ours) => {
                println!("  {} {} runs Plugover {theirs}; update this computer too ({})", paint(WARN, "!"), r.name, cmd("plugover update"));
            }
            Some(theirs) => println!("  {} {} runs Plugover {theirs}; update it to {ours} too ({})", paint(WARN, "!"), r.name, cmd("plugover update")),
            None => println!("  {} {} runs Plugover older than 0.5; update it to {ours} too (run the installer there)", paint(WARN, "!"), r.name),
        }
    }
    let (shown, hidden): (Vec<&DeviceStatus>, Vec<&DeviceStatus>) = r.devices.iter().partition(|d| d.info.identity.blocked.is_none());
    let titles: Vec<String> = shown.iter().map(|d| short(&d.info.identity.title)).chain(r.missing.iter().map(|m| short(&m.title))).collect();
    let wt = widest(titles.iter().map(String::as_str));
    let wb = widest(shown.iter().map(|d| d.info.busid.as_str()).chain(r.missing.iter().filter_map(|m| m.port.as_deref())));
    for (d, title) in shown.iter().zip(&titles) {
        let (dot, state) = match d.state {
            DeviceState::Here => (paint(GOOD, "●"), paint(GOOD, "connected")),
            DeviceState::Elsewhere => (paint(WARN, "◐"), paint(WARN, format!("in use by {}", d.info.lent_to.as_deref().unwrap_or("someone")))),
            DeviceState::Free => (paint(DIM, "○"), paint(DIM, "free")),
        };
        let auto = match (d.auto, d.auto_port) {
            (true, true) => paint(ACCENT, "  connects with streams, this port only"),
            (true, false) => paint(ACCENT, "  connects with streams"),
            _ => String::new(),
        };
        let drops = match d.history.drops {
            0 => String::new(),
            1 => paint(WARN, "  dropped out once"),
            n => paint(WARN, format!("  dropped out {n} times")),
        };
        println!("  {dot} {}  {}  {}  {state}{auto}{drops}", cell(BOLD, title, wt), cell(DIM, &d.info.busid, wb), cell(ACCENT, &d.info.id, 9));
        if let Some(why) = &d.error {
            println!("    {} could not connect: {why}", paint(BAD, "✗"));
        }
    }
    for (m, title) in r.missing.iter().zip(&titles[shown.len()..]) {
        println!(
            "  {} {}  {}  {}  {}",
            paint(DIM, "◌"),
            cell(DIM, title, wt),
            cell(DIM, m.port.as_deref().unwrap_or_default(), wb),
            cell(ACCENT, &m.id, 9),
            paint(DIM, if r.online { "not plugged in  connects with streams" } else { "connects with streams" })
        );
    }
    if r.online && shown.is_empty() && r.missing.is_empty() {
        println!("  {}", paint(DIM, "Nothing to share is plugged in."));
    }
    if !hidden.is_empty() {
        println!("  {}", paint(DIM, format!("{} hidden: hubs, storage and devices the relay keeps", hidden.len())));
    }
}

pub fn print_discovered(found: &Discovered) {
    if found.relays.is_empty() {
        println!("No relays found. Is the relay running, and does its firewall allow TCP 7275 and UDP 5353?");
        return;
    }
    let wn = widest(found.relays.iter().map(|f| f.name.as_str()));
    for f in &found.relays {
        let address = f.address.as_deref().map_or_else(|| paint(BAD, "not reachable"), |a| paint(ACCENT, a));
        let paired = if f.paired { paint(GOOD, "  paired") } else { String::new() };
        println!("{}  {address}{paired}", cell(BOLD, &f.name, wn));
    }
    println!("\n{} {}", paint(DIM, "Pair with"), cmd("plugover pair <address>"));
}

/// Finds the relay to pair with; asks which one when there are several.
pub fn choose_relay() -> Result<String> {
    print!("{}", paint(DIM, "Looking for relays… "));
    std::io::stdout().flush()?;
    let found: Vec<_> = discovery::browse(Duration::from_secs(3))?
        .into_iter()
        .filter_map(|f| Some((discovery::reachable(&f.addresses)?, f.name)))
        .collect();
    println!();
    match found.as_slice() {
        [] => bail!("no relay answered; check that it runs and its firewall allows TCP 7275 and UDP 5353, or pass its address"),
        [(addr, name)] => {
            println!("Found {} at {}.", paint(BOLD, name), paint(ACCENT, addr));
            Ok(addr.to_string())
        }
        _ if !std::io::stdin().is_terminal() => bail!("found {} relays; pass an address (see `plugover discover`)", found.len()),
        _ => {
            for (i, (addr, name)) in found.iter().enumerate() {
                println!("  {}  {}  {}", paint(ACCENT.bold(), i + 1), paint(BOLD, name), paint(DIM, addr));
            }
            let pick = ask(&format!("Which one? [1-{}] ", found.len()))?;
            let i: usize = pick.trim().parse().ok().filter(|i| (1..=found.len()).contains(i)).context("no such relay")?;
            Ok(found[i - 1].0.to_string())
        }
    }
}

pub fn ask(prompt: &str) -> Result<String> {
    print!("{prompt}");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(line)
}

pub fn print_code(code: &str) {
    println!();
    println!("  {}", paint(DIM, format!("Pairing code for {}", state::hostname())));
    println!();
    println!("      {}", paint(ACCENT.bold(), format!("{} {}", &code[..3], &code[3..])));
    println!();
    println!("  Type it in the punktfunk console under {}, or run {} on the host.", paint(BOLD, "Plugover"), cmd("plugover pair"));
    println!("  {}", paint(DIM, format!("It works once, for {} minutes.", relay::PAIR_MINUTES)));
}

pub fn print_hosts(st: &state::RelayState) {
    if st.hosts.is_empty() {
        println!("No hosts paired yet. Run {} and type the code on the host.", cmd(if cfg!(windows) { "plugover code" } else { "sudo plugover code" }));
        return;
    }
    println!("{}", paint(DIM, format!("Hosts paired with {}", state::hostname())));
    let wn = widest(st.hosts.iter().map(|h| h.name.as_str()));
    for h in &st.hosts {
        println!("  {}  {}", cell(BOLD, &h.name, wn), paint(DIM, &h.id));
    }
}

pub fn print_share(st: &state::RelayState) {
    let me = state::hostname();
    let Some(list) = &st.share else {
        println!("{} shares every device except hubs and storage.", paint(BOLD, &me));
        println!("{} {}", paint(DIM, "To share only some, run"), cmd("plugover share <id> on"));
        return;
    };
    if list.is_empty() {
        println!("{} shares {}.", paint(BOLD, &me), paint(WARN, "no devices"));
    } else {
        println!("{} shares only:", paint(BOLD, &me));
    }
    let plugged = local_views(&[]).unwrap_or_default();
    for id in list {
        let here = plugged.iter().find(|v| v.id == *id);
        let what = here.map_or(paint(DIM, "not plugged in"), |v| format!("{} {}", paint(BOLD, &v.identity.title), paint(DIM, format!("at {}", v.busid))));
        println!("  {}  {what}", paint(ACCENT, id));
    }
    println!("{} {}", paint(DIM, "Share everything again with"), cmd("plugover share --all"));
}
