//! The host daemon: keeps a live view of every paired relay, plugs the devices you chose in
//! while a stream runs (and again after a dropout), and answers the punktfunk plugin and the
//! CLI with one JSON object per line: over a socket file on Linux; on Windows over loopback TCP
//! with a token for the CLI and a named pipe for the plugin.

use crate::{
    api, catalog, discovery, host,
    proto::{self, Snapshot},
    state::{self, AutoRule, HostConfig, PairedRelay},
    vhci,
};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs,
    io::{BufReader, Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

/// Round trips kept per relay for its latency.
const SAMPLES: usize = 10;
/// A device that failed to connect is tried again after this long, not on every snapshot.
const RETRY: Duration = Duration::from_secs(5);
/// How long after a device vanished one with the same maker on its port counts as the same one.
const FOLLOW: Duration = Duration::from_secs(20);
/// A source that reported streams this recently still watches them, so devices in auto mode
/// connect only during streams. punktfunk's plugin reports every 10 s.
const WATCH: Duration = Duration::from_secs(60);
/// How often the host apps of hooked sources are checked, and how many checks in a row they may
/// miss before their stream counts as over.
const PROBE: Duration = Duration::from_secs(5);
const PROBE_MISSES: u32 = 3;

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Status,
    Discover,
    /// `code` is what `plugover code` printed on the relay.
    Pair { address: String, code: String },
    Forget { relay: String },
    /// Pass `device` (vendor:product) through whenever a stream runs; with `port`, only the one
    /// plugged in there.
    Auto {
        relay: Option<String>,
        device: String,
        on: bool,
        #[serde(default)]
        port: Option<String>,
        /// When it connects; left out, an existing rule keeps its mode and a new one is auto.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mode: Option<state::Mode>,
    },
    /// Names a model, or corrects its kind; neither removes the label.
    Label {
        device: String,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        kind: Option<catalog::Kind>,
    },
    /// Names one device. A missing or empty name removes its override.
    DeviceName {
        relay: String,
        device: String,
        busid: String,
        #[serde(default)]
        serial: Option<String>,
        #[serde(default)]
        name: Option<String>,
    },
    /// Keep the keyboard and mouse of `device` (vendor:product) with this computer's desktop when
    /// a stream runs the game on a screen of its own, or let them go there again.
    Desktop { device: String, on: bool },
    /// Plug a device in now; with `take`, first take it over from the host that has it.
    Attach {
        relay: Option<String>,
        busid: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        take: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_owner: Option<String>,
    },
    Detach { relay: Option<String>, busid: String },
    /// Allows connecting a relay's devices without encryption, for a relay too old to encrypt.
    Unencrypted { relay: String, on: bool },
    /// Bluetooth on the relay: look for input devices, pair one, or forget one. Each answers with
    /// the devices the relay knows.
    BtScan { relay: Option<String>, seconds: u64 },
    BtPair { relay: Option<String>, address: String },
    BtForget { relay: Option<String>, address: String },
    /// A one-time code for `plugover ui`. Not for the page itself, which cannot ask for one.
    UiCode,
    /// A stream started or stopped. Each `source` (the punktfunk plugin, a host app's hook, the
    /// CLI) holds its own lease; a stream runs while any lease does. With `ttl` (seconds) a lease
    /// ends by itself unless renewed, for sources that may die without saying stop. With `port`,
    /// the source's host app listens there while it runs: the source counts as watching streams
    /// while it answers, and its lease ends when it stops answering (the app crashed mid-stream).
    Session {
        active: bool,
        #[serde(default = "punktfunk")]
        source: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ttl: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        port: Option<u16>,
    },
}

/// Who sent a session request that names no source: the punktfunk plugin, the only sender
/// before 0.7.
fn punktfunk() -> String {
    "punktfunk".into()
}

type Slot = (String, String); // (relay id, busid)

fn naming_serial(device: &proto::DeviceInfo, devices: &[proto::DeviceInfo]) -> Option<String> {
    let serial = crate::sysfs::real_serial(device.serial.as_deref()?)?;
    (!devices.iter().any(|other| {
        other.busid != device.busid
            && other.id == device.id
            && other.serial.as_deref().is_some_and(|value| value.trim() == serial)
    }))
    .then_some(serial)
}

fn device_name<'a>(
    names: &'a [state::DeviceName],
    relay: &str,
    device: &proto::DeviceInfo,
    devices: &[proto::DeviceInfo],
) -> (Option<String>, Option<&'a state::DeviceName>) {
    let named = |serial| names.iter().find(|n| n.matches(relay, &device.id, &device.busid, serial));
    if let Some(name) = named(None) {
        return (None, Some(name));
    }
    let serial = naming_serial(device, devices);
    let name = serial.as_deref().and_then(|serial| named(Some(serial)));
    (serial, name)
}

/// Whether `d` should be plugged in here now, given its rules, and if so whether it stays outside
/// streams. Without a rule, a device that came back with another id is followed in the mode the
/// one before it had.
fn wanted(rules: &[AutoRule], relay: &str, d: &proto::DeviceInfo, track: Option<&Track>, session: bool, watching: bool) -> Option<bool> {
    let matched: Vec<&AutoRule> = rules.iter().filter(|a| a.matches(relay, d)).collect();
    let always = if matched.is_empty() {
        track.filter(|t| t.follows(d))?.always
    } else {
        matched.iter().any(|a| a.always(watching))
    };
    (session || always).then_some(always)
}

/// What happened to one device while the daemon runs.
#[derive(Default)]
struct Track {
    /// vendor:product it had when it was last plugged in here.
    id: String,
    /// Its serial number then, if it has one.
    serial: Option<String>,
    /// When it last vanished while plugged in.
    dropped: Option<Instant>,
    /// It was plugged in to stay outside streams too; a device that follows it does as well.
    always: bool,
    history: api::History,
}

impl Track {
    /// Wheels such as the G29 come back with another product id when their driver switches
    /// them to another mode; a device that vanished and is back on its port is followed. A serial
    /// number, when both have one, decides; without, the same maker on the same port does.
    fn follows(&self, d: &proto::DeviceInfo) -> bool {
        let same = match (&self.serial, &d.serial) {
            (Some(was), Some(is)) => was == is,
            _ => self.id.get(..4) == d.id.get(..4),
        };
        self.dropped.is_some_and(|at| at.elapsed() < FOLLOW) && self.id != d.id && same
    }
}

pub struct Daemon {
    sys: PathBuf,
    config_path: PathBuf,
    name: String,
    cfg: Mutex<HostConfig>,
    /// Latest snapshot per relay id; present means online.
    live: Mutex<HashMap<String, Snapshot>>,
    /// Why each offline relay is offline.
    errors: Mutex<HashMap<String, String>>,
    /// Devices we plugged in, and the vhci port each one landed on.
    attached: Mutex<HashMap<Slot, vhci::Attachment>>,
    /// Detached by hand; not plugged back in until the next stream, or until you plug it in or
    /// turn it on again.
    held: Mutex<HashSet<Slot>>,
    /// Plugged in by hand; stays until you unplug it or a stream ends.
    manual: Mutex<HashSet<Slot>>,
    /// Why plugging a chosen device in failed, and when, while it keeps failing.
    failed: Mutex<HashMap<Slot, (String, Instant)>>,
    /// Latest round trips to each relay.
    rtt: Mutex<HashMap<String, VecDeque<Duration>>>,
    tracks: Mutex<HashMap<Slot, Track>>,
    watched: Mutex<HashSet<String>>,
    /// Sources that say a stream runs, and when their lease ends if they set one.
    leases: Mutex<HashMap<String, Option<Instant>>>,
    /// When each source last said anything about streams.
    heard: Mutex<HashMap<String, Instant>>,
    started: Instant,
    /// A newer release, from the daily look at GitHub.
    update: Mutex<Option<String>>,
    /// Who may use the page `plugover ui` opens.
    pub ui: crate::ui::Gate,
    /// Some lease holds, as last applied.
    session: AtomicBool,
    /// Keyboard and mouse nodes passed into a stream's own screen, by node.
    #[cfg(target_os = "linux")]
    routes: Mutex<HashMap<PathBuf, (Slot, crate::route::Forward)>>,
    /// Why passing a node on failed, and when, while it keeps failing.
    #[cfg(target_os = "linux")]
    unrouted: Mutex<HashMap<PathBuf, (Slot, String, Instant)>>,
    /// Where gamescope puts its sockets.
    #[cfg(target_os = "linux")]
    runtime: PathBuf,
    reconciling: Mutex<()>,
}

impl Daemon {
    pub fn open(sys: PathBuf, config_path: PathBuf) -> Result<Arc<Self>> {
        Ok(Arc::new(Self {
            sys,
            cfg: Mutex::new({
                let mut cfg: HostConfig = state::load(&config_path)?;
                cfg.remembered_sources();
                cfg
            }),
            config_path,
            name: state::hostname(),
            live: Mutex::default(),
            errors: Mutex::default(),
            attached: Mutex::default(),
            held: Mutex::default(),
            manual: Mutex::default(),
            failed: Mutex::default(),
            rtt: Mutex::default(),
            tracks: Mutex::default(),
            watched: Mutex::default(),
            leases: Mutex::default(),
            heard: Mutex::default(),
            started: Instant::now(),
            update: Mutex::default(),
            ui: crate::ui::Gate::new(),
            session: AtomicBool::new(false),
            #[cfg(target_os = "linux")]
            routes: Mutex::default(),
            #[cfg(target_os = "linux")]
            unrouted: Mutex::default(),
            #[cfg(target_os = "linux")]
            runtime: crate::route::runtime_dir(),
            reconciling: Mutex::default(),
        }))
    }

