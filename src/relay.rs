//! The relay: the machine the wheel is plugged into. It lends devices to paired hosts, tells
//! them what is plugged in, and takes devices back when a host lets go or disappears. On Linux
//! the kernel's usbip-host serves the devices, on Windows usbipd-win.

use crate::{
    auth,
    catalog::{self, Identity, UsbIds},
    discovery, net,
    tls,
    proto::{self, DeviceInfo, PairConfirm, PairHello, PairReply, Ping, Pong, Snapshot},
    state::{self, PairedHost, PairingWindow, RelayState},
    usbip,
    usbipd::Device,
};
#[cfg(not(windows))]
use crate::sysfs;
use anyhow::{Context, Result, anyhow, bail, ensure};
use std::{
    collections::HashMap,
    io::{BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex},
    thread,
    time::{Duration, Instant},
};

pub const PAIR_MINUTES: u64 = 15;
const PAIR_ATTEMPTS: u8 = 5;
#[cfg(windows)]
const CODE_HINT: &str = "run `plugover code` as administrator";
#[cfg(not(windows))]
const CODE_HINT: &str = "run `sudo plugover code`";
const HEARTBEAT: Duration = Duration::from_secs(2);
/// How often hosts hear about plugs without uevents (Windows); also how long one device scan is
/// reused between them.
const POLL: Duration = Duration::from_millis(500);
/// How often hosts hear about changes with uevents, for what they do not cover (share lists).
const SLOW_POLL: Duration = Duration::from_secs(2);
/// Plug events come in bursts (device, interfaces, drivers); let one settle before scanning.
const SETTLE: Duration = Duration::from_millis(150);

/// A device a host has, or is about to get.
struct Lend {
    host_id: String,
    host: String,
    /// The kernel has the connection. Until then `tend` leaves the device alone (Linux).
    live: bool,
    /// Our end of the host's connection, closed to take the device back (Windows).
    #[cfg_attr(target_os = "linux", allow(dead_code))]
    conn: Option<TcpStream>,
}

pub struct Relay {
    sys: PathBuf,
    name: String,
    id: String,
    /// Read again on every use, so `plugover unpair` and `plugover share` apply right away.
    state_path: PathBuf,
    ids: Option<UsbIds>,
    /// HID descriptors vanish while a device is lent out, so remember what each one was.
    known: Mutex<HashMap<(String, u16, u16), Identity>>,
    /// busid -> who has it.
    lent: Mutex<HashMap<String, Lend>>,
    /// When, and at which `changes` count, the last scan ran.
    scanned: Mutex<Option<(Instant, u64, Vec<Device>)>>,
    /// Bumped on every USB uevent; `None` while nothing watches them, so everything polls.
    changes: Mutex<Option<u64>>,
    changed: Condvar,
    pairing: Mutex<()>,
}

impl Relay {
    pub fn open(sys: PathBuf, state_path: PathBuf) -> Result<Self> {
        let mut st: RelayState = state::load(&state_path)?;
        if st.id.is_empty() {
            st.id = hex::encode(auth::random::<8>());
            state::save(&state_path, &st)?;
        }
        Ok(Self {
            sys,
            name: state::hostname(),
            id: st.id,
            state_path,
            ids: UsbIds::load(),
            known: Mutex::default(),
            lent: Mutex::default(),
            scanned: Mutex::default(),
            changes: Mutex::default(),
            changed: Condvar::new(),
            pairing: Mutex::default(),
        })
    }

    fn state(&self) -> Result<RelayState> {
        state::load(&self.state_path)
    }

    pub fn serve(self: Arc<Self>, port: u16) -> Result<()> {
        let listener = TcpListener::bind(("0.0.0.0", port)).with_context(|| format!("listening on port {port}"))?;
        let _mdns = discovery::advertise(&self.name, &self.id, port)
            .map_err(|e| eprintln!("not announcing over mDNS: {e:#}"))
            .ok();
        eprintln!("relay {} ({}) {} listening on port {port}", self.name, self.id, proto::VERSION);
        self.accept(listener)
    }

