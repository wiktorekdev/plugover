//! The relay: the machine the wheel is plugged into. It lends devices to paired hosts, tells
//! them what is plugged in, and takes devices back when a host lets go or disappears.

use crate::{
    auth,
    catalog::{self, Identity, UsbIds},
    discovery, net,
    proto::{self, DeviceInfo, PairConfirm, PairHello, PairReply, Snapshot},
    state::{self, PairedHost, PairingWindow, RelayState},
    sysfs, usbip,
};
use anyhow::{Context, Result, anyhow, bail, ensure};
use std::{
    collections::HashMap,
    io::{BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    os::fd::AsRawFd,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

pub const PAIR_MINUTES: u64 = 15;
const PAIR_ATTEMPTS: u8 = 5;
const HEARTBEAT: Duration = Duration::from_secs(5);

pub struct Relay {
    sys: PathBuf,
    name: String,
    state_path: PathBuf,
    state: Mutex<RelayState>,
    ids: Option<UsbIds>,
    /// HID descriptors vanish while a device is lent out, so remember what each one was.
    known: Mutex<HashMap<(String, u16, u16), Identity>>,
    /// busid -> name of the host that has it.
    lent: Mutex<HashMap<String, String>>,
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
            state_path,
            state: Mutex::new(st),
            ids: UsbIds::load(),
            known: Mutex::default(),
            lent: Mutex::default(),
            pairing: Mutex::default(),
        })
    }

    pub fn serve(self: Arc<Self>, port: u16) -> Result<()> {
        let listener = TcpListener::bind(("0.0.0.0", port)).with_context(|| format!("listening on port {port}"))?;
        let id = self.state.lock().unwrap().id.clone();
        let _mdns = discovery::advertise(&self.name, &id, port)
            .map_err(|e| eprintln!("not announcing over mDNS: {e:#}"))
            .ok();
        eprintln!("relay {} ({id}) listening on port {port}", self.name);
        self.accept(listener)
    }

    fn accept(self: Arc<Self>, listener: TcpListener) -> Result<()> {
        // Nothing is lent yet, so anything usbip-host still claims is left over from a crash.
        for busid in sysfs::export_claimed(&self.sys) {
            match sysfs::export_release(&self.sys, &busid) {
                Ok(()) => eprintln!("gave {busid} back to its local driver (left over from an earlier run)"),
                Err(e) => eprintln!("could not give {busid} back: {e}"),
            }
        }
        let me = self.clone();
        thread::spawn(move || loop {
            thread::sleep(Duration::from_secs(1));
            me.reclaim();
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
        s.read_exact(&mut magic)?;
        match &magic {
            proto::DATA => self.lend(s),
            proto::CONTROL => self.control(s),
            proto::PAIR => self.pair(s),
            _ => bail!("not a Plugover host"),
        }
    }

    /// Reads the host id and sends the nonce. Returns the host and the nonce to verify against.
    fn challenge(&self, s: &mut TcpStream) -> Result<(PairedHost, [u8; 32])> {
        let mut id = [0u8; 8];
        s.read_exact(&mut id)?;
        let id = hex::encode(id);
        let host = self.state.lock().unwrap().hosts.iter().find(|h| h.id == id).cloned();
        let host = host.ok_or_else(|| anyhow!("unknown host {id}; pair it first"))?;
        let nonce: [u8; 32] = auth::random();
        s.write_all(&nonce)?;
        Ok((host, nonce))
    }

    fn lend(&self, mut s: TcpStream) -> Result<()> {
        let (host, nonce) = self.challenge(&mut s)?;
        let mut tag = [0u8; auth::TAG_LEN];
        s.read_exact(&mut tag)?;
        let busid = usbip::read_import_request(&mut s)?;
        let dev = if auth::verify(&state::decode_key(&host.key)?, &nonce, &busid, &tag) {
            self.lendable(&busid)
        } else {
            Err(anyhow!("{} failed the challenge", host.name))
        };
        let dev = match dev {
            Ok(d) => d,
            Err(e) => {
                usbip::write_import_reply(&mut s, None)?;
                return Err(e);
            }
        };
        sysfs::export_bind(&self.sys, &busid)?;
        net::tune(&s)?;
        // Same order as usbipd: the kernel takes the socket, then the reply goes out.
        sysfs::export_socket(&self.sys, &busid, s.as_raw_fd())?;
        self.lent.lock().unwrap().insert(busid.clone(), host.name.clone());
        usbip::write_import_reply(&mut s, Some(&net::exported(&dev)))?;
        eprintln!("lent {busid} to {}", host.name);
        Ok(())
    }

    fn lendable(&self, busid: &str) -> Result<sysfs::UsbDevice> {
        let dev = sysfs::list(&self.sys)?
            .into_iter()
            .find(|d| d.busid == busid)
            .with_context(|| format!("no device at {busid}"))?;
        if let Some(why) = catalog::blocked_reason(&dev) {
            bail!("{busid} is a {why}, which is never shared");
        }
        ensure!(!self.lent.lock().unwrap().contains_key(busid), "{busid} is already in use");
        Ok(dev)
    }

    fn control(&self, mut s: TcpStream) -> Result<()> {
        let (host, nonce) = self.challenge(&mut s)?;
        let mut tag = [0u8; auth::TAG_LEN];
        s.read_exact(&mut tag)?;
        ensure!(
            auth::verify(&state::decode_key(&host.key)?, &nonce, proto::CONTROL_CONTEXT, &tag),
            "{} failed the challenge",
            host.name
        );
        net::tune(&s)?;
        s.set_write_timeout(Some(HEARTBEAT))?;
        let (mut last, mut sent_at) = (None, Instant::now());
        loop {
            let snap = self.snapshot()?;
            if last.as_ref() != Some(&snap) || sent_at.elapsed() >= HEARTBEAT {
                proto::send(&mut s, &snap)?;
                (last, sent_at) = (Some(snap), Instant::now());
            }
            // ponytail: polls sysfs every 500 ms; a uevent socket would report plugs instantly.
            thread::sleep(Duration::from_millis(500));
        }
    }

    fn snapshot(&self) -> Result<Snapshot> {
        let lent = self.lent.lock().unwrap().clone();
        let mut known = self.known.lock().unwrap();
        let devices = sysfs::list(&self.sys)?
            .iter()
            .map(|d| {
                let key = (d.busid.clone(), d.vid, d.pid);
                let fresh = catalog::identify(d, self.ids.as_ref());
                let identity = if d.driver.as_deref() == Some("usbip-host") {
                    known.get(&key).cloned().unwrap_or(fresh)
                } else {
                    known.insert(key, fresh.clone());
                    fresh
                };
                DeviceInfo {
                    busid: d.busid.clone(),
                    id: format!("{:04x}:{:04x}", d.vid, d.pid),
                    identity,
                    lent_to: lent.get(&d.busid).cloned(),
                }
            })
            .collect();
        let st = self.state.lock().unwrap();
        Ok(Snapshot { relay_id: st.id.clone(), relay: self.name.clone(), devices })
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
            return Err(refuse(&mut w, format!("pairing is closed; run `sudo plugover code` on {} for a code", self.name)));
        }
        let host_id: auth::HostId =
            hex::decode(&hello.host_id).ok().and_then(|b| b.try_into().ok()).context("pairing request has a malformed id")?;
        let host_msg = hex::decode(&hello.pake).context("pairing request has a malformed key")?;
        let (pake, msg) = auth::pake_start(&window.code, false);
        let relay_id = self.state.lock().unwrap().id.clone();
        proto::send(&mut w, &PairReply::Pake { relay_id, relay: self.name.clone(), pake: hex::encode(msg) })?;
        let confirm: PairConfirm = proto::recv(&mut r)?;
        let key = auth::pake_finish(pake, &host_msg).context("pairing request has a malformed key")?;
        // The host proves the code first; a wrong guess learns nothing and costs an attempt.
        if !auth::verify(&key, &host_id, auth::CONFIRM_HOST, &hex::decode(confirm.tag).unwrap_or_default()) {
            window.attempts -= 1;
            if window.attempts == 0 {
                let _ = fs::remove_file(&window_path);
                return Err(refuse(&mut w, "wrong code; too many tries, so pairing is closed until you run `sudo plugover code` again".into()));
            }
            state::save(&window_path, &window)?;
            return Err(refuse(&mut w, format!("wrong code from {} ({} tries left)", hello.name, window.attempts)));
        }

        let host = PairedHost { id: hex::encode(host_id), name: hello.name.clone(), key: hex::encode(key) };
        {
            let mut st = self.state.lock().unwrap();
            st.hosts.retain(|h| h.id != host.id);
            st.hosts.push(host);
            state::save(&self.state_path, &*st)?;
        }
        // A code pairs one host.
        let _ = fs::remove_file(&window_path);
        proto::send(&mut w, &PairReply::Paired { tag: hex::encode(auth::sign(&key, &host_id, auth::CONFIRM_RELAY)) })?;
        eprintln!("paired with {}", hello.name);
        Ok(())
    }

    // ponytail: polls usbip_status every second; a uevent watch would react instantly.
    fn reclaim(&self) {
        self.lent.lock().unwrap().retain(|busid, host| match sysfs::export_status(&self.sys, busid) {
            Some(2) => true,
            None => false,
            Some(_) => {
                match sysfs::export_release(&self.sys, busid) {
                    Ok(()) => eprintln!("{busid} is back from {host}"),
                    Err(e) => eprintln!("could not take back {busid}: {e}"),
                }
                false
            }
        });
    }
}

