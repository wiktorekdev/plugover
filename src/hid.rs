//! Bluetooth devices paired with the relay, forwarded as HID rather than USB. The relay reads and
//! writes them through hidraw; a Linux host recreates each with uhid on the Bluetooth bus, with the
//! same ids and report descriptor, so its drivers treat it exactly like a controller paired with
//! the host itself: rumble, lights, motion sensors, touchpads and adaptive triggers included.
//!
//! On the wire, after the usual challenge and inside the usual encryption: the host sends the busid
//! as a JSON line, the relay answers with a `CREATE` frame, and then both sides exchange frames that
//! map one to one onto uhid's events.

use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

/// Bluetooth devices have busids like `bt-8c41f2974f28`.
pub fn is_bluetooth(busid: &str) -> bool {
    busid.starts_with("bt-")
}

const BUS_BLUETOOTH: u16 = 0x05;
/// Frame kinds. Report types inside them use uhid's numbers: 0 feature, 1 output, 2 input.
const CREATE: u8 = 1;
const INPUT: u8 = 2;
const OUTPUT: u8 = 3;
const GET: u8 = 4;
const GET_REPLY: u8 = 5;
const SET: u8 = 6;
const SET_REPLY: u8 = 7;
const ERROR: u8 = 8;
/// The largest report uhid carries, plus room for a frame's own fields.
const MAX_REPORT: usize = 4096;
const MAX_FRAME: usize = MAX_REPORT + 16;

/// What the host needs to recreate a device.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Info {
    pub name: String,
    /// The Bluetooth address, as the kernel reports it.
    pub uniq: String,
    pub bus: u16,
    pub vid: u16,
    pub pid: u16,
    pub country: u32,
    /// The report descriptor, hex.
    pub descriptor: String,
}

/// A Bluetooth HID device connected to this machine.
#[derive(Debug, Clone)]
pub struct Paired {
    pub busid: String,
    pub hidraw: PathBuf,
    pub info: Info,
}

impl Paired {
    pub fn descriptor(&self) -> Vec<u8> {
        hex::decode(&self.info.descriptor).unwrap_or_default()
    }
}

/// Bluetooth HID devices connected here now, from sysfs, one per device.
pub fn list(sys: &Path) -> Vec<Paired> {
    let mut found: Vec<Paired> = Vec::new();
    let Ok(dir) = fs::read_dir(sys.join("class/hidraw")) else { return found };
    let mut nodes: Vec<String> = dir.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    nodes.sort();
    for node in nodes {
        let dev = sys.join("class/hidraw").join(&node).join("device");
        let Ok(uevent) = fs::read_to_string(dev.join("uevent")) else { continue };
        let field = |k: &str| uevent.lines().find_map(|l| l.strip_prefix(k)).map(str::trim).map(str::to_string);
        // HID_ID=0005:0000054C:00000CE6
        let Some(ids) = field("HID_ID=") else { continue };
        let mut parts = ids.split(':').map(|p| u32::from_str_radix(p, 16).ok());
        let (Some(Some(bus)), Some(Some(vid)), Some(Some(pid))) = (parts.next(), parts.next(), parts.next()) else { continue };
        let uniq = field("HID_UNIQ=").unwrap_or_default().to_lowercase();
        if bus != u32::from(BUS_BLUETOOTH) || uniq.is_empty() {
            continue;
        }
        let busid = format!("bt-{}", uniq.replace(':', ""));
        if found.iter().any(|p| p.busid == busid) {
            continue;
        }
        let Ok(descriptor) = fs::read(dev.join("report_descriptor")) else { continue };
        let country = fs::read_to_string(dev.join("country")).ok().and_then(|c| u32::from_str_radix(c.trim(), 16).ok()).unwrap_or(0);
        found.push(Paired {
            busid,
            hidraw: Path::new("/dev").join(&node),
            info: Info {
                name: field("HID_NAME=").unwrap_or_default(),
                uniq,
                bus: BUS_BLUETOOTH,
                vid: vid as u16,
                pid: pid as u16,
                country,
                descriptor: hex::encode(descriptor),
            },
        });
    }
    found
}

