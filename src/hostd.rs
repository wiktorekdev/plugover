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
    collections::{HashMap, HashSet},
    fs,
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

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
    /// The plugin sends this when a stream starts and stops.
    Session { active: bool },
}

type Slot = (String, String); // (relay id, busid)

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
        thread::spawn(move || loop {
            thread::sleep(Duration::from_secs(2));
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
            while let Some(relay) = me.relay(&id) {
                let result = host::watch(&relay, |snap| {
                    if !was_online {
                        eprintln!("{} is online at {}", relay.name, relay.address);
                        was_online = true;
                    }
                    me.errors.lock().unwrap().remove(&id);
                    me.live.lock().unwrap().insert(id.clone(), snap);
                    me.reconcile();
                    me.relay(&id).is_some()
                });
                me.live.lock().unwrap().remove(&id);
                let why = result.err().map_or("relay forgotten".to_string(), |e| format!("{e:#}"));
                if was_online {
                    eprintln!("{} went offline: {why}", relay.name);
                    was_online = false;
                }
                me.errors.lock().unwrap().insert(id.clone(), why);
                me.rediscover(&id);
                thread::sleep(Duration::from_secs(2));
            }
            me.watched.lock().unwrap().remove(&id);
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

    /// Makes reality match the plan: forgets devices the kernel dropped and, while a stream
    /// runs, plugs in every chosen device that is available.
    fn reconcile(&self) {
        let Ok(_pass) = self.reconciling.try_lock() else { return };
        self.attached.lock().unwrap().retain(|_, a| vhci::alive(&self.sys, a));
        if !self.session.load(Ordering::SeqCst) {
            return;
        }
        let todo: Vec<(PairedRelay, String)> = {
            let (cfg, live) = (self.cfg.lock().unwrap(), self.live.lock().unwrap());
            let (attached, held) = (self.attached.lock().unwrap(), self.held.lock().unwrap());
            cfg.auto
                .iter()
                .filter_map(|rule| Some((cfg.relay.iter().find(|r| r.id == rule.relay)?, live.get(&rule.relay)?, rule)))
                .flat_map(|(relay, snap, rule)| {
                    snap.devices
                        .iter()
                        .filter(|d| rule.matches(&rule.relay, d) && d.identity.blocked.is_none() && d.lent_to.is_none())
                        .map(|d| (relay.id.clone(), d.busid.clone()))
                        .filter(|slot| !attached.contains_key(slot) && !held.contains(slot))
                        .map(|(_, busid)| (relay.clone(), busid))
                        .collect::<Vec<_>>()
                })
                .collect()
        };
        for (relay, busid) in todo {
            match vhci::attach(&self.sys, &relay, &busid) {
                Ok(a) => {
                    eprintln!("plugged in {busid} from {} (port {})", relay.name, a.port);
                    self.attached.lock().unwrap().insert((relay.id.clone(), busid), a);
                }
                Err(e) => eprintln!("could not plug in {busid} from {}: {e:#}", relay.name),
            }
        }
    }

    fn set_session(&self, active: bool) {
        let _pass = self.reconciling.lock().unwrap();
        // The plugin repeats the current state now and then; only a change does anything.
        if self.session.swap(active, Ordering::SeqCst) == active {
            return;
        }
        if active {
            self.held.lock().unwrap().clear();
            return;
        }
        for ((_, busid), a) in self.attached.lock().unwrap().drain() {
            if let Err(e) = vhci::detach(&self.sys, &a) {
                eprintln!("could not unplug {busid}: {e}");
            }
        }
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
        for slot in gone {
            let Some(a) = self.attached.lock().unwrap().remove(&slot) else { continue };
            if let Err(e) = vhci::detach(&self.sys, &a) {
                eprintln!("could not unplug {}: {e}", slot.1);
            }
        }
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
                self.errors.lock().unwrap().remove(&id);
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
                let a = vhci::attach(&self.sys, &relay, &busid)?;
                let port = a.port;
                self.attached.lock().unwrap().insert((relay.id.clone(), busid), a);
                json!({ "ok": true, "port": port })
            }
            Request::Detach { relay, busid } => {
                let slot = (self.pick(relay.as_deref())?.id, busid);
                let _pass = self.reconciling.lock().unwrap();
                let a = self.attached.lock().unwrap().remove(&slot).context("that device is not plugged in here")?;
                vhci::detach(&self.sys, &a)?;
                self.held.lock().unwrap().insert(slot);
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
        let relays = cfg
            .relay
            .iter()
            .map(|r| {
                let snap = live.get(&r.id);
                let devices = snap.map_or(&[][..], |s| &s.devices[..]);
                let rules: Vec<&AutoRule> = cfg.auto.iter().filter(|a| a.relay == r.id).collect();
                let status = |d: &proto::DeviceInfo| {
                    let here = attached.contains_key(&(r.id.clone(), d.busid.clone())) || d.lent_to.as_deref() == Some(self.name.as_str());
                    let matched: Vec<&&AutoRule> = rules.iter().filter(|a| a.matches(&r.id, d)).collect();
                    let mut info = d.clone();
                    state::Label::apply(&cfg.label, &d.id, &mut info.identity);
                    api::DeviceStatus {
                        info,
                        state: if here { api::DeviceState::Here } else if d.lent_to.is_some() { api::DeviceState::Elsewhere } else { api::DeviceState::Free },
                        auto: !matched.is_empty(),
                        auto_port: matched.iter().any(|a| a.port.is_some()),
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
                    version: snap.map(|s| s.version.clone()).filter(|v| !v.is_empty()),
                    error: if snap.is_some() { None } else { errors.get(&r.id).cloned() },
                    devices: devices.iter().map(status).collect(),
                    missing,
                }
            })
            .collect();
        api::Status { host: self.name.clone(), version: proto::VERSION.into(), session: self.session.load(Ordering::SeqCst), relays }
    }

    fn answer<S: Read + Write>(me: &Arc<Self>, read: S, mut write: S, token: Option<&str>) {
        for line in BufReader::new(read).lines().map_while(Result::ok) {
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
}