    fn accept(self: Arc<Self>, listener: TcpListener) -> Result<()> {
        // Nothing is lent yet, so anything usbip-host still claims is left over from a crash.
        #[cfg(target_os = "linux")]
        for busid in sysfs::export_claimed(&self.sys) {
            match sysfs::export_release(&self.sys, &busid) {
                Ok(()) => eprintln!("gave {busid} back to its local driver (left over from an earlier run)"),
                Err(e) => eprintln!("could not give {busid} back: {e}"),
            }
        }
        #[cfg(target_os = "linux")]
        {
            *self.changes.lock().unwrap() = Some(0);
            let me = self.clone();
            if let Err(e) = sysfs::watch(move || me.bump()) {
                *self.changes.lock().unwrap() = None;
                eprintln!("not watching for USB plugs, polling instead: {e}");
            }
        }
        let me = self.clone();
        thread::spawn(move || loop {
            thread::sleep(Duration::from_secs(1));
            me.tend();
        });
        for conn in listener.incoming() {
            let Ok(stream) = conn else { continue };
            let me = self.clone();
            thread::spawn(move || {
                let peer = stream.peer_addr().map(|a| a.to_string()).unwrap_or_default();
                if let Err(e) = me.handle(stream) {
                    eprintln!("{peer}: {e:#}");
                }
            });
        }
        Ok(())
    }

    fn handle(&self, mut s: TcpStream) -> Result<()> {
        s.set_read_timeout(Some(net::HANDSHAKE_TIMEOUT))?;
        let mut magic = [0u8; 4];
        match s.read_exact(&mut magic) {
            // Hosts check that the port answers by connecting and hanging up.
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
            r => r?,
        }
        match &magic {
            proto::DATA => self.lend(s, false),
            proto::DATA_ENCRYPTED => self.lend(s, true),
            proto::CONTROL => self.control(s, false),
            proto::CONTROL_ENCRYPTED => self.control(s, true),
            proto::PAIR => self.pair(s),
            _ => bail!("not a Plugover host"),
        }
    }

    /// Reads the host id and sends the nonce. Returns the host and the nonce to verify against.
    fn challenge(&self, s: &mut TcpStream) -> Result<(PairedHost, [u8; 32])> {
        let mut id = [0u8; 8];
        s.read_exact(&mut id)?;
        let id = hex::encode(id);
        let Some(host) = self.state()?.hosts.into_iter().find(|h| h.id == id) else {
            let _ = s.write_all(proto::UNPAIRED);
            bail!("unknown host {id}; pair it first");
        };
        let nonce: [u8; 32] = auth::random();
        s.write_all(&nonce)?;
        Ok((host, nonce))
    }