/// Whether the descriptor numbers its reports. Without numbers, hidraw wants a 0 in front of
/// what it writes, and uhid leaves it out.
fn numbered(desc: &[u8]) -> bool {
    let mut i = 0;
    while i < desc.len() {
        let prefix = desc[i];
        if prefix == 0xfe {
            // Long item: size in the next byte.
            i += 3 + desc.get(i + 1).copied().unwrap_or(0) as usize;
            continue;
        }
        if prefix & 0xfc == 0x84 {
            return true;
        }
        i += 1 + [0, 1, 2, 4][(prefix & 3) as usize];
    }
    false
}

fn send(w: &mut impl Write, kind: u8, payload: &[u8]) -> io::Result<()> {
    let mut frame = Vec::with_capacity(3 + payload.len());
    frame.push(kind);
    frame.extend((payload.len() as u16).to_be_bytes());
    frame.extend(payload);
    // One write, so one encrypted record per frame.
    w.write_all(&frame)?;
    w.flush()
}

fn recv(r: &mut impl Read) -> io::Result<(u8, Vec<u8>)> {
    let mut head = [0u8; 3];
    r.read_exact(&mut head)?;
    let len = u16::from_be_bytes([head[1], head[2]]) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too long"));
    }
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload)?;
    Ok((head[0], payload))
}

/// Tells the host why it does not get the device.
pub fn refuse(w: &mut impl Write, why: &str) -> io::Result<()> {
    send(w, ERROR, why.as_bytes())
}