    pub fn run(self: Arc<Self>, socket: &Path) -> Result<()> {
        let ids: Vec<String> = self.cfg.lock().unwrap().relay.iter().map(|r| r.id.clone()).collect();
        for id in ids {
            self.watch(id);
        }
        let page = self.clone();
        thread::spawn(move || crate::ui::serve(page));
        let probe = self.clone();
        thread::spawn(move || probe.probe_hosts());
        let releases = self.clone();
        thread::spawn(move || {
            // A while after starting, then twice a day; a failed look just waits for the next.
            thread::sleep(Duration::from_secs(60));
            loop {
                if let Ok(v) = crate::setup::latest_release()
                    && crate::setup::newer(&v, proto::VERSION)
                {
                    *releases.update.lock().unwrap() = Some(v);
                }
                thread::sleep(Duration::from_secs(12 * 3600));
            }
        });
        let me = self.clone();
        // Also notices a device that dropped out, to plug it in again; reading vhci's status is cheap.
        thread::spawn(move || loop {
            thread::sleep(Duration::from_millis(500));
            me.reconcile();
        });
        self.serve(socket)
    }

    fn save(&self, cfg: &HostConfig) -> Result<()> {
        state::save(&self.config_path, cfg)
    }

    fn relay(&self, id: &str) -> Option<PairedRelay> {
        self.cfg.lock().unwrap().relay.iter().find(|r| r.id == id).cloned()
    }

    /// One relay by id or name; with a single paired relay the argument may be left out.
    /// Asks the relay to scan, pair or forget, and answers with the devices it then knows.
    fn bluetooth(&self, relay: Option<String>, req: proto::RelayRequest) -> Result<Value> {
        let reply = host::request(&self.pick(relay.as_deref())?, &req)?;
        Ok(json!({ "ok": true, "nearby": reply.nearby.unwrap_or_default() }))
    }

    fn pick(&self, relay: Option<&str>) -> Result<PairedRelay> {
        let cfg = self.cfg.lock().unwrap();
        match relay {
            Some(want) => cfg.relay.iter().find(|r| r.id == want || r.name == want).cloned().context("no such relay"),
            None => match cfg.relay.as_slice() {
                [only] => Ok(only.clone()),
                [] => bail!("no relay is paired yet"),
                _ => bail!("several relays are paired; say which one"),
            },
        }
    }

    fn watch(self: &Arc<Self>, id: String) {
        if !self.watched.lock().unwrap().insert(id.clone()) {
            return;
        }
        let me = self.clone();
        thread::spawn(move || {
            let mut was_online = false;
            // Tries since the relay last answered.
            let mut fails = 0;
            loop {
                let Some(relay) = me.relay(&id) else {
                    // Checked again under the lock: a pairing that comes in now must find this
                    // thread still running, or no thread at all.
                    let mut watched = me.watched.lock().unwrap();
                    if me.relay(&id).is_some() {
                        continue;
                    }
                    watched.remove(&id);
                    return;
                };
                let result = host::watch(
                    &relay,
                    |snap| {
                        if !was_online {
                            eprintln!("{} is online at {}", relay.name, relay.address);
                            was_online = true;
                        }
                        fails = 0;
                        me.errors.lock().unwrap().remove(&id);
                        if snap.encrypted {
                            me.mark_encrypting(&id);
                        }
                        me.live.lock().unwrap().insert(id.clone(), snap);
                        me.reconcile();
                        me.relay(&id).is_some()
                    },
                    |d| {
                        let mut rtt = me.rtt.lock().unwrap();
                        let samples = rtt.entry(id.clone()).or_default();
                        if samples.len() == SAMPLES {
                            samples.pop_front();
                        }
                        samples.push_back(d);
                    },
                );
                me.live.lock().unwrap().remove(&id);
                me.rtt.lock().unwrap().remove(&id);
                if me.relay(&id).is_none() {
                    continue;
                }
                let why = result.err().map_or("the relay stopped answering".to_string(), |e| format!("{e:#}"));
                if was_online {
                    eprintln!("{} went offline: {why}", relay.name);
                    was_online = false;
                }
                me.errors.lock().unwrap().insert(id.clone(), why);
                // A relay that restarts (an update, say) is back at its address within a second or
                // two; looking it up over mDNS takes 3 s and is for one that moved.
                fails += 1;
                if fails <= 5 {
                    thread::sleep(Duration::from_millis(500));
                } else {
                    me.rediscover(&id);
                    thread::sleep(Duration::from_secs(2));
                }
            }
        });
    }

    /// The relay may have a new address; mDNS knows it by id.
    fn rediscover(&self, id: &str) {
        let Ok(found) = discovery::browse(Duration::from_secs(3)) else { return };
        let Some(addr) = found.iter().find(|f| f.id == id).and_then(|f| discovery::reachable(&f.addresses)) else { return };
        let mut cfg = self.cfg.lock().unwrap();
        if let Some(r) = cfg.relay.iter_mut().find(|r| r.id == id && r.address != addr.to_string()) {
            r.address = addr.to_string();
            let _ = self.save(&cfg);
        }
    }

    /// A relay that encrypted once must encrypt from now on, which also ends any permission to
    /// connect it without encryption.
    fn mark_encrypting(&self, id: &str) {
        let mut cfg = self.cfg.lock().unwrap();
        if let Some(r) = cfg.relay.iter_mut().find(|r| r.id == id && !r.encrypts) {
            (r.encrypts, r.unencrypted) = (true, false);
            let _ = self.save(&cfg);
        }
    }

    /// Whether `relay`'s devices are encrypted; an error if it cannot encrypt and you did not
    /// allow connecting without.
    fn encryption(&self, relay: &PairedRelay) -> Result<bool> {
        if relay.encrypts {
            return Ok(true);
        }
        let version = self.live.lock().unwrap().get(&relay.id).map(|s| s.version.clone()).filter(|v| !v.is_empty());
        anyhow::ensure!(
            relay.unencrypted,
            "{} runs Plugover {}, which cannot encrypt; update it, or allow connecting without encryption \
             (plugover unencrypted {} on)",
            relay.name,
            version.as_deref().unwrap_or("older than 0.5"),
            relay.name
        );
        Ok(false)
    }

    /// Notes that the device on `slot` is plugged in here as `id`, to stay outside streams too
    /// with `always`.
    fn plugged(&self, slot: &Slot, id: &str, serial: Option<String>, always: bool) {
        let mut tracks = self.tracks.lock().unwrap();
        let t = tracks.entry(slot.clone()).or_default();
        // An id not known yet (the relay's list lags a device that just came back) keeps the old one.
        if !id.is_empty() {
            if t.dropped.is_some() && t.id != id && !t.id.is_empty() {
                eprintln!("{} came back as {id}; following it", slot.1);
            }
            t.id = id.to_string();
            t.serial = serial;
        }
        (t.dropped, t.always) = (None, always);
    }

    /// Something here reports streams: a source holds a lease or spoke recently, or, for a while
    /// after a restart, one did before.
    fn watching(&self) -> bool {
        !self.sessions().is_empty()
            || self.heard.lock().unwrap().values().any(|at| at.elapsed() < WATCH)
            || (self.started.elapsed() < WATCH && !self.cfg.lock().unwrap().remembered_sources().is_empty())
    }

    /// Whether the attached device on `slot` stays when no stream runs: by its rule, or, without
    /// one, by how it was plugged in (a device that followed one, say). Never one plugged in by hand.
    fn stays(&self, slot: &Slot, watching: bool) -> bool {
        if self.manual.lock().unwrap().contains(slot) {
            return false;
        }
        let (cfg, live) = (self.cfg.lock().unwrap(), self.live.lock().unwrap());
        let Some(d) = live.get(&slot.0).and_then(|s| s.devices.iter().find(|d| d.busid == slot.1)) else { return false };
        let rules: Vec<&AutoRule> = cfg.auto.iter().filter(|a| a.matches(&slot.0, d)).collect();
        if rules.is_empty() {
            return self.tracks.lock().unwrap().get(slot).is_some_and(|t| t.always);
        }
        rules.iter().any(|a| a.always(watching))
    }