    fn lend(&self, mut s: TcpStream, encrypted: bool) -> Result<()> {
        let (host, nonce) = self.challenge(&mut s)?;
        let mut tag = [0u8; auth::TAG_LEN];
        s.read_exact(&mut tag)?;
        let busid = usbip::read_import_request(&mut s)?;
        let dev = if auth::verify(&state::decode_key(&host.key)?, &nonce, &busid, &tag) {
            self.reserve(&busid, &host, &s)
        } else {
            Err(anyhow!("{} failed the challenge", host.name))
        };
        let key = state::decode_key(&host.key)?;
        let crypt = encrypted.then_some((&key, &nonce));
        let dev = match dev {
            Ok(d) => d,
            Err(e) => {
                refuse(&s, crypt)?;
                return Err(e);
            }
        };
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd;
            // The kernel takes the host's connection, or, where it cannot encrypt it, one end of a
            // local socket whose other end Plugover encrypts towards the host.
            let (mut relayed, mut kernel) = (None, false);
            let handed = sysfs::export_bind(&self.sys, &busid).and_then(|()| {
                net::tune(&s)?;
                kernel = encrypted && net::encrypt(&s, &key, &nonce, true)?;
                if encrypted && !kernel {
                    relayed = Some(net::hand_over(|fd| sysfs::export_socket(&self.sys, &busid, fd))?);
                    return Ok(());
                }
                // Same order as usbipd: the kernel takes the socket, then the reply goes out.
                sysfs::export_socket(&self.sys, &busid, s.as_raw_fd())
            });
            if let Err(e) = handed {
                self.lent.lock().unwrap().remove(&busid);
                // Half a hand-over would leave the device with no driver on the relay.
                if sysfs::export_claimed(&self.sys).contains(&busid)
                    && let Err(e) = sysfs::export_release(&self.sys, &busid)
                {
                    eprintln!("could not give {busid} back: {e}");
                }
                let _ = refuse(&s, crypt.filter(|_| !kernel));
                return Err(anyhow!(e).context(format!("lending {busid} to {}", host.name)));
            }
            if let Some(l) = self.lent.lock().unwrap().get_mut(&busid) {
                l.live = true;
            }
            let reply = Some(net::exported(&dev.dev));
            match relayed {
                None => usbip::write_import_reply(&mut s, reply.as_ref())?,
                Some(ours) => {
                    let (r, mut w) = tls::wrap(s.try_clone()?, s.try_clone()?, &key, &nonce, true);
                    usbip::write_import_reply(&mut w, reply.as_ref())?;
                    // tend() takes the device back once either side hangs up.
                    thread::spawn(move || ours.pump(s, r, w));
                }
            }
            eprintln!("lent {busid} to {}", host.name);
            // tend() takes it back.
        }
        #[cfg(windows)]
        {
            eprintln!("lent {busid} to {}", host.name);
            let crypt = encrypted.then(|| -> Result<_> { Ok(tls::wrap(s.try_clone()?, s.try_clone()?, &key, &nonce, true)) }).transpose()?;
            let result = crate::usbipd::lend(s, &dev, crypt);
            self.lent.lock().unwrap().remove(&busid);
            eprintln!("{busid} is back from {}", host.name);
            result?;
        }
        Ok(())
    }

    /// Devices plugged in here right now.
    fn scan(&self) -> Result<Vec<Device>> {
        #[cfg(windows)]
        return crate::usbipd::list();
        #[cfg(not(windows))]
        Ok(sysfs::list(&self.sys)?.into_iter().map(|dev| Device { dev, attached: false, bound: false }).collect())
    }

    /// `scan`, shared by every watching host for one poll (on Windows a scan runs `usbipd state`).
    fn devices(&self) -> Result<Vec<Device>> {
        let now = *self.changes.lock().unwrap();
        let mut scanned = self.scanned.lock().unwrap();
        match &*scanned {
            Some((at, seen, devs)) if at.elapsed() < POLL && *seen == now.unwrap_or(0) => Ok(devs.clone()),
            _ => {
                let devs = self.scan()?;
                *scanned = Some((Instant::now(), now.unwrap_or(0), devs.clone()));
                Ok(devs)
            }
        }
    }

    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    fn bump(&self) {
        if let Some(n) = self.changes.lock().unwrap().as_mut() {
            *n += 1;
        }
        self.changed.notify_all();
    }

    /// Sleeps until a USB uevent or a poll interval passes, whichever comes first.
    fn wait(&self, seen: &mut Option<u64>) {
        let changes = self.changes.lock().unwrap();
        let timeout = if changes.is_some() { SLOW_POLL } else { POLL };
        let (changes, _) = self.changed.wait_timeout_while(changes, timeout, |n| *n == *seen).unwrap();
        let woken = *changes != *seen;
        *seen = *changes;
        drop(changes);
        if woken {
            thread::sleep(SETTLE);
        }
    }

    /// Checks that `host` may have the device and claims it in the same step, so two hosts
    /// asking at once cannot both get it.
    fn reserve(&self, busid: &str, host: &PairedHost, s: &TcpStream) -> Result<Device> {
        let st = self.state()?;
        let dev = self.scan()?.into_iter().find(|d| d.dev.busid == busid).with_context(|| format!("no device at {busid}"))?;
        if let Some(why) = catalog::blocked_reason(&dev.dev) {
            bail!("{busid} is a {why}, which is never shared");
        }
        let id = format!("{:04x}:{:04x}", dev.dev.vid, dev.dev.pid);
        if st.withheld(&id).is_some() {
            bail!("{busid} ({id}) is not on the share list of {}", self.name);
        }
        let mut lent = self.lent.lock().unwrap();
        ensure!(!dev.attached && !lent.contains_key(busid), "{busid} is already in use");
        let windows = cfg!(windows);
        let conn = if windows { Some(s.try_clone()?) } else { None };
        lent.insert(busid.into(), Lend { host_id: host.id.clone(), host: host.name.clone(), live: windows, conn });
        Ok(dev)
    }

    fn control(&self, mut s: TcpStream, encrypted: bool) -> Result<()> {
        let (host, nonce) = self.challenge(&mut s)?;
        let mut tag = [0u8; auth::TAG_LEN];
        s.read_exact(&mut tag)?;
        let key = state::decode_key(&host.key)?;
        ensure!(auth::verify(&key, &nonce, proto::CONTROL_CONTEXT, &tag), "{} failed the challenge", host.name);
        if !encrypted {
            eprintln!("{} connects without encryption; update Plugover on it", host.name);
        }
        net::tune(&s)?;
        s.set_write_timeout(Some(HEARTBEAT))?;
        let (r, w): (Box<dyn Read + Send>, Box<dyn Write + Send>) = if encrypted {
            let (r, w) = tls::wrap(s.try_clone()?, s.try_clone()?, &key, &nonce, true);
            (Box::new(r), Box::new(w))
        } else {
            (Box::new(s.try_clone()?), Box::new(s.try_clone()?))
        };
        // Pings come in on the same connection; answering them shares the writer.
        let w = Arc::new(Mutex::new(w));
        let (mut r, pong) = (BufReader::new(r), w.clone());
        thread::spawn(move || {
            while let Ok(Ping { ping }) = proto::recv(&mut r) {
                if proto::send(&mut *pong.lock().unwrap(), &Pong { pong: ping }).is_err() {
                    break;
                }
            }
        });
        let result = (|| {
            let (mut last, mut sent_at) = (None, Instant::now());
            let mut seen = *self.changes.lock().unwrap();
            loop {
                let st = self.state()?;
                ensure!(st.hosts.iter().any(|h| h.id == host.id), "{} was unpaired", host.name);
                let snap = self.snapshot(&st)?;
                if last.as_ref() != Some(&snap) || sent_at.elapsed() >= HEARTBEAT {
                    proto::send(&mut *w.lock().unwrap(), &snap)?;
                    (last, sent_at) = (Some(snap), Instant::now());
                }
                // ponytail: Windows still polls `usbipd state`; device notifications would make it instant.
                self.wait(&mut seen);
            }
        })();
        // Ends the ping reader too.
        let _ = s.shutdown(std::net::Shutdown::Both);
        result
    }

    fn snapshot(&self, st: &RelayState) -> Result<Snapshot> {
        let lent: HashMap<String, String> = self.lent.lock().unwrap().iter().map(|(b, l)| (b.clone(), l.host.clone())).collect();
        let mut known = self.known.lock().unwrap();
        let devices = self
            .devices()?
            .iter()
            .map(|Device { dev: d, attached, .. }| {
                let key = (d.busid.clone(), d.vid, d.pid);
                let fresh = catalog::identify(d, self.ids.as_ref());
                let mut identity = if d.driver.as_deref() == Some("usbip-host") {
                    known.get(&key).cloned().unwrap_or(fresh)
                } else {
                    known.insert(key, fresh.clone());
                    fresh
                };
                let id = format!("{:04x}:{:04x}", d.vid, d.pid);
                if identity.blocked.is_none() {
                    identity.blocked = st.withheld(&id).map(str::to_string);
                }
                DeviceInfo {
                    busid: d.busid.clone(),
                    id,
                    identity,
                    // Attached but not by us: WSL or another USB/IP client on Windows.
                    lent_to: lent.get(&d.busid).cloned().or_else(|| attached.then(|| "another USB/IP client".into())),
                }
            })
            .collect();
        Ok(Snapshot { relay_id: self.id.clone(), relay: self.name.clone(), devices, version: proto::VERSION.into(), encrypted: false })
    }

    fn pair(&self, s: TcpStream) -> Result<()> {
        let mut w = s.try_clone()?;
        let refuse = |w: &mut TcpStream, reason: String| {
            let _ = proto::send(w, &PairReply::Refused { reason: reason.clone() });
            anyhow!(reason)
        };
        // One at a time, so wrong guesses are counted in order.
        let Ok(_guard) = self.pairing.try_lock() else {
            return Err(refuse(&mut w, "another pairing is in progress".into()));
        };
        let mut r = BufReader::new(s);
        let hello: PairHello = proto::recv(&mut r)?;
        let window_path = PairingWindow::path(&self.state_path);
        let mut window: PairingWindow = state::load(&window_path)?;
        if !window.is_open() {
            let _ = fs::remove_file(&window_path);
            return Err(refuse(&mut w, format!("pairing is closed; {CODE_HINT} on {} for a code", self.name)));
        }
        let host_id: auth::HostId =
            hex::decode(&hello.host_id).ok().and_then(|b| b.try_into().ok()).context("pairing request has a malformed id")?;
        let host_msg = hex::decode(&hello.pake).context("pairing request has a malformed key")?;
        let (pake, msg) = auth::pake_start(&window.code, false);
        proto::send(&mut w, &PairReply::Pake { relay_id: self.id.clone(), relay: self.name.clone(), pake: hex::encode(msg), version: proto::VERSION.into() })?;
        let confirm: PairConfirm = proto::recv(&mut r)?;
        let key = auth::pake_finish(pake, &host_msg).context("pairing request has a malformed key")?;
        // The host proves the code first; a wrong guess learns nothing and costs an attempt.
        if !auth::verify(&key, &host_id, auth::CONFIRM_HOST, &hex::decode(confirm.tag).unwrap_or_default()) {
            // `plugover code` may have opened a new window meanwhile; that one keeps its tries.
            let current: PairingWindow = state::load(&window_path)?;
            if current.code != window.code {
                return Err(refuse(&mut w, format!("wrong code from {}; a new code was printed since", hello.name)));
            }
            window.attempts -= 1;
            if window.attempts == 0 {
                let _ = fs::remove_file(&window_path);
                return Err(refuse(&mut w, format!("wrong code; too many tries, so pairing is closed until you {CODE_HINT} again")));
            }
            state::save(&window_path, &window)?;
            return Err(refuse(&mut w, format!("wrong code from {} ({} tries left)", hello.name, window.attempts)));
        }

        let host = PairedHost { id: hex::encode(host_id), name: hello.name.clone(), key: hex::encode(key) };
        let mut st = self.state()?;
        st.hosts.retain(|h| h.id != host.id);
        st.hosts.push(host);
        state::save(&self.state_path, &st)?;
        // A code pairs one host.
        let _ = fs::remove_file(&window_path);
        proto::send(&mut w, &PairReply::Paired { tag: hex::encode(auth::sign(&key, &host_id, auth::CONFIRM_RELAY)) })?;
        eprintln!("paired with {}", hello.name);
        Ok(())
    }

    /// Takes devices back from hosts that let go, died or were unpaired.
    // ponytail: runs every second; a uevent watch would react to a dropped link instantly.
    fn tend(&self) {
        if self.lent.lock().unwrap().is_empty() {
            return;
        }
        // Unreadable state revokes nothing: a broken file should not unplug anyone's wheel.
        let paired: Option<Vec<String>> = self.state().ok().map(|st| st.hosts.into_iter().map(|h| h.id).collect());
        self.lent.lock().unwrap().retain(|busid, l| {
            let revoked = l.live && paired.as_ref().is_some_and(|p| !p.contains(&l.host_id));
            if revoked {
                eprintln!("{} was unpaired; taking {busid} back", l.host);
            }
            self.keep(busid, l, revoked)
        });
    }

    /// Whether `tend` keeps tracking the device.
    #[cfg(target_os = "linux")]
    fn keep(&self, busid: &str, l: &Lend, revoked: bool) -> bool {
        if !l.live {
            return true;
        }
        match sysfs::export_status(&self.sys, busid) {
            Some(2) if !revoked => true,
            None => false,
            Some(_) => {
                match sysfs::export_release(&self.sys, busid) {
                    Ok(()) => eprintln!("{busid} is back from {}", l.host),
                    Err(e) => eprintln!("could not take back {busid}: {e}"),
                }
                false
            }
        }
    }

    /// `lend` removes the entry itself once the copy loop ends.
    #[cfg(not(target_os = "linux"))]
    fn keep(&self, _busid: &str, l: &Lend, revoked: bool) -> bool {
        if revoked && let Some(c) = &l.conn {
            let _ = c.shutdown(std::net::Shutdown::Both);
        }
        true
    }
}