#[cfg(target_os = "linux")]
pub use linux::{Link, attach, lend};

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use crate::{auth, host, net, proto, state::PairedRelay, tls};
    use anyhow::{Context, Result, bail};
    use std::{
        fs::{File, OpenOptions},
        io::BufReader,
        net::{Shutdown, TcpStream},
        os::fd::AsRawFd,
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
        },
        thread,
    };

    /// `_IOWR('H', nr, len)` for the hidraw report ioctls.
    fn ioc(nr: u8, len: usize) -> libc::c_ulong {
        (3 << 30) | ((len as libc::c_ulong) << 16) | ((b'H' as libc::c_ulong) << 8) | nr as libc::c_ulong
    }

    /// The hidraw ioctl that gets (`get`) or sets a report of uhid type `rtype`.
    fn report_ioctl(rtype: u8, get: bool) -> u8 {
        match (rtype, get) {
            (0, false) => 0x06, // HIDIOCSFEATURE
            (0, true) => 0x07,  // HIDIOCGFEATURE
            (2, false) => 0x09, // HIDIOCSINPUT
            (2, true) => 0x0a,  // HIDIOCGINPUT
            (_, false) => 0x0b, // HIDIOCSOUTPUT
            (_, true) => 0x0c,  // HIDIOCGOUTPUT
        }
    }

    /// Runs `nr` on `buf`; the bytes it filled, or the errno.
    fn hidraw_ioctl(f: &File, nr: u8, buf: &mut [u8]) -> std::result::Result<usize, u16> {
        // SAFETY: the ioctl reads and writes at most `buf.len()` bytes, which the request encodes.
        let n = unsafe { libc::ioctl(f.as_raw_fd(), ioc(nr, buf.len()), buf.as_mut_ptr()) };
        if n < 0 { Err(io::Error::last_os_error().raw_os_error().unwrap_or(libc::EIO) as u16) } else { Ok(n as usize) }
    }

    /// Relay side: serves the device to the host over `r`/`w` until either side lets go.
    /// `stop` is the host's connection, shut when the device goes away.
    pub fn lend(dev: &Paired, r: impl Read, mut w: impl Write + Send + 'static, stop: TcpStream) -> Result<()> {
        let file = match OpenOptions::new().read(true).write(true).open(&dev.hidraw) {
            Ok(f) => f,
            Err(e) => {
                let why = format!("the relay cannot open {}: {e}", dev.hidraw.display());
                let _ = refuse(&mut w, &why);
                bail!(why);
            }
        };
        let w = Arc::new(Mutex::new(w));
        send(&mut *w.lock().unwrap(), CREATE, &serde_json::to_vec(&dev.info)?)?;
        let numbered = numbered(&dev.descriptor());

        let (mut from_dev, to_host) = (file.try_clone()?, w.clone());
        thread::spawn(move || {
            let mut buf = vec![0u8; MAX_REPORT];
            // Ends when the device disconnects or the host's connection does.
            while let Ok(n) = from_dev.read(&mut buf) {
                if n == 0 || send(&mut *to_host.lock().unwrap(), INPUT, &buf[..n]).is_err() {
                    break;
                }
            }
            let _ = stop.shutdown(Shutdown::Both);
        });

        let mut r = r;
        let mut to_dev = file;
        loop {
            let (kind, p) = match recv(&mut r) {
                Ok(f) => f,
                // The host let go, or the device went away and the reader shut the connection.
                Err(_) => return Ok(()),
            };
            match kind {
                OUTPUT if !p.is_empty() => {
                    let mut out = if numbered { Vec::new() } else { vec![0] };
                    out.extend(&p[1..]);
                    to_dev.write_all(&out)?;
                }
                GET if p.len() >= 6 => {
                    let (id, rnum, rtype) = (&p[..4], p[4], p[5]);
                    let mut buf = vec![0u8; MAX_REPORT];
                    buf[0] = rnum;
                    let mut reply = id.to_vec();
                    match hidraw_ioctl(&to_dev, report_ioctl(rtype, true), &mut buf) {
                        Ok(n) => {
                            reply.extend(0u16.to_be_bytes());
                            reply.extend(&buf[..n]);
                        }
                        Err(errno) => reply.extend(errno.to_be_bytes()),
                    }
                    send(&mut *w.lock().unwrap(), GET_REPLY, &reply)?;
                }
                SET if p.len() >= 6 => {
                    let (id, rtype) = (&p[..4], p[5]);
                    let mut data = p[6..].to_vec();
                    let errno = if data.is_empty() { libc::EINVAL as u16 } else { hidraw_ioctl(&to_dev, report_ioctl(rtype, false), &mut data).err().unwrap_or(0) };
                    let mut reply = id.to_vec();
                    reply.extend(errno.to_be_bytes());
                    send(&mut *w.lock().unwrap(), SET_REPLY, &reply)?;
                }
                _ => bail!("the host sent a frame this relay does not understand ({kind})"),
            }
        }
    }

    // uhid's events (include/uapi/linux/uhid.h): a u32 type, then a packed payload.
    const UHID_DESTROY: u32 = 1;
    const UHID_OUTPUT: u32 = 6;
    const UHID_GET_REPORT: u32 = 9;
    const UHID_GET_REPORT_REPLY: u32 = 10;
    const UHID_CREATE2: u32 = 11;
    const UHID_INPUT2: u32 = 12;
    const UHID_SET_REPORT: u32 = 13;
    const UHID_SET_REPORT_REPLY: u32 = 14;
    /// sizeof(struct uhid_event): the type and the largest payload, create2's.
    const EVENT: usize = 4 + 128 + 64 + 64 + 2 + 2 + 4 * 4 + MAX_REPORT;

    fn event(kind: u32, len: usize) -> Vec<u8> {
        let mut e = vec![0u8; 4 + len];
        e[..4].copy_from_slice(&kind.to_ne_bytes());
        e
    }

    fn put(e: &mut [u8], at: usize, bytes: &[u8]) {
        e[at..at + bytes.len()].copy_from_slice(bytes);
    }

    fn create2(info: &Info, desc: &[u8]) -> Result<Vec<u8>> {
        anyhow::ensure!(!desc.is_empty() && desc.len() <= MAX_REPORT, "the relay sent no usable report descriptor");
        let mut e = event(UHID_CREATE2, EVENT - 4);
        let text = |e: &mut Vec<u8>, at: usize, max: usize, s: &str| {
            let b = s.as_bytes();
            put(e, at, &b[..b.len().min(max - 1)]);
        };
        text(&mut e, 4, 128, &info.name);
        text(&mut e, 132, 64, "plugover");
        text(&mut e, 196, 64, &info.uniq);
        put(&mut e, 260, &(desc.len() as u16).to_ne_bytes());
        put(&mut e, 262, &info.bus.to_ne_bytes());
        put(&mut e, 264, &u32::from(info.vid).to_ne_bytes());
        put(&mut e, 268, &u32::from(info.pid).to_ne_bytes());
        put(&mut e, 276, &info.country.to_ne_bytes());
        put(&mut e, 280, desc);
        Ok(e)
    }

    /// A device recreated here; it goes when the connection ends.
    pub struct Link {
        alive: Arc<AtomicBool>,
        stream: TcpStream,
    }

    impl Link {
        pub fn alive(&self) -> bool {
            self.alive.load(Ordering::SeqCst)
        }

        pub fn close(&self) {
            let _ = self.stream.shutdown(Shutdown::Both);
        }
    }

    /// Host side: asks the relay for the device and recreates it here with uhid.
    pub fn attach(relay: &PairedRelay, busid: &str) -> Result<Link> {
        let (id, key) = host::credentials(relay)?;
        let mut s = host::connect(&relay.address)?;
        let nonce = match proto::challenge(&mut s, proto::HID, &id) {
            Ok(n) => n,
            Err(e) if matches!(e.kind(), io::ErrorKind::UnexpectedEof | io::ErrorKind::ConnectionReset | io::ErrorKind::ConnectionAborted) => {
                bail!("{} runs a Plugover older than 0.7, which cannot lend Bluetooth devices; update it", relay.name)
            }
            Err(e) => return Err(e.into()),
        };
        s.write_all(&auth::sign(&key, &nonce, busid))?;
        net::tune(&s)?;
        s.set_read_timeout(Some(net::HANDSHAKE_TIMEOUT))?;
        let (r, mut w) = tls::wrap(s.try_clone()?, s.try_clone()?, &key, &nonce, false);
        proto::send(&mut w, &serde_json::json!({ "busid": busid }))?;
        let mut r = BufReader::new(r);
        let info: Info = match recv(&mut r)? {
            (CREATE, p) => serde_json::from_slice(&p)?,
            (ERROR, p) => bail!("{}", String::from_utf8_lossy(&p)),
            (k, _) => bail!("the relay answered with frame {k} instead of the device"),
        };
        s.set_read_timeout(None)?;
        let desc = hex::decode(&info.descriptor).context("the relay sent a malformed descriptor")?;
        let uhid = OpenOptions::new().read(true).write(true).open("/dev/uhid").context("opening /dev/uhid (is it writable for you?)")?;
        (&uhid).write_all(&create2(&info, &desc)?).context("creating the device")?;

        let alive = Arc::new(AtomicBool::new(true));
        // Relay to uhid: reports from the device, and answers to the host's requests.
        let (to_uhid, gone, stop) = (uhid.try_clone()?, alive.clone(), s.try_clone()?);
        thread::spawn(move || {
            let mut to_uhid = to_uhid;
            while let Ok((kind, p)) = recv(&mut r) {
                let e = match kind {
                    INPUT if p.len() <= MAX_REPORT => {
                        let mut e = event(UHID_INPUT2, 2 + p.len());
                        put(&mut e, 4, &(p.len() as u16).to_ne_bytes());
                        put(&mut e, 6, &p);
                        e
                    }
                    GET_REPLY if p.len() >= 6 && p.len() - 6 <= MAX_REPORT => {
                        let data = &p[6..];
                        let mut e = event(UHID_GET_REPORT_REPLY, 8 + data.len());
                        put(&mut e, 4, &p[..4]);
                        put(&mut e, 8, &u16::from_be_bytes([p[4], p[5]]).to_ne_bytes());
                        put(&mut e, 10, &(data.len() as u16).to_ne_bytes());
                        put(&mut e, 12, data);
                        e
                    }
                    SET_REPLY if p.len() >= 6 => {
                        let mut e = event(UHID_SET_REPORT_REPLY, 6);
                        put(&mut e, 4, &p[..4]);
                        put(&mut e, 8, &u16::from_be_bytes([p[4], p[5]]).to_ne_bytes());
                        e
                    }
                    _ => break,
                };
                if to_uhid.write_all(&e).is_err() {
                    break;
                }
            }
            gone.store(false, Ordering::SeqCst);
            let _ = to_uhid.write_all(&event(UHID_DESTROY, 0));
            let _ = stop.shutdown(Shutdown::Both);
        });
        // uhid to relay: what the host's drivers send to the device. The ids in requests are
        // uhid's own, carried through as they are.
        let (gone, stop) = (alive.clone(), s.try_clone()?);
        thread::spawn(move || {
            let mut buf = vec![0u8; EVENT];
            let mut pfd = libc::pollfd { fd: uhid.as_raw_fd(), events: libc::POLLIN, revents: 0 };
            while gone.load(Ordering::SeqCst) {
                // SAFETY: one valid pollfd, for the length of the call.
                if unsafe { libc::poll(&mut pfd, 1, 500) } <= 0 {
                    continue;
                }
                let Ok(n) = (&uhid).read(&mut buf) else { break };
                if n < 4 {
                    continue;
                }
                let kind = u32::from_ne_bytes(buf[..4].try_into().unwrap());
                let u16_at = |at: usize| u16::from_ne_bytes([buf[at], buf[at + 1]]) as usize;
                let sent = match kind {
                    UHID_OUTPUT => {
                        let size = u16_at(4 + MAX_REPORT).min(MAX_REPORT);
                        let mut p = vec![buf[4 + MAX_REPORT + 2]];
                        p.extend(&buf[4..4 + size]);
                        send(&mut w, OUTPUT, &p)
                    }
                    UHID_GET_REPORT => send(&mut w, GET, &buf[4..10]),
                    UHID_SET_REPORT => {
                        let size = u16_at(10).min(MAX_REPORT);
                        let mut p = buf[4..10].to_vec();
                        p.extend(&buf[12..12 + size]);
                        send(&mut w, SET, &p)
                    }
                    // Start, stop, open and close need nothing from the relay.
                    _ => Ok(()),
                };
                if sent.is_err() {
                    break;
                }
            }
            gone.store(false, Ordering::SeqCst);
            let _ = stop.shutdown(Shutdown::Both);
        });
        Ok(Link { alive, stream: s })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip_and_long_ones_are_refused() {
        let mut buf = Vec::new();
        send(&mut buf, INPUT, &[1, 2, 3]).unwrap();
        send(&mut buf, SET_REPLY, &[]).unwrap();
        let mut r = &buf[..];
        assert_eq!(recv(&mut r).unwrap(), (INPUT, vec![1, 2, 3]));
        assert_eq!(recv(&mut r).unwrap(), (SET_REPLY, vec![]));
        let long = [vec![INPUT], ((MAX_FRAME + 1) as u16).to_be_bytes().to_vec()].concat();
        assert_eq!(recv(&mut &long[..]).unwrap_err().kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn report_ids_are_found_in_descriptors() {
        // Usage Page (Generic Desktop), Usage (Game Pad), Collection, Report ID 1, End.
        assert!(numbered(&[0x05, 0x01, 0x09, 0x05, 0xa1, 0x01, 0x85, 0x01, 0xc0]));
        // A boot keyboard's: no report ids.
        assert!(!numbered(&[0x05, 0x01, 0x09, 0x06, 0xa1, 0x01, 0x75, 0x01, 0x95, 0x08, 0xc0]));
        // A long item whose data looks like a report id is skipped.
        assert!(!numbered(&[0xfe, 0x02, 0x00, 0x85, 0x01]));
    }

    #[test]
    fn lists_bluetooth_devices_from_sysfs_only() {
        let root = std::env::temp_dir().join(format!("plugover-hid-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let node = |name: &str, uevent: &str| {
            let dev = root.join("class/hidraw").join(name).join("device");
            fs::create_dir_all(&dev).unwrap();
            fs::write(dev.join("uevent"), uevent).unwrap();
            fs::write(dev.join("report_descriptor"), [0x05u8, 0x01]).unwrap();
            fs::write(dev.join("country"), "00\n").unwrap();
        };
        node("hidraw3", "DRIVER=playstation\nHID_ID=0005:0000054C:00000CE6\nHID_NAME=DualSense Wireless Controller\nHID_UNIQ=8C:41:F2:97:4F:28\n");
        node("hidraw4", "DRIVER=hid-generic\nHID_ID=0003:0000046D:0000C24F\nHID_NAME=G29\nHID_UNIQ=\n");
        let found = list(&root);
        assert_eq!(found.len(), 1, "the USB device is not listed here");
        let d = &found[0];
        assert_eq!((d.busid.as_str(), d.hidraw.to_str().unwrap()), ("bt-8c41f2974f28", "/dev/hidraw3"));
        assert_eq!((d.info.vid, d.info.pid, d.info.bus, d.info.uniq.as_str()), (0x054c, 0x0ce6, BUS_BLUETOOTH, "8c:41:f2:97:4f:28"));
        assert_eq!(d.descriptor(), [0x05, 0x01]);
        let _ = fs::remove_dir_all(root);
    }
}