    /// Makes reality match the plan: forgets devices the kernel dropped, unplugs devices whose
    /// rule does not want them now, and plugs in every chosen device that is available and
    /// wanted now (always, or while a stream runs).
    fn reconcile(&self) {
        let Ok(_pass) = self.reconciling.try_lock() else { return };
        // A lease that ran out ends the stream like a stop would.
        self.apply_session();
        let mut dropped = Vec::new();
        self.attached.lock().unwrap().retain(|slot, a| {
            let alive = vhci::alive(&self.sys, a);
            if !alive {
                dropped.push(slot.clone());
            }
            alive
        });
        for slot in dropped {
            eprintln!("{} dropped out", slot.1);
            self.manual.lock().unwrap().remove(&slot);
            let mut tracks = self.tracks.lock().unwrap();
            let t = tracks.entry(slot).or_default();
            t.dropped = Some(Instant::now());
            t.history.drops += 1;
            t.history.last_drop = Some(state::now());
        }
        let (session, watching) = (self.session.load(Ordering::SeqCst), self.watching());
        // A rule that now wants its device only during streams (its mode changed, or a source
        // started watching) gives it back.
        let unwanted: Vec<Slot> = {
            let (attached, manual) = (self.attached.lock().unwrap(), self.manual.lock().unwrap());
            let candidates: Vec<Slot> = attached.keys().filter(|s| !manual.contains(*s)).cloned().collect();
            drop((attached, manual));
            let (cfg, live) = (self.cfg.lock().unwrap(), self.live.lock().unwrap());
            candidates
                .into_iter()
                .filter(|(r, busid)| {
                    let Some(d) = live.get(r).and_then(|s| s.devices.iter().find(|d| d.busid == *busid)) else { return false };
                    let rules: Vec<&AutoRule> = cfg.auto.iter().filter(|a| a.matches(r, d)).collect();
                    !session && !rules.is_empty() && !rules.iter().any(|a| a.always(watching))
                })
                .collect()
        };
        if !unwanted.is_empty() {
            self.unplug(|slot| unwanted.contains(slot));
        }
        let todo: Vec<(PairedRelay, String, String, Option<String>, bool)> = {
            let (cfg, live) = (self.cfg.lock().unwrap(), self.live.lock().unwrap());
            let (attached, held, failed) = (self.attached.lock().unwrap(), self.held.lock().unwrap(), self.failed.lock().unwrap());
            let tracks = self.tracks.lock().unwrap();
            cfg.relay
                .iter()
                .filter_map(|r| Some((r, live.get(&r.id)?)))
                .flat_map(|(relay, snap)| {
                    snap.devices
                        .iter()
                        .filter(|d| {
                            let slot = (relay.id.clone(), d.busid.clone());
                            wanted(&cfg.auto, &relay.id, d, tracks.get(&slot), session, watching).is_some()
                                && d.identity.blocked.is_none()
                                && d.lent_to.is_none()
                                && !attached.contains_key(&slot)
                                && !held.contains(&slot)
                                && failed.get(&slot).is_none_or(|(_, at)| at.elapsed() >= RETRY)
                        })
                        .filter_map(|d| {
                            let always = wanted(&cfg.auto, &relay.id, d, tracks.get(&(relay.id.clone(), d.busid.clone())), session, watching)?;
                            Some((relay.clone(), d.busid.clone(), d.id.clone(), d.serial.clone(), always))
                        })
                        .collect::<Vec<_>>()
                })
                .collect()
        };
        for (relay, busid, id, serial, always) in todo {
            let slot = (relay.id.clone(), busid);
            match self.encryption(&relay).and_then(|encrypt| vhci::attach(&self.sys, &relay, &slot.1, encrypt)) {
                Ok(a) => {
                    eprintln!("plugged in {} from {} (port {})", slot.1, relay.name, a.port);
                    self.plugged(&slot, &id, serial, always);
                    self.manual.lock().unwrap().remove(&slot);
                    self.failed.lock().unwrap().remove(&slot);
                    self.attached.lock().unwrap().insert(slot, a);
                }
                Err(e) => {
                    let why = format!("{e:#}");
                    let mut failed = self.failed.lock().unwrap();
                    // Said once, not on every retry.
                    if failed.get(&slot).is_none_or(|(old, _)| *old != why) {
                        eprintln!("could not plug in {} from {}: {why}", slot.1, relay.name);
                    }
                    let mut tracks = self.tracks.lock().unwrap();
                    let h = &mut tracks.entry(slot.clone()).or_default().history;
                    (h.last_error, h.last_error_at) = (Some(why.clone()), Some(state::now()));
                    failed.insert(slot, (why, Instant::now()));
                }
            }
        }
        #[cfg(target_os = "linux")]
        self.route();
    }

    /// While a stream runs and a gamescope is up, passes the keyboards and mice plugged in here
    /// into it (see `route`); otherwise leaves them to the desktop. The caller holds `reconciling`.
    #[cfg(target_os = "linux")]
    fn route(&self) {
        use crate::route;
        let screens = if self.session.load(Ordering::SeqCst) { route::screens(&self.runtime) } else { Vec::new() };
        let wanted: Vec<(PathBuf, Slot, route::Kind)> = if screens.is_empty() {
            Vec::new()
        } else {
            let (cfg, live, attached) = (self.cfg.lock().unwrap(), self.live.lock().unwrap(), self.attached.lock().unwrap());
            let stays = |(relay, busid): &Slot| {
                live.get(relay).and_then(|s| s.devices.iter().find(|d| d.busid == *busid)).is_some_and(|d| cfg.desktop.contains(&d.id))
            };
            attached
                .iter()
                .filter(|(slot, _)| !stays(slot))
                .flat_map(|(slot, a)| route::nodes(&self.sys, &slot.1, a.port).into_iter().map(|(node, kind)| (node, slot.clone(), kind)))
                .collect()
        };
        let (mut routes, mut unrouted) = (self.routes.lock().unwrap(), self.unrouted.lock().unwrap());
        let ended: Vec<PathBuf> =
            routes.iter().filter(|(node, (_, f))| f.finished() || !wanted.iter().any(|w| w.0 == **node)).map(|(node, _)| node.clone()).collect();
        for node in ended {
            let Some((slot, f)) = routes.remove(&node) else { continue };
            let (by_itself, into) = (f.finished(), f.screen());
            let why = f.stop();
            match (by_itself, why) {
                (true, Some(why)) => {
                    if unrouted.get(&node).is_none_or(|(_, old, _)| *old != why) {
                        eprintln!("could not pass {} ({}) into the stream: {why}", slot.1, node.display());
                    }
                    unrouted.insert(node, (slot, why, Instant::now()));
                }
                // Unplugged, or the stream is over.
                _ if into.is_some() => eprintln!("{} ({}) is back with the desktop", slot.1, node.display()),
                _ => {}
            }
        }
        unrouted.retain(|node, _| wanted.iter().any(|w| w.0 == *node));
        for (node, slot, kind) in wanted {
            if routes.contains_key(&node) || unrouted.get(&node).is_some_and(|(_, _, at)| at.elapsed() < RETRY) {
                continue;
            }
            if !unrouted.contains_key(&node) {
                eprintln!("passing {} ({}) into the stream's own screen", slot.1, node.display());
            }
            routes.insert(node.clone(), (slot, route::Forward::start(node, kind, screens.clone())));
        }
    }

    /// Where the keyboard or mouse of the device on `slot` goes while a stream runs on its own
    /// screen, and why it cannot, if it cannot.
    fn routing(&self, slot: &Slot) -> (Option<String>, Option<String>) {
        #[cfg(target_os = "linux")]
        {
            let into = self.routes.lock().unwrap().values().filter(|(s, _)| s == slot).find_map(|(_, f)| f.screen());
            let why = self.unrouted.lock().unwrap().values().find(|(s, _, _)| s == slot).map(|(_, why, _)| why.clone());
            (into, why)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = slot;
            (None, None)
        }
    }

    fn set_session(&self, source: &str, active: bool, ttl: Option<Duration>) {
        let _pass = self.reconciling.lock().unwrap();
        self.heard.lock().unwrap().insert(source.to_string(), Instant::now());
        {
            let mut cfg = self.cfg.lock().unwrap();
            if !cfg.remembered_sources().iter().any(|s| s == source) {
                cfg.remembered_sources().push(source.to_string());
                if let Err(e) = self.save(&cfg) {
                    eprintln!("could not note {source} as a stream source: {e:#}");
                }
            }
        }
        let mut leases = self.leases.lock().unwrap();
        if active {
            leases.insert(source.to_string(), ttl.map(|t| Instant::now() + t));
        } else {
            leases.remove(source);
        }
        drop(leases);
        self.apply_session();
    }