/// Tells the host its import failed; through `crypt` when it encrypts and the kernel does not
/// already.
fn refuse(s: &TcpStream, crypt: Option<(&auth::Key, &[u8; 32])>) -> std::io::Result<()> {
    match crypt {
        Some((key, nonce)) => usbip::write_import_reply(&mut tls::wrap(std::io::empty(), s, key, nonce, true).1, None),
        None => usbip::write_import_reply(&mut &*s, None),
    }
}

/// Removes a paired host by name or id; anything it has is taken back within a second.
pub fn unpair(state_path: &Path, who: &str) -> Result<PairedHost> {
    let mut st: RelayState = state::load(state_path)?;
    let host = st.host(who).cloned().with_context(|| format!("no paired host called {who} (see `plugover hosts`)"))?;
    st.hosts.retain(|h| h.id != host.id);
    state::save(state_path, &st)?;
    Ok(host)
}

/// Adds or removes a device on the share list; `None` shares everything again.
pub fn share(state_path: &Path, change: Option<(&str, bool)>) -> Result<RelayState> {
    let mut st: RelayState = state::load(state_path)?;
    match change {
        None => st.share = None,
        Some((device, on)) => {
            let device = state::device_id(device)?;
            ensure!(on || st.share.is_some(), "{} shares every device; turn on the ones it should share instead", state::hostname());
            let list = st.share.get_or_insert_default();
            list.retain(|d| *d != device);
            if on {
                list.push(device);
            }
        }
    }
    state::save(state_path, &st)?;
    Ok(st)
}

