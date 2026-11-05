//! The host daemon: keeps a live view of every paired relay, plugs the devices you chose in
//! while a stream runs (and again after a dropout), and answers the punktfunk plugin and the
//! CLI with one JSON object per line: over a socket file on Linux, over loopback TCP with a token
//! on Windows.

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
    },
    /// Names a model, or corrects its kind; neither removes the label.
    Label {
        device: String,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        kind: Option<catalog::Kind>,
    },
    Attach { relay: Option<String>, busid: String },
    Detach { relay: Option<String>, busid: String },
    /// Allows connecting a relay's devices without encryption, for a relay too old to encrypt.
    Unencrypted { relay: String, on: bool },
    /// The plugin sends this when a stream starts and stops.
    Session { active: bool },
}

type Slot = (String, String); // (relay id, busid)

/// What happened to one device while the daemon runs.
#[derive(Default)]
struct Track {
    /// vendor:product it had when it was last plugged in here.
    id: String,
    /// When it last vanished while plugged in.
    dropped: Option<Instant>,
    history: api::History,
}

impl Track {
    /// Wheels such as the G29 come back with another product id when their driver switches
    /// them to another mode; a device that vanished and is back on its port is followed.
    // ponytail: same maker on the same port within FOLLOW; a USB serial number would be surer.
    fn follows(&self, d: &proto::DeviceInfo) -> bool {
        self.dropped.is_some_and(|at| at.elapsed() < FOLLOW) && self.id != d.id && self.id.get(..4) == d.id.get(..4)
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
    /// Detached by hand during this stream; not plugged back in until the next one.
    held: Mutex<HashSet<Slot>>,
    /// Why plugging a chosen device in failed, and when, while it keeps failing.
    failed: Mutex<HashMap<Slot, (String, Instant)>>,
    /// Latest round trips to each relay.
    rtt: Mutex<HashMap<String, VecDeque<Duration>>>,
    tracks: Mutex<HashMap<Slot, Track>>,
    watched: Mutex<HashSet<String>>,
    session: AtomicBool,
    reconciling: Mutex<()>,
}

impl Daemon {
    pub fn open(sys: PathBuf, config_path: PathBuf) -> Result<Arc<Self>> {
        Ok(Arc::new(Self {
            sys,
            cfg: Mutex::new(state::load(&config_path)?),
            config_path,
            name: state::hostname(),
            live: Mutex::default(),
            errors: Mutex::default(),
            attached: Mutex::default(),
            held: Mutex::default(),
            failed: Mutex::default(),
            rtt: Mutex::default(),
            tracks: Mutex::default(),
            watched: Mutex::default(),
            session: AtomicBool::new(false),
            reconciling: Mutex::default(),
        }))
    }