    /// Checks the host apps of hooked sources: one that answers keeps its source watching; one that
    /// stopped answering mid-stream (it crashed, so its hook never said stop) ends its stream. Also
    /// follows the apps Plugover watches without a hook (Parsec, Steam Remote Play).
    fn probe_hosts(&self) {
        let mut misses: HashMap<String, u32> = HashMap::new();
        let mut apps = crate::follow::find();
        loop {
            for app in &mut apps {
                let Some(streaming) = app.poll() else { continue };
                let source = app.source();
                let held = self.leases.lock().unwrap().contains_key(source);
                if streaming != held {
                    eprintln!("{} {}", app.title(), if streaming { "started a stream" } else { "stopped streaming" });
                    self.set_session(source, streaming, None);
                } else if self.cfg.lock().unwrap().remembered_sources().iter().any(|s| s == source) {
                    // An app that streamed here before keeps this host watching while it can be asked.
                    // Only having it installed does not: Steam is on nearly every gaming PC.
                    self.heard.lock().unwrap().insert(source.to_string(), Instant::now());
                }
            }
            let watch = self.cfg.lock().unwrap().watch.clone();
            for (source, port) in watch {
                let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
                if std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(500)).is_ok() {
                    self.heard.lock().unwrap().insert(source.clone(), Instant::now());
                    misses.remove(&source);
                    continue;
                }
                let n = misses.entry(source.clone()).or_default();
                *n += 1;
                if *n == PROBE_MISSES && self.leases.lock().unwrap().contains_key(&source) {
                    eprintln!("{source} stopped answering on port {port}; its stream is over");
                    let _pass = self.reconciling.lock().unwrap();
                    self.leases.lock().unwrap().remove(&source);
                    self.apply_session();
                }
            }
            thread::sleep(PROBE);
        }
    }

    /// Sources whose lease holds, dropping the ones that ran out.
    fn sessions(&self) -> Vec<String> {
        let mut leases = self.leases.lock().unwrap();
        leases.retain(|_, until| until.is_none_or(|u| Instant::now() < u));
        let mut held: Vec<String> = leases.keys().cloned().collect();
        held.sort();
        held
    }

    /// Starts or ends the stream when the leases say so. The caller holds `reconciling`.
    fn apply_session(&self) {
        let active = !self.sessions().is_empty();
        // Sources repeat the current state now and then; only a change does anything.
        if self.session.swap(active, Ordering::SeqCst) == active {
            return;
        }
        self.failed.lock().unwrap().clear();
        if active {
            self.held.lock().unwrap().clear();
            return;
        }
        let watching = self.watching();
        let slots: Vec<Slot> = self.attached.lock().unwrap().keys().cloned().collect();
        let going: Vec<Slot> = slots.into_iter().filter(|s| !self.stays(s, watching)).collect();
        self.unplug(|slot| going.contains(slot));
    }

    /// Unplugs the attached devices `which` picks. One that will not unplug stays tracked, so it
    /// still shows and can be unplugged again.
    fn unplug(&self, which: impl Fn(&Slot) -> bool) {
        self.attached.lock().unwrap().retain(|slot, a| {
            if !which(slot) {
                return true;
            }
            match vhci::detach(&self.sys, a) {
                Ok(()) => {
                    self.manual.lock().unwrap().remove(slot);
                    false
                }
                Err(e) => {
                    eprintln!("could not unplug {}: {e:#}", slot.1);
                    true
                }
            }
        });
    }

    pub fn handle(self: &Arc<Self>, req: Request) -> Result<Value> {
        Ok(match req {
            Request::Status => reply(self.status())?,
            Request::Discover => {
                let paired: HashSet<String> = self.cfg.lock().unwrap().relay.iter().map(|r| r.id.clone()).collect();
                let relays = discovery::browse(Duration::from_secs(3))?
                    .into_iter()
                    .map(|f| api::Found {
                        paired: paired.contains(&f.id),
                        // Relays announce docker bridges and VPNs too; this is the one that answers.
                        address: discovery::reachable(&f.addresses).map(|a| a.to_string()),
                        addresses: f.addresses.iter().map(|a| a.to_string()).collect(),
                        id: f.id,
                        name: f.name,
                    })
                    .collect();
                reply(api::Discovered { relays })?
            }
            Request::Pair { address, code } => {
                let relay = host::pair(&address, &self.name, &code)?;
                let mut cfg = self.cfg.lock().unwrap();
                cfg.relay.retain(|r| r.id != relay.id);
                cfg.relay.push(relay.clone());
                self.save(&cfg)?;
                drop(cfg);
                self.watch(relay.id.clone());
                json!({ "ok": true, "relay": { "id": relay.id, "name": relay.name } })
            }
            Request::Forget { relay } => {
                let id = self.pick(Some(&relay))?.id;
                let mut cfg = self.cfg.lock().unwrap();
                cfg.relay.retain(|r| r.id != id);
                cfg.auto.retain(|a| a.relay != id);
                cfg.device_name.retain(|n| n.relay != id);
                self.save(&cfg)?;
                drop(cfg);
                // Its devices go back too, even mid-stream.
                let _pass = self.reconciling.lock().unwrap();
                self.unplug(|(r, _)| *r == id);
                self.live.lock().unwrap().remove(&id);
                self.errors.lock().unwrap().remove(&id);
                self.held.lock().unwrap().retain(|(r, _)| *r != id);
                self.failed.lock().unwrap().retain(|(r, _), _| *r != id);
                self.tracks.lock().unwrap().retain(|(r, _), _| *r != id);
                json!({ "ok": true })
            }
            Request::Auto { relay, device, on, port, mode } => {
                let (relay, device) = (self.pick(relay.as_deref())?.id, state::device_id(&device)?);
                let pass = self.reconciling.lock().unwrap();
                let mut cfg = self.cfg.lock().unwrap();
                anyhow::ensure!(cfg.relay.iter().any(|r| r.id == relay), "relay is no longer paired");
                let live = self.live.lock().unwrap();
                let devices = live.get(&relay).map_or(&[][..], |s| &s.devices[..]);
                let targets = devices.iter().filter(|d| d.id == device && port.as_ref().is_none_or(|p| *p == d.busid));
                let title = targets.clone().next().map(|d| d.identity.title.clone());
                let busids: Vec<String> = targets.map(|d| d.busid.clone()).collect();
                let same_model = cfg.auto.iter().filter(|a| a.relay == relay && a.device == device);
                anyhow::ensure!(
                    on || port.is_none() || !same_model.clone().any(|a| a.port.is_none()),
                    "auto-connect covers all ports for this model; turn off the all-ports rule or narrow it first"
                );
                let mode = mode
                    .or_else(|| {
                        same_model.clone().find(|a| a.port == port)
                            .or_else(|| same_model.clone().find(|a| a.port.is_none() || port.is_none()))
                            .map(|a| a.mode)
                    })
                    .unwrap_or_default();
                let previous = cfg.auto.clone();
                state::set_auto(&mut cfg.auto, AutoRule { relay: relay.clone(), device: device.clone(), title, port, mode }, on);
                let released: Vec<Slot> = {
                    let (attached, tracks) = (self.attached.lock().unwrap(), self.tracks.lock().unwrap());
                    attached.keys().filter(|slot| {
                        let (r, busid) = slot;
                        if r != &relay {
                            return false;
                        }
                        // Attachments can outlast an interrupted control connection to the relay.
                        let id = tracks.get(*slot).map(|t| t.id.as_str()).filter(|id| !id.is_empty())
                            .or_else(|| devices.iter().find(|d| d.busid == *busid).map(|d| d.id.as_str()));
                        id == Some(device.as_str())
                            && previous.iter().any(|a| a.matches_port(r, &device, busid))
                            && !cfg.auto.iter().any(|a| a.matches_port(r, &device, busid))
                    }).cloned().collect()
                };
                drop(live);
                if let Err(error) = self.save(&cfg) {
                    cfg.auto = previous;
                    return Err(error);
                }
                drop(cfg);
                if on {
                    // Turning it on again undoes unplugging it by hand.
                    self.held.lock().unwrap().retain(|(r, b)| !(*r == relay && busids.contains(b)));
                }
                // A narrower rule gives back automatic connections outside its scope.
                if !released.is_empty() {
                    let manual = self.manual.lock().unwrap().clone();
                    self.unplug(|slot| released.contains(slot) && (!on || !manual.contains(slot)));
                }
                drop(pass);
                self.reconcile();
                json!({ "ok": true })
            }
            Request::Label { device, name, kind } => {
                let mut cfg = self.cfg.lock().unwrap();
                state::set_label(&mut cfg.label, state::device_id(&device)?, name, kind)?;
                self.save(&cfg)?;
                json!({ "ok": true })
            }
            Request::DeviceName { relay, device, busid, serial, name } => {
                let device = state::device_id(&device)?;
                let name = state::checked_name(name)?;
                let mut cfg = self.cfg.lock().unwrap();
                anyhow::ensure!(cfg.relay.iter().any(|r| r.id == relay), "no such relay");
                if name.is_some() {
                    let live = self.live.lock().unwrap();
                    let devices = live.get(&relay).map_or(&[][..], |s| &s.devices[..]);
                    let current = devices.iter().find(|d| d.busid == busid && d.id == device);
                    anyhow::ensure!(
                        current.is_some_and(|d| device_name(&cfg.device_name, &relay, d, devices).0 == serial),
                        "the device changed or is unavailable; refresh and try again"
                    );
                }
                let mut names = cfg.device_name.clone();
                names.retain(|n| !n.matches(&relay, &device, &busid, serial.as_deref()));
                if let Some(name) = name {
                    names.push(state::DeviceName { relay, device, port: busid, serial, name });
                }
                if names != cfg.device_name {
                    let previous = std::mem::replace(&mut cfg.device_name, names);
                    if let Err(error) = self.save(&cfg) {
                        cfg.device_name = previous;
                        return Err(error);
                    }
                }
                json!({ "ok": true })
            }
            Request::Desktop { device, on } => {
                anyhow::ensure!(
                    cfg!(target_os = "linux"),
                    "only Linux hosts pass keyboards and mice into a stream's own screen; here they always reach the game"
                );
                let device = state::device_id(&device)?;
                let mut cfg = self.cfg.lock().unwrap();
                cfg.desktop.retain(|d| *d != device);
                if on {
                    cfg.desktop.push(device);
                }
                self.save(&cfg)?;
                drop(cfg);
                self.reconcile();
                json!({ "ok": true })
            }
            Request::Attach { relay, busid, take, expected_owner } => {
                let relay = self.pick(relay.as_deref())?;
                let encrypt = self.encryption(&relay)?;
                if take {
                    host::request(&relay, &proto::RelayRequest::Take { busid: busid.clone(), expected_owner })?;
                }
                let _pass = self.reconciling.lock().unwrap();
                let a = vhci::attach(&self.sys, &relay, &busid, encrypt)?;
                let port = a.port;
                let slot = (relay.id.clone(), busid);
                let dev = self.live.lock().unwrap().get(&relay.id).and_then(|s| s.devices.iter().find(|d| d.busid == slot.1).map(|d| (d.id.clone(), d.serial.clone())));
                let (id, serial) = dev.unwrap_or_default();
                self.plugged(&slot, &id, serial, false);
                self.held.lock().unwrap().remove(&slot);
                self.attached.lock().unwrap().insert(slot.clone(), a);
                self.manual.lock().unwrap().insert(slot);
                json!({ "ok": true, "port": port })
            }
            Request::Detach { relay, busid } => {
                let slot = (self.pick(relay.as_deref())?.id, busid);
                let _pass = self.reconciling.lock().unwrap();
                let mut attached = self.attached.lock().unwrap();
                let a = attached.get(&slot).context("that device is not plugged in here")?;
                vhci::detach(&self.sys, a)?;
                attached.remove(&slot);
                drop(attached);
                self.manual.lock().unwrap().remove(&slot);
                self.held.lock().unwrap().insert(slot);
                json!({ "ok": true })
            }
            Request::Unencrypted { relay, on } => {
                let id = self.pick(Some(&relay))?.id;
                let mut cfg = self.cfg.lock().unwrap();
                let r = cfg.relay.iter_mut().find(|r| r.id == id).context("no such relay")?;
                anyhow::ensure!(!on || !r.encrypts, "{} encrypts, so there is nothing to allow", r.name);
                r.unencrypted = on;
                self.save(&cfg)?;
                drop(cfg);
                if !on {
                    // What is plugged in from it now is unencrypted.
                    let _pass = self.reconciling.lock().unwrap();
                    self.unplug(|(r, _)| *r == id);
                }
                self.reconcile();
                json!({ "ok": true })
            }
            Request::BtScan { relay, seconds } => self.bluetooth(relay, proto::RelayRequest::BtScan { seconds })?,
            Request::BtPair { relay, address } => self.bluetooth(relay, proto::RelayRequest::BtPair { address })?,
            Request::BtForget { relay, address } => self.bluetooth(relay, proto::RelayRequest::BtForget { address })?,
            Request::UiCode => json!({ "ok": true, "code": self.ui.code(), "port": crate::ui::PORT }),
            Request::Session { active, source, ttl, port } => {
                anyhow::ensure!(!source.is_empty() && source.len() <= 64, "a session source needs a short name");
                anyhow::ensure!(ttl.is_none_or(|t| (1..=86_400).contains(&t)), "a session lease lasts 1 second to 1 day");
                if let Some(port) = port {
                    let mut cfg = self.cfg.lock().unwrap();
                    if cfg.watch.insert(source.clone(), port) != Some(port) {
                        self.save(&cfg)?;
                    }
                }
                self.set_session(&source, active, ttl.map(Duration::from_secs));
                self.reconcile();
                json!({ "ok": true })
            }
        })
    }

    fn status(&self) -> api::Status {
        let (watching, sessions) = (self.watching(), self.sessions());
        let (cfg, live) = (self.cfg.lock().unwrap(), self.live.lock().unwrap());
        let (attached, errors) = (self.attached.lock().unwrap(), self.errors.lock().unwrap());
        let (failed, rtt, tracks) = (self.failed.lock().unwrap(), self.rtt.lock().unwrap(), self.tracks.lock().unwrap());
        let relays = cfg
            .relay
            .iter()
            .map(|r| {
                let snap = live.get(&r.id);
                let devices = snap.map_or(&[][..], |s| &s.devices[..]);
                let rules: Vec<&AutoRule> = cfg.auto.iter().filter(|a| a.relay == r.id).collect();
                let status = |d: &proto::DeviceInfo| {
                    let slot = (r.id.clone(), d.busid.clone());
                    let here = attached.contains_key(&slot);
                    let matched: Vec<&&AutoRule> = rules.iter().filter(|a| a.matches(&r.id, d)).collect();
                    let mut info = d.clone();
                    let model = cfg.model_label(&d.id);
                    let model_name = model.and_then(|l| l.name.clone());
                    let model_kind = model.and_then(|l| l.kind);
                    info.identity.relabel(model_name.as_deref(), model_kind);
                    let (serial, named) = device_name(&cfg.device_name, &r.id, d, devices);
                    let instance_name = named.map(|n| n.name.clone());
                    let model_title = info.identity.title.clone();
                    info.identity.relabel(instance_name.as_deref(), None);
                    let (routed, route_error) = if attached.contains_key(&slot) { self.routing(&slot) } else { (None, None) };
                    api::DeviceStatus {
                        routed,
                        route_error,
                        desktop: cfg.desktop.contains(&d.id),
                        info,
                        state: if here { api::DeviceState::Here } else if d.lent_to.is_some() { api::DeviceState::Elsewhere } else { api::DeviceState::Free },
                        auto: !matched.is_empty(),
                        auto_port: matched.iter().any(|a| a.port.is_some()),
                        mode: matched.first().map(|a| a.mode).unwrap_or_default(),
                        always: matched.iter().any(|a| a.always(watching)),
                        error: (!here && d.lent_to.is_none() && !matched.is_empty()).then(|| failed.get(&slot).map(|(why, _)| why.clone())).flatten(),
                        history: tracks.get(&slot).map(|t| t.history.clone()).unwrap_or_default(),
                        naming: Some(api::DeviceNaming { model_title, model_name, model_kind, instance_name, serial }),
                    }
                };
                // Chosen devices that are not plugged into the relay (or it is offline).
                let missing = rules
                    .iter()
                    .filter(|a| !devices.iter().any(|d| a.matches(&r.id, d)))
                    .map(|a| {
                        let mut names = cfg.device_name.iter().filter(|n| {
                            n.relay == r.id && n.device == a.device && a.port.as_ref() == Some(&n.port)
                                && n.serial.as_ref().is_none_or(|serial| {
                                    !devices.iter().any(|d| d.id == n.device && d.serial.as_ref() == Some(serial))
                                })
                        });
                        let named = names.clone().find(|n| n.serial.is_none())
                            .or_else(|| names.next().filter(|_| names.next().is_none()));
                        // Rules from before 0.5 have no title; the catalog may still know the device.
                        let title = named.map(|n| n.name.clone())
                            .or_else(|| cfg.model_label(&a.device).and_then(|l| l.name.clone()))
                            .or_else(|| a.title.clone())
                            .or_else(|| catalog::catalog_title(&a.device))
                            .unwrap_or_else(|| a.device.clone());
                        api::Missing {
                            id: a.device.clone(),
                            title,
                            port: a.port.clone(),
                            named: named.map(|n| api::MissingName { port: n.port.clone(), serial: n.serial.clone() }),
                        }
                    })
                    .collect();
                api::RelayStatus {
                    id: r.id.clone(),
                    name: r.name.clone(),
                    address: r.address.clone(),
                    online: snap.is_some(),
                    encrypted: r.encrypts,
                    unencrypted: r.unencrypted && !r.encrypts,
                    version: snap.map(|s| s.version.clone()).filter(|v| !v.is_empty()),
                    error: if snap.is_some() { None } else { errors.get(&r.id).cloned() },
                    latency: snap.and(rtt.get(&r.id)).and_then(|s| api::Latency::of(&s.iter().copied().collect::<Vec<_>>())),
                    devices: devices.iter().map(status).collect(),
                    missing,
                }
            })
            .collect();
        api::Status { host: self.name.clone(), version: proto::VERSION.into(), session: self.session.load(Ordering::SeqCst), sessions, watching, bluetooth: cfg!(target_os = "linux"), screens: cfg!(target_os = "linux"), update: self.update.lock().unwrap().clone(), relays }
    }

    fn answer<S: Read + Write>(me: &Arc<Self>, read: S, mut write: S, token: Option<&str>) {
        let mut read = BufReader::new(read);
        while let Ok(Some(line)) = proto::read_line(&mut read) {
            let reply = match parse(&line, token) {
                Ok(req) => me.handle(req).unwrap_or_else(|e| json!({ "ok": false, "error": format!("{e:#}") })),
                Err(e) => json!({ "ok": false, "error": format!("{e:#}") }),
            };
            if proto::send(&mut write, &reply).is_err() {
                break;
            }
        }
    }

    #[cfg(unix)]
    fn serve(self: Arc<Self>, socket: &Path) -> Result<()> {
        use std::os::unix::{fs::PermissionsExt, net::UnixListener};
        if let Some(dir) = socket.parent() {
            fs::create_dir_all(dir)?;
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
        }
        let _ = fs::remove_file(socket);
        let listener = UnixListener::bind(socket).with_context(|| format!("listening on {}", socket.display()))?;
        eprintln!("host daemon listening on {}", socket.display());
        for conn in listener.incoming() {
            let Ok(conn) = conn else { continue };
            let me = self.clone();
            thread::spawn(move || {
                if let Ok(w) = conn.try_clone() {
                    Self::answer(&me, conn, w, None);
                }
            });
        }
        Ok(())
    }

    /// `endpoint` is the token file; the token changes every start.
    #[cfg(windows)]
    fn serve(self: Arc<Self>, endpoint: &Path) -> Result<()> {
        use std::net::TcpListener;
        let token = hex::encode(crate::auth::random::<16>());
        if let Some(dir) = endpoint.parent() {
            fs::create_dir_all(dir)?;
        }
        fs::write(endpoint, &token).with_context(|| format!("writing {}", endpoint.display()))?;
        let me = self.clone();
        thread::spawn(move || {
            pipe::serve(PIPE, move |conn| {
                Self::answer(&me, conn.try_clone()?, conn, None);
                Ok(())
            })
        });
        let listener = TcpListener::bind(("127.0.0.1", API_PORT)).with_context(|| format!("listening on 127.0.0.1:{API_PORT}"))?;
        eprintln!("host daemon listening on 127.0.0.1:{API_PORT}");
        for conn in listener.incoming() {
            let Ok(conn) = conn else { continue };
            let (me, token) = (self.clone(), token.clone());
            thread::spawn(move || {
                if let Ok(w) = conn.try_clone() {
                    Self::answer(&me, conn, w, Some(&token));
                }
            });
        }
        Ok(())
    }
}