/// Opens pairing for `PAIR_MINUTES` and returns the code to type on the host.
pub fn open_pairing(state_path: &Path) -> Result<String> {
    let window = PairingWindow { code: auth::new_code(), expires: state::now() + PAIR_MINUTES * 60, attempts: PAIR_ATTEMPTS };
    state::save(&PairingWindow::path(state_path), &window)?;
    Ok(window.code)
}

// The fake device tree is sysfs; the Windows relay asks usbipd-win instead.
#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use crate::{host, state::PairedRelay};
    use std::fs;

    #[test]
    fn paired_host_watches_devices_and_strangers_are_refused() {
        let root = std::env::temp_dir().join(format!("plugover-relay-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let dev = root.join("sys/bus/usb/devices/1-2");
        fs::create_dir_all(&dev).unwrap();
        for (f, v) in [("busnum", "1"), ("devnum", "5"), ("speed", "12"), ("idVendor", "046d"), ("idProduct", "c24f")] {
            fs::write(dev.join(f), v).unwrap();
        }
        let key = hex::encode([9u8; 32]);
        let st = RelayState {
            id: "r1".into(),
            share: None,
            hosts: vec![PairedHost { id: hex::encode([1u8; 8]), name: "desktop".into(), key: key.clone() }],
        };
        state::save(&root.join("relay.toml"), &st).unwrap();

        let relay = Arc::new(Relay::open(root.join("sys"), root.join("relay.toml")).unwrap());
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let server = relay.clone();
        thread::spawn(move || server.accept(listener));

        let mut paired = PairedRelay {
            id: "r1".into(),
            name: "relay".into(),
            address,
            host_id: hex::encode([1u8; 8]),
            key: key.clone(),
            encrypts: true,
            unencrypted: false,
        };
        let mut seen = None;
        host::watch(
            &paired,
            |snap| {
                seen = Some(snap);
                false
            },
            |_| {},
        )
        .unwrap();
        let snap = seen.expect("a snapshot arrives");
        assert!(snap.encrypted);
        assert_eq!((snap.relay_id.as_str(), snap.devices.len()), ("r1", 1));

        // A relay before 0.6 gets a plain control channel, unless it encrypted before.
        let old = old_relay(paired.address.clone());
        let real = std::mem::replace(&mut paired.address, old);
        let err = host::watch(&paired, |_| false, |_| {}).unwrap_err();
        assert!(format!("{err:#}").contains("stopped encrypting"), "{err:#}");
        paired.encrypts = false;
        let mut plain = None;
        host::watch(&paired, |snap| { plain = Some(snap.encrypted); false }, |_| {}).unwrap();
        assert_eq!(plain, Some(false));
        (paired.address, paired.encrypts) = (real, true);
        assert_eq!((snap.devices[0].id.as_str(), snap.devices[0].identity.title.as_str()), ("046d:c24f", "Logitech G29 Driving Force"));

        // The relay answers the host's pings, so the link gets timed.
        let rtt = std::cell::Cell::new(None);
        host::watch(&paired, |_| rtt.get().is_none(), |d| rtt.set(Some(d))).unwrap();
        assert!(rtt.get().is_some_and(|d| d < Duration::from_secs(1)), "{:?}", rtt.get());

        paired.key = hex::encode([8u8; 32]);
        assert!(host::watch(&paired, |_| false, |_| {}).is_err(), "a wrong key gets no snapshots");
        paired.key = key;

        // This fake sysfs has no usbip-host, so handing the device over fails; the claim on it
        // must not outlive the attempt.
        #[cfg(target_os = "linux")]
        {
            for encrypt in [true, false] {
                let Err(err) = crate::vhci::attach(&root.join("sys"), &paired, "1-2", encrypt) else { panic!("attached without usbip-host") };
                assert!(format!("{err:#}").contains("refused"), "the refusal arrives, encrypted or not: {err:#}");
                assert!(relay.lent.lock().unwrap().is_empty(), "a failed lend leaves nothing claimed");
            }
        }

        // The share list applies to a running relay right away.
        share(&root.join("relay.toml"), Some(("045e:028e", true))).unwrap();
        let mut seen = None;
        host::watch(
            &paired,
            |snap| {
                seen = Some(snap);
                false
            },
            |_| {},
        )
        .unwrap();
        assert_eq!(seen.unwrap().devices[0].identity.blocked.as_deref(), Some("not shared by the relay"));

        // So does unpairing: the host is told, not just dropped.
        assert_eq!(unpair(&root.join("relay.toml"), "desktop").unwrap().name, "desktop");
        let err = host::watch(&paired, |_| false, |_| {}).unwrap_err();
        assert!(format!("{err:#}").contains("pair again"), "{err:#}");
        assert!(unpair(&root.join("relay.toml"), "desktop").is_err());
        fs::remove_dir_all(root).unwrap();
    }

    /// A relay before 0.6: hangs up on the encrypted channels and passes the rest to `real`.
    fn old_relay(real: String) -> String {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = l.local_addr().unwrap().to_string();
        thread::spawn(move || {
            for mut c in l.incoming().flatten() {
                let real = real.clone();
                thread::spawn(move || {
                    let mut magic = [0u8; 4];
                    if c.read_exact(&mut magic).is_err() || magic[3] == b'2' {
                        return;
                    }
                    let mut up = TcpStream::connect(&real).unwrap();
                    up.write_all(&magic).unwrap();
                    let _ = net::pump(c, up.try_clone().unwrap(), up.try_clone().unwrap(), up);
                });
            }
        });
        address
    }

    #[test]
    fn pairing_needs_the_printed_code_and_uses_it_once() {
        let root = std::env::temp_dir().join(format!("plugover-pair-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("sys/bus/usb/devices")).unwrap();
        let state_path = root.join("relay.toml");
        let relay = Arc::new(Relay::open(root.join("sys"), state_path.clone()).unwrap());
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        thread::spawn(move || relay.accept(listener));

        let err = host::pair(&address, "desktop", "123456").unwrap_err().to_string();
        assert!(err.contains("pairing is closed"), "{err}");

        let code = open_pairing(&state_path).unwrap();
        let wrong = if code == "000000" { "000001" } else { "000000" };
        let err = host::pair(&address, "mallory", wrong).unwrap_err().to_string();
        assert!(err.contains("wrong code") && err.contains("4 tries left"), "{err}");

        let paired = host::pair(&address, "desktop", &code).unwrap();
        assert!(paired.encrypts, "a relay that says its version when pairing encrypts");
        assert!(host::watch(&paired, |_| false, |_| {}).is_ok(), "the new key opens the control channel");
        let st: RelayState = state::load(&state_path).unwrap();
        assert_eq!(st.hosts.iter().map(|h| h.name.as_str()).collect::<Vec<_>>(), ["desktop"]);

        let err = host::pair(&address, "mallory", &code).unwrap_err().to_string();
        assert!(err.contains("pairing is closed"), "a code pairs one host: {err}");
        fs::remove_dir_all(root).unwrap();
    }
}