    pub fn run(self: Arc<Self>, socket: &Path) -> Result<()> {
        let ids: Vec<String> = self.cfg.lock().unwrap().relay.iter().map(|r| r.id.clone()).collect();
        for id in ids {
            self.watch(id);
        }
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

    /// Notes that the device on `slot` is plugged in here as `id`.
    fn plugged(&self, slot: &Slot, id: &str) {
        let mut tracks = self.tracks.lock().unwrap();
        let t = tracks.entry(slot.clone()).or_default();
        if t.dropped.is_some() && t.id != id && !t.id.is_empty() {
            eprintln!("{} came back as {id}; following it", slot.1);
        }
        (t.id, t.dropped) = (id.to_string(), None);
    }

    /// Makes reality match the plan: forgets devices the kernel dropped and, while a stream
    /// runs, plugs in every chosen device that is available.
    fn reconcile(&self) {
        let Ok(_pass) = self.reconciling.try_lock() else { return };
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
            let mut tracks = self.tracks.lock().unwrap();
            let t = tracks.entry(slot).or_default();
            t.dropped = Some(Instant::now());
            t.history.drops += 1;
            t.history.last_drop = Some(state::now());
        }
        if !self.session.load(Ordering::SeqCst) {
            return;
        }
        let todo: Vec<(PairedRelay, String, String)> = {
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
                            let wanted = cfg.auto.iter().any(|rule| rule.matches(&relay.id, d)) || tracks.get(&slot).is_some_and(|t| t.follows(d));
                            wanted
                                && d.identity.blocked.is_none()
                                && d.lent_to.is_none()
                                && !attached.contains_key(&slot)
                                && !held.contains(&slot)
                                && failed.get(&slot).is_none_or(|(_, at)| at.elapsed() >= RETRY)
                        })
                        .map(|d| (relay.clone(), d.busid.clone(), d.id.clone()))
                        .collect::<Vec<_>>()
                })
                .collect()
        };
        for (relay, busid, id) in todo {
            let slot = (relay.id.clone(), busid);
            match self.encryption(&relay).and_then(|encrypt| vhci::attach(&self.sys, &relay, &slot.1, encrypt)) {
                Ok(a) => {
                    eprintln!("plugged in {} from {} (port {})", slot.1, relay.name, a.port);
                    self.plugged(&slot, &id);
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
    }

    fn set_session(&self, active: bool) {
        let _pass = self.reconciling.lock().unwrap();
        // The plugin repeats the current state now and then; only a change does anything.
        if self.session.swap(active, Ordering::SeqCst) == active {
            return;
        }
        self.failed.lock().unwrap().clear();
        if active {
            self.held.lock().unwrap().clear();
            return;
        }
        self.unplug(|_| true);
    }

    /// Unplugs the attached devices `which` picks. One that will not unplug stays tracked, so it
    /// still shows and can be unplugged again.
    fn unplug(&self, which: impl Fn(&Slot) -> bool) {
        self.attached.lock().unwrap().retain(|slot, a| {
            if !which(slot) {
                return true;
            }
            match vhci::detach(&self.sys, a) {
                Ok(()) => false,
                Err(e) => {
                    eprintln!("could not unplug {}: {e:#}", slot.1);
                    true
                }
            }
        });
    }

    /// Turning auto-connect off gives the device back now, not only when the stream ends.
    fn release(&self, relay: &str, device: &str) {
        let _pass = self.reconciling.lock().unwrap();
        let gone: Vec<Slot> = {
            let (cfg, live, attached) = (self.cfg.lock().unwrap(), self.live.lock().unwrap(), self.attached.lock().unwrap());
            let devices = live.get(relay).map_or(&[][..], |s| &s.devices[..]);
            attached
                .keys()
                .filter(|(r, busid)| {
                    r == relay
                        && devices.iter().any(|d| d.busid == *busid && d.id == device && !cfg.auto.iter().any(|a| a.matches(relay, d)))
                })
                .cloned()
                .collect()
        };
        self.unplug(|slot| gone.contains(slot));
    }

    fn handle(self: &Arc<Self>, req: Request) -> Result<Value> {
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
            Request::Auto { relay, device, on, port } => {
                let (relay, device) = (self.pick(relay.as_deref())?.id, state::device_id(&device)?);
                let title = self.live.lock().unwrap().get(&relay).and_then(|snap| {
                    snap.devices.iter().find(|d| d.id == device).map(|d| d.identity.title.clone())
                });
                let mut cfg = self.cfg.lock().unwrap();
                state::set_auto(&mut cfg.auto, AutoRule { relay: relay.clone(), device: device.clone(), title, port }, on);
                self.save(&cfg)?;
                drop(cfg);
                if !on {
                    self.release(&relay, &device);
                }
                self.reconcile();
                json!({ "ok": true })
            }
            Request::Label { device, name, kind } => {
                let mut cfg = self.cfg.lock().unwrap();
                state::set_label(&mut cfg.label, state::device_id(&device)?, name, kind)?;
                self.save(&cfg)?;
                json!({ "ok": true })
            }
            Request::Attach { relay, busid } => {
                let relay = self.pick(relay.as_deref())?;
                let _pass = self.reconciling.lock().unwrap();
                let a = vhci::attach(&self.sys, &relay, &busid, self.encryption(&relay)?)?;
                let port = a.port;
                let slot = (relay.id.clone(), busid);
                let id = self.live.lock().unwrap().get(&relay.id).and_then(|s| s.devices.iter().find(|d| d.busid == slot.1).map(|d| d.id.clone()));
                self.plugged(&slot, &id.unwrap_or_default());
                self.attached.lock().unwrap().insert(slot, a);
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
            Request::Session { active } => {
                self.set_session(active);
                self.reconcile();
                json!({ "ok": true })
            }
        })
    }

    fn status(&self) -> api::Status {
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
                    let here = attached.contains_key(&slot) || d.lent_to.as_deref() == Some(self.name.as_str());
                    let matched: Vec<&&AutoRule> = rules.iter().filter(|a| a.matches(&r.id, d)).collect();
                    let mut info = d.clone();
                    state::Label::apply(&cfg.label, &d.id, &mut info.identity);
                    api::DeviceStatus {
                        info,
                        state: if here { api::DeviceState::Here } else if d.lent_to.is_some() { api::DeviceState::Elsewhere } else { api::DeviceState::Free },
                        auto: !matched.is_empty(),
                        auto_port: matched.iter().any(|a| a.port.is_some()),
                        error: (!here && d.lent_to.is_none() && !matched.is_empty()).then(|| failed.get(&slot).map(|(why, _)| why.clone())).flatten(),
                        history: tracks.get(&slot).map(|t| t.history.clone()).unwrap_or_default(),
                    }
                };
                // Chosen devices that are not plugged into the relay (or it is offline).
                let missing = rules
                    .iter()
                    .filter(|a| !devices.iter().any(|d| a.matches(&r.id, d)))
                    .map(|a| api::Missing {
                        id: a.device.clone(),
                        // Rules from before 0.5 have no title; the catalog may still know the device.
                        title: cfg
                            .label
                            .iter()
                            .find(|l| l.device == a.device)
                            .and_then(|l| l.name.clone())
                            .or_else(|| a.title.clone())
                            .or_else(|| catalog::catalog_title(&a.device))
                            .unwrap_or_else(|| a.device.clone()),
                        port: a.port.clone(),
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
        api::Status { host: self.name.clone(), version: proto::VERSION.into(), session: self.session.load(Ordering::SeqCst), relays }
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
        let token = fs::read_to_string(endpoint).context("the host daemon is not running (no token file)")?;
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
        let dev = |id: &str| proto::DeviceInfo {
            busid: "1-2".into(),
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
        };
        let mut t = Track { id: "046d:c294".into(), dropped: Some(Instant::now()), history: api::History::default() };
        assert!(t.follows(&dev("046d:c24f")), "same maker, new mode");
        assert!(!t.follows(&dev("045e:028e")), "another maker is another device");
        assert!(!t.follows(&dev("046d:c294")), "the same id is plugged in by its rule, if any");
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
        d.attached.lock().unwrap().insert(slot.clone(), vhci::Attachment { port: 3 });

        // No vhci_hcd in this fake sysfs, so every detach fails.
        d.set_session(true);
        d.set_session(false);
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