/// Loopback port of the Windows daemon.
#[cfg(windows)]
pub const API_PORT: u16 = 7276;

/// The punktfunk plugin's way in on Windows. Its AppContainer may not dial loopback or read the
/// token file, so the pipe's DACL is the credential and a connection needs no token.
#[cfg(windows)]
const PIPE: &str = r"\\.\pipe\plugover-host";

#[cfg(windows)]
mod pipe {
    use std::{fs::File, io, os::windows::io::FromRawHandle, ptr, thread, time::Duration};
    use windows_sys::Win32::{
        Foundation::{CloseHandle, ERROR_PIPE_CONNECTED, GetLastError, INVALID_HANDLE_VALUE},
        Security::{Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES},
        Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX},
        System::Pipes::{ConnectNamedPipe, CreateNamedPipeW, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT},
    };

    /// The AppContainer package of punktfunk's `plugover` plugin, `punktfunk.plugin.plugover`:
    /// SHA-256 of the name, as DeriveAppContainerSidFromAppContainerName does it.
    pub const PLUGIN_PACKAGE: &str = "S-1-15-2-3093401335-3438397477-857312563-170376724-2049339860-2358065718-3618005222";

    /// SYSTEM and Administrators in full; LOCAL SERVICE, the account punktfunk runs plugins
    /// under, and the plugin's package may read and write. A container's open is checked
    /// against both of the last, as punktfunk's own plugin pipe is.
    pub fn sddl() -> String {
        format!("D:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FRFW;;;LS)(A;;FRFW;;;{PLUGIN_PACKAGE})")
    }

    /// Accept connections on `name` forever, each handed to `answer` on its own thread.
    pub fn serve(name: &str, answer: impl Fn(File) -> io::Result<()> + Clone + Send + 'static) {
        let name: Vec<u16> = name.encode_utf16().chain([0]).collect();
        let sddl: Vec<u16> = sddl().encode_utf16().chain([0]).collect();
        let mut sd: PSECURITY_DESCRIPTOR = ptr::null_mut();
        // SAFETY: `sddl` is NUL-terminated. `sd` is never freed: every instance is made from it.
        if unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl.as_ptr(), 1, &mut sd, ptr::null_mut()) } == 0 {
            eprintln!("the punktfunk plugin's pipe is not served: {}", io::Error::last_os_error());
            return;
        }
        let sa = SECURITY_ATTRIBUTES { nLength: size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: sd, bInheritHandle: 0 };
        let mut first = true;
        loop {
            // The first instance claims the name, so nobody else can serve it to the plugin.
            let flags = PIPE_ACCESS_DUPLEX | if first { FILE_FLAG_FIRST_PIPE_INSTANCE } else { 0 };
            // SAFETY: `name` is NUL-terminated and `sa` outlives the call, which copies it.
            let h = unsafe { CreateNamedPipeW(name.as_ptr(), flags, PIPE_TYPE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS, PIPE_UNLIMITED_INSTANCES, 4096, 4096, 0, &sa) };
            if h == INVALID_HANDLE_VALUE {
                eprintln!("the punktfunk plugin's pipe: {}; trying again", io::Error::last_os_error());
                thread::sleep(Duration::from_secs(5));
                continue;
            }
            first = false;
            // SAFETY: `h` is a pipe instance this loop owns.
            if unsafe { ConnectNamedPipe(h, ptr::null_mut()) } == 0 && unsafe { GetLastError() } != ERROR_PIPE_CONNECTED {
                unsafe { CloseHandle(h) };
                continue;
            }
            // SAFETY: the handle is ours alone; the File closes it.
            let conn = unsafe { File::from_raw_handle(h) };
            let answer = answer.clone();
            thread::spawn(move || answer(conn));
        }
    }
}