/// Opens pairing for `PAIR_MINUTES` and returns the code to type on the host.
pub fn open_pairing(state_path: &Path) -> Result<String> {
    let window = PairingWindow { code: auth::new_code(), expires: state::now() + PAIR_MINUTES * 60, attempts: PAIR_ATTEMPTS };
    state::save(&PairingWindow::path(state_path), &window)?;
    Ok(window.code)
}

#[cfg(test)]
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
            hosts: vec![PairedHost { id: hex::encode([1u8; 8]), name: "desktop".into(), key: key.clone() }],
        };
        state::save(&root.join("relay.toml"), &st).unwrap();

        let relay = Arc::new(Relay::open(root.join("sys"), root.join("relay.toml")).unwrap());
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        thread::spawn(move || relay.accept(listener));

        let mut paired = PairedRelay { id: "r1".into(), name: "relay".into(), address, host_id: hex::encode([1u8; 8]), key };
        let mut seen = None;
        host::watch(&paired, |snap| {
            seen = Some(snap);
            false
        })
        .unwrap();
        let snap = seen.expect("a snapshot arrives");
        assert_eq!((snap.relay_id.as_str(), snap.devices.len()), ("r1", 1));
        assert_eq!((snap.devices[0].id.as_str(), snap.devices[0].identity.title.as_str()), ("046d:c24f", "Logitech G29 Driving Force"));

        paired.key = hex::encode([8u8; 32]);
        assert!(host::watch(&paired, |_| false).is_err(), "a wrong key gets no snapshots");
        fs::remove_dir_all(root).unwrap();
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
        assert!(host::watch(&paired, |_| false).is_ok(), "the new key opens the control channel");
        let st: RelayState = state::load(&state_path).unwrap();
        assert_eq!(st.hosts.iter().map(|h| h.name.as_str()).collect::<Vec<_>>(), ["desktop"]);

        let err = host::pair(&address, "mallory", &code).unwrap_err().to_string();
        assert!(err.contains("pairing is closed"), "a code pairs one host: {err}");
        fs::remove_dir_all(root).unwrap();
    }
}