/// A browser can be tricked into sending lines to a loopback port, but it cannot read the token
/// file, so on Windows every request must carry the token.
fn parse(line: &str, token: Option<&str>) -> Result<Request> {
    let v: Value = serde_json::from_str(line).context("bad request")?;
    if let Some(want) = token
        && v.get("token").and_then(Value::as_str) != Some(want)
    {
        bail!("missing or wrong token");
    }
    serde_json::from_value(v).context("bad request")
}

/// A successful answer: the value's fields plus `"ok": true`.
fn reply(value: impl Serialize) -> Result<Value> {
    let mut v = serde_json::to_value(value)?;
    v["ok"] = json!(true);
    Ok(v)
}

/// One request to a running daemon, read back as `T`.
pub fn ask<T: serde::de::DeserializeOwned>(endpoint: &Path, req: &Request) -> Result<T> {
    serde_json::from_value(call(endpoint, req)?).context("the host daemon answered something this version does not understand")
}

/// One request to a running daemon.
pub fn call(endpoint: &Path, req: &Request) -> Result<Value> {
    #[allow(unused_mut)]
    let mut msg = serde_json::to_value(req)?;
    #[cfg(unix)]
    let s = std::os::unix::net::UnixStream::connect(endpoint)
        .with_context(|| format!("the host daemon is not running ({}); start `plugover host`", endpoint.display()))?;
    #[cfg(windows)]
    let s = {
        let token = fs::read_to_string(endpoint).map_err(|e| match e.kind() {
            std::io::ErrorKind::PermissionDenied => anyhow::anyhow!("only administrators may talk to the host daemon; run this in an administrator terminal"),
            _ => anyhow::anyhow!("the host daemon is not running (no token file)"),
        })?;
        msg["token"] = json!(token.trim());
        std::net::TcpStream::connect(("127.0.0.1", API_PORT)).context("the host daemon is not running")?
    };
    let mut w = s.try_clone()?;
    proto::send(&mut w, &msg)?;
    let reply: Value = proto::recv(&mut BufReader::new(s))?;
    if reply["ok"] != json!(true) {
        bail!("{}", reply["error"].as_str().unwrap_or("request failed"));
    }
    Ok(reply)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_need_the_token_when_one_is_set() {
        assert!(matches!(parse(r#"{"op":"status"}"#, None), Ok(Request::Status)));
        assert!(parse(r#"{"op":"status"}"#, Some("t")).is_err());
        assert!(parse(r#"{"op":"status","token":"x"}"#, Some("t")).is_err());
        assert!(matches!(parse(r#"{"op":"status","token":"t"}"#, Some("t")), Ok(Request::Status)));
        assert!(parse("POST / HTTP/1.1", Some("t")).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn the_plugin_pipe_answers_and_names_the_plugins_package() {
        use sha2::{Digest, Sha256};
        use std::io::BufRead;
        let name: Vec<u8> = "punktfunk.plugin.plugover".encode_utf16().flat_map(u16::to_le_bytes).collect();
        let hash = Sha256::digest(&name);
        let rids: Vec<String> = hash[..28].as_chunks::<4>().0.iter().map(|c| u32::from_le_bytes(*c).to_string()).collect();
        assert_eq!(pipe::PLUGIN_PACKAGE, format!("S-1-15-2-{}", rids.join("-")));

        let name = r"\\.\pipe\plugover-test";
        thread::spawn(move || {
            pipe::serve(name, |conn| {
                let mut line = String::new();
                BufReader::new(conn.try_clone()?).read_line(&mut line)?;
                (&conn).write_all(line.to_uppercase().as_bytes())
            })
        });
        for _ in 0..2 {
            let mut conn = loop {
                match fs::OpenOptions::new().read(true).write(true).open(name) {
                    Ok(c) => break c,
                    Err(_) => thread::sleep(Duration::from_millis(50)),
                }
            };
            conn.write_all(b"hi\n").unwrap();
            let mut back = String::new();
            BufReader::new(conn).read_line(&mut back).unwrap();
            assert_eq!(back, "HI\n");
        }
    }

    #[test]
    fn a_stream_runs_while_any_source_holds_a_lease() {
        // What the punktfunk plugin of 0.6 sends: no source, so it is punktfunk's lease.
        let Ok(Request::Session { active: true, source, ttl: None, port: None }) = parse(r#"{"op":"session","active":true}"#, None) else { panic!() };
        assert_eq!(source, "punktfunk");

        let root = std::env::temp_dir().join(format!("plugover-hostd-leases-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let d = Daemon::open(root.join("sys"), root.join("host.toml")).unwrap();
        let on = || d.session.load(Ordering::SeqCst);
        let session = |source: &str, active: bool| d.handle(Request::Session { active, source: source.into(), ttl: None, port: None }).unwrap();

        session("punktfunk", true);
        session("cli", true);
        assert_eq!(d.status().sessions, ["cli", "punktfunk"]);
        session("punktfunk", false);
        assert!(on(), "the CLI still says a stream runs");
        session("cli", false);
        assert!(!on() && d.status().sessions.is_empty());

        // A lease that is not renewed ends the stream by itself.
        d.set_session("sunshine", true, Some(Duration::from_millis(50)));
        assert!(on());
        thread::sleep(Duration::from_millis(80));
        d.reconcile();
        assert!(!on(), "the lease ran out");

        assert!(d.handle(Request::Session { active: true, source: String::new(), ttl: None, port: None }).is_err());
        assert!(d.handle(Request::Session { active: true, source: "x".into(), ttl: Some(u64::MAX), port: None }).is_err());
        let _ = fs::remove_dir_all(root);
    }

    fn device(busid: &str, id: &str) -> proto::DeviceInfo {
        proto::DeviceInfo {
            busid: busid.into(),
            id: id.into(),
            identity: catalog::Identity {
                title: "wheel".into(),
                name: "wheel".into(),
                maker: String::new(),
                kind: catalog::Kind::Wheel,
                source: catalog::Source::Unknown,
                blocked: None,
            },
            lent_to: None,
            reserved: false,
            serial: None,
        }
    }

    fn rule(device: &str, mode: state::Mode) -> AutoRule {
        AutoRule { relay: "r".into(), device: device.into(), title: None, port: None, mode }
    }

    #[test]
    fn device_names_follow_a_serial_or_stay_with_the_port() {
        let root = std::env::temp_dir().join(format!("plugover-hostd-naming-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let open = || Daemon::open(root.join("sys"), root.join("host.toml")).unwrap();
        let daemon = open();
        let model = "046d:c24f";
        let paired = |id: &str| PairedRelay {
            id: id.into(), name: id.into(), address: "127.0.0.1:1".into(), host_id: "00".repeat(8),
            key: "00".repeat(32), encrypts: true, unencrypted: false,
        };
        {
            let mut cfg = daemon.cfg.lock().unwrap();
            cfg.relay = vec![paired("r"), paired("other")];
            cfg.auto.push(AutoRule { port: Some("1-2".into()), ..rule(model, state::Mode::Session) });
        }
        daemon.handle(Request::Label { device: model.into(), name: Some("Wheel model".into()), kind: Some(catalog::Kind::Other) }).unwrap();

        let serial_pad = |busid: &str| proto::DeviceInfo { serial: Some("ABCD".into()), ..device(busid, model) };
        let snapshot = |relay: &str, devices: Vec<_>| Snapshot {
            relay_id: relay.into(), relay: relay.into(), devices, version: proto::VERSION.into(), encrypted: true,
        };
        let show = |relay, devices| daemon.live.lock().unwrap().insert(relay, devices);
        show("r".into(), snapshot("r", vec![serial_pad("1-1"), device("1-2", model)]));
        show("other".into(), snapshot("other", vec![serial_pad("1-1"), device("1-2", model)]));
        let request = |busid: &str, serial: Option<&str>, name: Option<&str>| Request::DeviceName {
            relay: "r".into(), device: model.into(), busid: busid.into(), serial: serial.map(str::to_string), name: name.map(str::to_string),
        };
        let titles = || daemon.status().relays.into_iter().map(|relay| {
            relay.devices.into_iter().map(|d| d.info.identity.title).collect::<Vec<_>>()
        }).collect::<Vec<_>>();

        assert!(daemon.handle(request("1-1", None, Some("Stale selection"))).is_err());
        daemon.handle(request("1-1", Some("ABCD"), Some(" Driver "))).unwrap();
        daemon.handle(request("1-2", None, Some("Passenger"))).unwrap();
        assert_eq!(titles(), [vec!["Driver", "Passenger"], vec!["Wheel model", "Wheel model"]]);

        show("r".into(), snapshot("r", vec![device("1-1", model), serial_pad("1-3"), device("1-2", "045e:028e")]));
        assert_eq!(titles()[0], ["Wheel model", "Driver", "wheel"]);
        assert_eq!(daemon.status().relays[0].devices[1].info.identity.kind, catalog::Kind::Other);

        let restarted = open();
        let missing = &restarted.status().relays[0].missing[0];
        assert_eq!(missing.title, "Passenger");
        assert!(missing.named.is_some());
        restarted.handle(request("1-2", None, None)).unwrap();
        assert_eq!(restarted.status().relays[0].missing[0].title, "Wheel model");
        assert_eq!(restarted.cfg.lock().unwrap().device_name.len(), 1);
        restarted.handle(request("1-3", Some("ABCD"), None)).unwrap();
        assert!(restarted.cfg.lock().unwrap().device_name.is_empty());
        assert_eq!(restarted.cfg.lock().unwrap().label[0].name.as_deref(), Some("Wheel model"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn duplicate_and_placeholder_serials_fall_back_to_ports() {
        let first = proto::DeviceInfo { serial: Some("SAME".into()), ..device("1-1", "046d:c24f") };
        let second = proto::DeviceInfo { busid: "1-2".into(), ..first.clone() };
        let saved = |port: &str, serial: Option<&str>| state::DeviceName {
            relay: "r".into(), device: first.id.clone(), port: port.into(),
            serial: serial.map(str::to_string), name: port.into(),
        };
        let serial_name = saved("1-1", Some("SAME"));
        let port_name = saved("1-2", None);
        let names = [serial_name.clone(), port_name.clone()];
        let twins = &[first.clone(), second.clone()];
        assert_eq!(device_name(&names, "r", &first, twins), (None, None));
        assert_eq!(device_name(&names, "r", &second, twins), (None, Some(&port_name)));
        assert_eq!(device_name(&names, "r", &second, std::slice::from_ref(&second)), (None, Some(&port_name)));
        assert_eq!(device_name(std::slice::from_ref(&port_name), "r", &second, std::slice::from_ref(&second)), (None, Some(&port_name)));

        assert_eq!(naming_serial(&first, std::slice::from_ref(&first)).as_deref(), Some("SAME"));
        let placeholder = proto::DeviceInfo { serial: Some(" 00000 ".into()), ..first };
        assert_eq!(naming_serial(&placeholder, std::slice::from_ref(&placeholder)), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn narrowing_auto_connect_releases_only_connections_that_lose_their_rule() {
        let root = std::env::temp_dir().join(format!("plugover-hostd-auto-ports-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let vhci = root.join("sys/devices/platform/vhci_hcd.0");
        fs::create_dir_all(&vhci).unwrap();
        fs::write(vhci.join("detach"), "").unwrap();
        fs::write(vhci.join("status"), "hub port sta\nhs 1 6\nhs 2 6\nhs 3 6\n").unwrap();
        let d = Daemon::open(root.join("sys"), root.join("host.toml")).unwrap();
        let id = "046d:c24f";
        let devices = (1..=3).map(|p| device(&format!("1-{p}"), id)).collect();
        d.live.lock().unwrap().insert(
            "r".into(),
            Snapshot { relay_id: "r".into(), relay: "rig".into(), devices, version: proto::VERSION.into(), encrypted: true },
        );
        {
            let mut cfg = d.cfg.lock().unwrap();
            cfg.relay.push(PairedRelay { id: "r".into(), name: "rig".into(), address: "127.0.0.1:1".into(), host_id: "00".repeat(8), key: "00".repeat(32), encrypts: true, unencrypted: false });
            cfg.auto.push(rule(id, state::Mode::Always));
        }
        for (port, busid) in [(1, "1-1"), (2, "1-2"), (3, "1-3")] {
            d.attached.lock().unwrap().insert(("r".into(), busid.into()), vhci::Attachment { port, hid: None });
        }
        let slot = |busid: &str| ("r".to_string(), busid.to_string());
        d.manual.lock().unwrap().insert(slot("1-3"));

        let auto = |busid: &str, on, mode| d.handle(Request::Auto {
            relay: Some("r".into()), device: id.into(), on, port: Some(busid.into()), mode,
        });
        assert!(auto("1-1", false, None).is_err(), "a port-specific disable cannot remove a model-wide rule");
        auto("1-1", true, None).unwrap();
        assert_eq!(fs::read_to_string(vhci.join("detach")).unwrap(), "2");
        assert_eq!(d.cfg.lock().unwrap().auto[0].mode, state::Mode::Always, "narrowing preserves the inherited mode");
        assert!(d.attached.lock().unwrap().contains_key(&slot("1-1")));
        assert!(d.attached.lock().unwrap().contains_key(&slot("1-3")), "a manual connection survives narrowing");

        {
            let mut live = d.live.lock().unwrap();
            let reserved = &mut live.get_mut("r").unwrap().devices[1];
            reserved.lent_to = Some(d.name.clone());
            reserved.reserved = true;
        }
        auto("1-2", true, Some(state::Mode::Always)).unwrap();
        let status = d.status();
        let rows = &status.relays[0].devices;
        assert!(rows[0].auto_port && rows[1].auto_port && !rows[2].auto);
        assert!(matches!(rows[1].state, api::DeviceState::Elsewhere), "a reservation is not a local attachment");

        auto("1-1", false, None).unwrap();
        assert_eq!(fs::read_to_string(vhci.join("detach")).unwrap(), "1");
        let cfg: HostConfig = state::load(&root.join("host.toml")).unwrap();
        assert_eq!(cfg.auto.len(), 1);
        assert_eq!(cfg.auto[0].port.as_deref(), Some("1-2"));

        // A missing snapshot is not evidence that a tracked attachment was unplugged.
        d.attached.lock().unwrap().insert(slot("1-2"), vhci::Attachment { port: 2, hid: None });
        d.plugged(&slot("1-2"), id, None, true);
        d.live.lock().unwrap().remove("r");
        auto("1-2", false, None).unwrap();
        assert_eq!(fs::read_to_string(vhci.join("detach")).unwrap(), "2");
        assert!(!d.attached.lock().unwrap().contains_key(&slot("1-2")));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn modes_decide_when_a_device_is_wanted() {
        use state::Mode::*;
        let d = device("1-2", "046d:c24f");
        let want = |mode, session, watching| wanted(&[rule("046d:c24f", mode)], "r", &d, None, session, watching);
        assert_eq!(want(Auto, false, false), Some(true), "nothing reports streams: always");
        assert_eq!(want(Auto, false, true), None, "something does: only during streams");
        assert_eq!(want(Auto, true, true), Some(false));
        assert_eq!(want(Always, false, true), Some(true));
        assert_eq!(want(Session, false, false), None);
        assert_eq!(want(Session, true, false), Some(false));
        assert_eq!(wanted(&[], "r", &d, None, true, true), None, "no rule, nothing to follow");

        // The G29 comes back as another product; it keeps the mode it was plugged in with.
        let track = |always| Track { id: "046d:c294".into(), serial: None, dropped: Some(Instant::now()), always, history: api::History::default() };
        assert_eq!(wanted(&[], "r", &d, Some(&track(true)), false, true), Some(true));
        assert_eq!(wanted(&[], "r", &d, Some(&track(false)), false, true), None);
        assert_eq!(wanted(&[], "r", &d, Some(&track(false)), true, true), Some(false));
    }

    #[test]
    fn sources_that_report_streams_are_remembered() {
        let root = std::env::temp_dir().join(format!("plugover-hostd-watch-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let d = Daemon::open(root.join("sys"), root.join("host.toml")).unwrap();
        assert!(!d.watching(), "a fresh host with nothing reporting streams");
        d.set_session("punktfunk", false, None);
        assert!(d.watching(), "a stop counts too: punktfunk is there, just not streaming");
        assert!(d.status().watching);
        // After a restart it has a while to speak up before auto devices connect outside streams.
        let again = Daemon::open(root.join("sys"), root.join("host.toml")).unwrap();
        assert_eq!(again.cfg.lock().unwrap().sources.as_deref(), Some(&["punktfunk".to_string()][..]));
        assert!(again.watching());
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_stream_ending_keeps_only_what_stays_connected() {
        let root = std::env::temp_dir().join(format!("plugover-hostd-end-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let vhci = root.join("sys/devices/platform/vhci_hcd.0");
        fs::create_dir_all(&vhci).unwrap();
        fs::write(vhci.join("detach"), "").unwrap();
        let d = Daemon::open(root.join("sys"), root.join("host.toml")).unwrap();
        let devices = vec![device("1-1", "046d:c24f"), device("1-2", "046d:c266"), device("1-3", "0eb7:0e04")];
        d.live.lock().unwrap().insert(
            "r".into(),
            Snapshot { relay_id: "r".into(), relay: "rig".into(), devices, version: proto::VERSION.into(), encrypted: true },
        );
        d.cfg.lock().unwrap().auto = vec![rule("046d:c24f", state::Mode::Always), rule("046d:c266", state::Mode::Auto)];
        for (port, busid) in [(1, "1-1"), (2, "1-2"), (3, "1-3")] {
            d.attached.lock().unwrap().insert(("r".into(), busid.into()), vhci::Attachment { port, hid: None });
        }
        d.manual.lock().unwrap().insert(("r".into(), "1-3".into()));

        d.set_session("punktfunk", true, None);
        d.set_session("punktfunk", false, None);
        let left: Vec<String> = d.attached.lock().unwrap().keys().map(|(_, b)| b.clone()).collect();
        assert_eq!(left, ["1-1"], "the always wheel stays; auto (punktfunk watches) and by-hand go back");
        assert!(d.manual.lock().unwrap().is_empty());
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn keyboards_go_into_a_streams_own_screen_only_during_it() {
        use std::os::unix::{fs::symlink, net::UnixListener};
        let root = std::env::temp_dir().join(format!("plugover-hostd-route-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let (sys, runtime) = (root.join("sys"), root.join("run"));
        let vhci = sys.join("devices/platform/vhci_hcd.0");
        fs::create_dir_all(&vhci).unwrap();
        fs::write(vhci.join("status"), "hub port sta spd dev      sockfd local_busid\nhs  0003 006 002 00030002 000003 3-1\n").unwrap();
        // A keyboard node that sysfs lists but /dev/input does not have, so passing it on fails.
        let input = vhci.join("usb3/3-1/3-1:1.0/0003:046D:C31C.0001/input/input9999");
        fs::create_dir_all(input.join("capabilities")).unwrap();
        fs::write(input.join("capabilities/key"), "fffffffffffffffe").unwrap();
        fs::create_dir_all(sys.join("class/input")).unwrap();
        symlink(&input, sys.join("class/input/event9999")).unwrap();
        symlink(&input, input.join("device")).unwrap();
        fs::create_dir_all(&runtime).unwrap();
        let _screen = UnixListener::bind(runtime.join("gamescope-0-ei")).unwrap();

        let mut d = Daemon::open(sys, root.join("host.toml")).unwrap();
        Arc::get_mut(&mut d).unwrap().runtime = runtime;
        let relay = PairedRelay { id: "r".into(), name: "rig".into(), address: "127.0.0.1:1".into(), host_id: "00".repeat(8), key: "00".repeat(32), encrypts: true, unencrypted: false };
        d.cfg.lock().unwrap().relay.push(relay);
        let mut kbd = device("1-2", "046d:c31c");
        kbd.identity.kind = catalog::Kind::Keyboard;
        d.live.lock().unwrap().insert("r".into(), Snapshot { relay_id: "r".into(), relay: "rig".into(), devices: vec![kbd], version: proto::VERSION.into(), encrypted: true });
        d.attached.lock().unwrap().insert(("r".into(), "1-2".into()), vhci::Attachment { port: 3, hid: None });
        let status = || d.status().relays[0].devices[0].clone();

        d.route();
        assert!(d.routes.lock().unwrap().is_empty(), "no stream: the desktop keeps it");
        d.set_session("punktfunk", true, None);
        d.route();
        assert_eq!(d.routes.lock().unwrap().keys().collect::<Vec<_>>(), [Path::new("/dev/input/event9999")]);
        while !d.routes.lock().unwrap().values().all(|(_, f)| f.finished()) {
            thread::sleep(Duration::from_millis(10));
        }
        d.route();
        let st = status();
        assert!(st.routed.is_none());
        assert!(st.route_error.as_deref().is_some_and(|e| e.contains("cannot read /dev/input/event9999")), "{:?}", st.route_error);
        d.route();
        assert!(d.routes.lock().unwrap().is_empty(), "a failed node waits before it is tried again");

        // Kept with the desktop: not passed on, and no problem to show.
        d.handle(Request::Desktop { device: "046d:c31c".into(), on: true }).unwrap();
        let st = status();
        assert!(st.desktop && st.route_error.is_none() && d.routes.lock().unwrap().is_empty());
        d.handle(Request::Desktop { device: "046d:c31c".into(), on: false }).unwrap();
        assert!(!status().desktop && d.cfg.lock().unwrap().desktop.is_empty());

        d.set_session("punktfunk", false, None);
        d.route();
        assert!(status().route_error.is_none(), "the stream is over, and with it the problem");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unencrypted_only_when_allowed_and_the_relay_cannot_encrypt() {
        let root = std::env::temp_dir().join(format!("plugover-hostd-crypt-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let d = Daemon::open(root.join("sys"), root.join("host.toml")).unwrap();
        let old = PairedRelay {
            id: "r".into(),
            name: "rig".into(),
            address: "127.0.0.1:1".into(),
            host_id: "00".repeat(8),
            key: "00".repeat(32),
            encrypts: false,
            unencrypted: false,
        };
        d.cfg.lock().unwrap().relay.push(old.clone());
        let err = d.encryption(&old).unwrap_err().to_string();
        assert!(err.contains("cannot encrypt") && err.contains("plugover unencrypted rig on"), "{err}");
        d.handle(Request::Unencrypted { relay: "rig".into(), on: true }).unwrap();
        assert!(!d.encryption(&d.relay("r").unwrap()).unwrap(), "allowed: connects unencrypted");

        // Once it encrypted, it must, and the permission is gone.
        d.mark_encrypting("r");
        let r = d.relay("r").unwrap();
        assert!(r.encrypts && !r.unencrypted && d.encryption(&r).unwrap());
        assert!(d.handle(Request::Unencrypted { relay: "rig".into(), on: true }).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_device_that_comes_back_with_another_id_is_followed() {
        let dev = |id: &str| device("1-2", id);
        let mut t = Track { id: "046d:c294".into(), serial: None, dropped: Some(Instant::now()), always: false, history: api::History::default() };
        assert!(t.follows(&dev("046d:c24f")), "same maker, new mode");
        assert!(!t.follows(&dev("045e:028e")), "another maker is another device");
        assert!(!t.follows(&dev("046d:c294")), "the same id is plugged in by its rule, if any");

        // With serial numbers on both sides, they decide.
        let with = |id: &str, serial: &str| proto::DeviceInfo { serial: Some(serial.into()), ..dev(id) };
        t.serial = Some("A00SA5292S0K9Z".into());
        assert!(t.follows(&with("046d:c24f", "A00SA5292S0K9Z")));
        assert!(!t.follows(&with("046d:c24f", "B11")), "another wheel of the same maker, plugged in there meanwhile");
        assert!(t.follows(&with("0eb7:0e04", "A00SA5292S0K9Z")), "the same device under another maker's id");
        assert!(t.follows(&dev("046d:c24f")), "the relay sent no serial: as before");
        t.dropped = Some(Instant::now() - FOLLOW);
        assert!(!t.follows(&dev("046d:c24f")), "too long ago");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn devices_that_will_not_unplug_stay_tracked_and_forget_unplugs() {
        let root = std::env::temp_dir().join(format!("plugover-hostd-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let sys = root.join("sys");
        fs::create_dir_all(&sys).unwrap();
        let d = Daemon::open(sys.clone(), root.join("host.toml")).unwrap();
        let relay = PairedRelay { id: "r".into(), name: "rig".into(), address: "127.0.0.1:1".into(), host_id: "00".repeat(8), key: "00".repeat(32), encrypts: true, unencrypted: false };
        d.cfg.lock().unwrap().relay.push(relay);
        let slot = ("r".to_string(), "1-2".to_string());
        d.attached.lock().unwrap().insert(slot.clone(), vhci::Attachment { port: 3, hid: None });

        // Plugged in again before the relay's list knows its id: the device keeps the old one.
        d.plugged(&slot, "054c:09cc", None, false);
        d.tracks.lock().unwrap().get_mut(&slot).unwrap().dropped = Some(Instant::now());
        d.plugged(&slot, "", None, false);
        assert_eq!(d.tracks.lock().unwrap()[&slot].id, "054c:09cc");

        // No vhci_hcd in this fake sysfs, so every detach fails.
        d.set_session("punktfunk", true, None);
        d.set_session("punktfunk", false, None);
        assert!(d.attached.lock().unwrap().contains_key(&slot), "a failed unplug at the end of a stream keeps the device");
        let req = || Request::Detach { relay: None, busid: "1-2".into() };
        assert!(d.handle(req()).is_err());
        assert!(d.attached.lock().unwrap().contains_key(&slot), "so does a failed detach by hand");

        let vhci = sys.join("devices/platform/vhci_hcd.0");
        fs::create_dir_all(&vhci).unwrap();
        fs::write(vhci.join("detach"), "").unwrap();
        d.handle(Request::Forget { relay: "rig".into() }).unwrap();
        assert!(d.attached.lock().unwrap().is_empty(), "forgetting a relay unplugs its devices");
        assert_eq!(fs::read_to_string(vhci.join("detach")).unwrap(), "3");
        fs::remove_dir_all(root).unwrap();
    }
}
