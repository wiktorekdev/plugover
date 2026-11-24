//! Keyboards and mice from a relay, passed into a stream's own screen.
//!
//! A device from a relay shows up like one plugged in here, so the desktop takes its keyboard and
//! mouse. That is right until a stream runs the game on a screen of its own: a headless gamescope
//! (punktfunk's dedicated game session, or a Sunshine app launched in gamescope) reads no input
//! devices at all, only what is sent to its EIS socket. While a stream runs and such a gamescope
//! is up, Plugover takes the keyboard and mouse nodes of the devices it plugged in for itself
//! (EVIOCGRAB, so the desktop stops seeing them) and sends their events there through libei. Pads,
//! wheels and joysticks are left alone: games read those directly, force feedback included.

use anyhow::{Context, Result, bail};
use reis::{
    ei,
    event::{DeviceCapability, EiEvent, EiEventConverter},
    handshake::EiHandshaker,
};
use std::{
    collections::HashSet,
    fs::{self, File, OpenOptions},
    io::{self, Read},
    os::{
        fd::{AsRawFd, RawFd},
        unix::{fs::FileTypeExt, fs::OpenOptionsExt, net::UnixStream},
    },
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

/// What a node carries that the game's screen should get.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Kind {
    pub keyboard: bool,
    pub pointer: bool,
}

const EV_SYN: u16 = 0;
const EV_KEY: u16 = 1;
const EV_REL: u16 = 2;
const SYN_REPORT: u16 = 0;
const REL_X: u16 = 0;
const REL_Y: u16 = 1;
const REL_HWHEEL: u16 = 6;
const REL_WHEEL: u16 = 8;
const REL_WHEEL_HI_RES: u16 = 11;
const REL_HWHEEL_HI_RES: u16 = 12;
/// BTN_LEFT to BTN_TASK: mouse buttons. Other codes from 0x100 to 0x15f are buttons of
/// joysticks, pads and pens.
const MOUSE_BUTTONS: std::ops::RangeInclusive<u16> = 0x110..=0x117;
const BUTTONS: std::ops::RangeInclusive<u16> = 0x100..=0x15f;
/// How long a stopped or failing forward may take to notice.
const TICK: Duration = Duration::from_millis(100);
/// How long to wait for every key to be let go before taking a node anyway: a device may report
/// a key that is always down.
const LET_GO: Duration = Duration::from_secs(5);

/// One bitmap from `capabilities/` in sysfs: hex words of the kernel's `long`, most significant
/// first.
// ponytail: the word is this process's usize; an armv7 build on an arm64 kernel would misread it.
fn bitmap(dev: &Path, file: &str) -> Vec<u64> {
    let text = fs::read_to_string(dev.join("capabilities").join(file)).unwrap_or_default();
    let words: Vec<u64> = text.split_whitespace().filter_map(|w| u64::from_str_radix(w, 16).ok()).collect();
    words.into_iter().rev().collect()
}

fn has(map: &[u64], bit: u16) -> bool {
    let word = usize::BITS as usize;
    map.get(bit as usize / word).is_some_and(|w| w >> (bit as usize % word) & 1 == 1)
}

/// Whether the input device at `dev` (an `inputN` directory) is a keyboard or mouse to pass on.
/// Anything with joystick axes or buttons is not: a pad whose node also has a key (the Xbox
/// share button is KEY_RECORD) must stay where games read it.
pub fn classify(dev: &Path) -> Option<Kind> {
    let (key, rel, abs) = (bitmap(dev, "key"), bitmap(dev, "rel"), bitmap(dev, "abs"));
    // Sticks, triggers, hats and touch surfaces; volume (0x20) and misc (0x28) come on keyboards.
    let axes = (0x00..=0x1f).chain(0x2f..=0x3f).any(|a| has(&abs, a));
    let pad_buttons = (0x120..=0x13f).chain(0x2c0..=0x2e7).any(|b| has(&key, b));
    if axes || pad_buttons {
        return None;
    }
    let kind = Kind {
        keyboard: (1..0x100).any(|k| has(&key, k)),
        pointer: has(&rel, REL_X) && has(&rel, REL_Y) && has(&key, 0x110),
    };
    (kind.keyboard || kind.pointer).then_some(kind)
}

/// The keyboard and mouse nodes (`/dev/input/eventN`) of the device on `busid`, which landed on
/// vhci port `port`, or for a Bluetooth device, was recreated through uhid.
pub fn nodes(sys: &Path, busid: &str, port: u32) -> Vec<(PathBuf, Kind)> {
    let usb = if crate::hid::is_bluetooth(busid) { None } else { crate::sysfs::vhci_local_busid(sys, port) };
    if usb.is_none() && !crate::hid::is_bluetooth(busid) {
        return Vec::new();
    }
    let Ok(dir) = fs::read_dir(sys.join("class/input")) else { return Vec::new() };
    let mut found: Vec<(PathBuf, Kind)> = dir
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("event"))
        .filter_map(|e| {
            let dev = fs::canonicalize(e.path().join("device")).ok()?;
            let ours = match &usb {
                // The USB device is a path component under the vhci controller: .../vhci_hcd.0/usb3/3-1/3-1:1.0/...
                Some(local) => dev.components().any(|c| c.as_os_str() == "vhci_hcd.0") && dev.components().any(|c| c.as_os_str() == local.as_str()),
                None => {
                    let uniq = fs::read_to_string(dev.join("uniq")).unwrap_or_default().trim().to_lowercase().replace(':', "");
                    dev.components().any(|c| c.as_os_str() == "uhid") && !uniq.is_empty() && busid.strip_prefix("bt-") == Some(uniq.as_str())
                }
            };
            ours.then(|| Some((Path::new("/dev/input").join(e.file_name()), classify(&dev)?))).flatten()
        })
        .collect();
    found.sort_by(|a, b| a.0.cmp(&b.0));
    found
}

/// EIS sockets of the gamescopes running here, best first: the ones punktfunk names as its
/// streams' screens, then the rest, newest first (a stream's own screen is usually the one that
/// started last; a Sunshine app in gamescope names none).
// ponytail: newest wins; with two streams on two gamescopes at once, both send to the newer one.
pub fn screens(runtime: &Path) -> Vec<PathBuf> {
    let live = gamescopes(runtime);
    let named: Vec<PathBuf> = punktfunk_screens(runtime).into_iter().filter(|p| live.contains(p)).collect();
    let rest = live.into_iter().filter(|p| !named.contains(p)).collect::<Vec<_>>();
    named.into_iter().chain(rest).collect()
}

/// The sockets punktfunk's dedicated sessions say their gamescope serves, newest first. Each
/// session it spawns a gamescope for writes that gamescope's `LIBEI_SOCKET` to
/// `punktfunk-gamescope-ei` (or `punktfunk-gamescope-<client>-ei` when isolated) in this runtime
/// dir, and removes the file with the session.
fn punktfunk_screens(runtime: &Path) -> Vec<PathBuf> {
    let Ok(dir) = fs::read_dir(runtime) else { return Vec::new() };
    let mut found: Vec<(std::time::SystemTime, PathBuf)> = dir
        .flatten()
        .filter(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            // A symlink could point anywhere; punktfunk refuses those too.
            name.starts_with("punktfunk-gamescope") && name.ends_with("-ei") && e.file_type().is_ok_and(|t| t.is_file())
        })
        .filter_map(|e| {
            let text = fs::read_to_string(e.path()).ok()?;
            let name = text.lines().next()?.trim();
            if name.is_empty() {
                return None;
            }
            Some((e.metadata().ok()?.modified().ok()?, runtime.join(name)))
        })
        .collect();
    found.sort_by_key(|f| std::cmp::Reverse(f.0));
    found.into_iter().map(|(_, p)| p).collect()
}

/// Every live gamescope EIS socket here, newest first.
fn gamescopes(runtime: &Path) -> Vec<PathBuf> {
    let Ok(dir) = fs::read_dir(runtime) else { return Vec::new() };
    let locks = fs::read_to_string("/proc/locks").ok();
    let mut found: Vec<(std::time::SystemTime, PathBuf)> = dir
        .flatten()
        .filter(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.starts_with("gamescope-") && name.ends_with("-ei") && e.file_type().is_ok_and(|t| t.is_socket())
        })
        .filter(|e| locks.as_deref().is_none_or(|l| alive(&e.path(), l)))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    found.sort_by_key(|f| std::cmp::Reverse(f.0));
    found.into_iter().map(|(_, p)| p).collect()
}

/// Whether the gamescope that made `socket` still runs: libei keeps a lock on `<socket>.lock`
/// while it serves the socket, and a gamescope that crashed leaves the socket behind but not the
/// lock. Read from /proc/locks, so nothing here ever takes the lock itself.
fn alive(socket: &Path, locks: &str) -> bool {
    use std::os::unix::fs::MetadataExt;
    let mut lock = socket.as_os_str().to_owned();
    lock.push(".lock");
    // No lock file: some other libei, which cannot be told apart from a dead one.
    let Ok(m) = fs::metadata(lock) else { return true };
    let id = format!("{:02x}:{:02x}:{}", libc::major(m.dev()), libc::minor(m.dev()), m.ino());
    // `1: FLOCK  ADVISORY  WRITE 2622729 00:3e:1320 0 EOF`; waiters have a `->` and do not count.
    locks.lines().any(|l| l.split_whitespace().nth(5) == Some(id.as_str()))
}

/// The directory gamescope puts its sockets in, for this user.
pub fn runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(|| {
        // SAFETY: getuid cannot fail.
        PathBuf::from(format!("/run/user/{}", unsafe { libc::getuid() }))
    })
}

/// A node being passed into a screen, on its own thread.
pub struct Forward {
    stop: Arc<AtomicBool>,
    /// The screen it reaches, once its events go there.
    into: Arc<Mutex<Option<String>>>,
    thread: Option<JoinHandle<Result<()>>>,
}

impl Forward {
    pub fn start(node: PathBuf, kind: Kind, screens: Vec<PathBuf>) -> Self {
        let (stop, into) = (Arc::new(AtomicBool::new(false)), Arc::new(Mutex::new(None)));
        let (stop2, into2) = (stop.clone(), into.clone());
        let thread = thread::spawn(move || forward(&node, kind, &screens, &stop2, &into2));
        Self { stop, into, thread: Some(thread) }
    }

    /// The screen's name (`gamescope-0`) while events go there.
    pub fn screen(&self) -> Option<String> {
        self.into.lock().unwrap().clone()
    }

    pub fn finished(&self) -> bool {
        self.thread.as_ref().is_none_or(|t| t.is_finished())
    }

    /// Stops passing events on and gives the node back to the desktop; why it ended by itself, if
    /// it did.
    pub fn stop(mut self) -> Option<String> {
        self.stop.store(true, Ordering::SeqCst);
        match self.thread.take()?.join() {
            Ok(Ok(())) => None,
            Ok(Err(e)) => Some(format!("{e:#}")),
            Err(_) => Some("passing it on crashed".into()),
        }
    }
}

impl Drop for Forward {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

/// `struct input_event`: two kernel longs of time, then type, code and value.
const EVENT: usize = 2 * size_of::<libc::c_ulong>() + 8;

fn parse(buf: &[u8]) -> impl Iterator<Item = (u16, u16, i32)> + '_ {
    buf.as_chunks::<EVENT>().0.iter().map(|e| {
        let t = &e[EVENT - 8..];
        (u16::from_ne_bytes([t[0], t[1]]), u16::from_ne_bytes([t[2], t[3]]), i32::from_ne_bytes([t[4], t[5], t[6], t[7]]))
    })
}

/// Whether any key or button of the node is down now. Taking it then would leave that key pressed
/// on the desktop, which would never see it let go.
fn pressed(f: &File) -> bool {
    // EVIOCGKEY: _IOR('E', 0x18, len), a bitmap of KEY_MAX (0x2ff) + 1 bits.
    let mut bits = [0u8; 0x300 / 8];
    let req = (2 << 30) | ((bits.len() as libc::c_ulong) << 16) | ((b'E' as libc::c_ulong) << 8) | 0x18;
    // SAFETY: the request encodes the buffer's length, which the kernel fills at most.
    let n = unsafe { libc::ioctl(f.as_raw_fd(), req as libc::Ioctl, bits.as_mut_ptr()) };
    n > 0 && bits.iter().any(|b| *b != 0)
}

fn grab(f: &File, on: bool) -> io::Result<()> {
    // _IOW('E', 0x90, int)
    const EVIOCGRAB: libc::c_ulong = 0x4004_4590;
    // SAFETY: EVIOCGRAB takes an int by value.
    if unsafe { libc::ioctl(f.as_raw_fd(), EVIOCGRAB as libc::Ioctl, libc::c_int::from(on)) } == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

/// Which fds are readable within `timeout`.
fn poll(fds: &[RawFd], timeout: Duration) -> io::Result<Vec<bool>> {
    let mut p: Vec<libc::pollfd> = fds.iter().map(|&fd| libc::pollfd { fd, events: libc::POLLIN, revents: 0 }).collect();
    // SAFETY: `p` holds `p.len()` pollfds.
    let n = unsafe { libc::poll(p.as_mut_ptr(), p.len() as libc::nfds_t, timeout.as_millis() as libc::c_int) };
    if n < 0 {
        let e = io::Error::last_os_error();
        return if e.kind() == io::ErrorKind::Interrupted { Ok(vec![false; fds.len()]) } else { Err(e) };
    }
    Ok(p.iter().map(|p| p.revents != 0).collect())
}

fn now_us() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: CLOCK_MONOTONIC always exists; `ts` is a valid timespec.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1_000_000 + ts.tv_nsec as u64 / 1000
}

/// Reads what gamescope sent into the context's buffer.
fn read(ctx: &ei::Context, name: &str) -> Result<()> {
    match ctx.read() {
        Ok(0) => bail!("{name} closed"),
        Ok(_) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => bail!("{name} closed"),
        Err(e) => Err(anyhow::Error::new(e).context(format!("reading from {name}"))),
    }
}

fn connect(screens: &[PathBuf], stop: &AtomicBool) -> Result<(ei::Context, EiEventConverter, String)> {
    let (stream, path) = screens
        .iter()
        .find_map(|p| Some((UnixStream::connect(p).ok()?, p)))
        .context("no gamescope answers on its input socket")?;
    let name = path.file_name().unwrap_or_default().to_string_lossy().trim_end_matches("-ei").to_string();
    let ctx = ei::Context::new(stream)?;
    let mut hands = EiHandshaker::new("plugover", ei::handshake::ContextType::Sender);
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        anyhow::ensure!(!stop.load(Ordering::SeqCst), "stopped");
        anyhow::ensure!(Instant::now() < deadline, "{name} did not answer");
        if !poll(&[ctx.as_raw_fd()], TICK)?[0] {
            continue;
        }
        read(&ctx, &name)?;
        while let Some(pending) = ctx.pending_event() {
            let reis::PendingRequestResult::Request(event) = pending else { bail!("{name} sent something unreadable") };
            if let Some(resp) = hands.handle_event(event).map_err(|e| anyhow::anyhow!("{e}"))? {
                let conv = EiEventConverter::new(&ctx, resp);
                return Ok((ctx, conv, name));
            }
        }
    }
}

/// Input devices the screen gave us, and which of them may be used now.
#[derive(Default)]
struct Emulated {
    devices: Vec<(reis::event::Device, bool)>,
    sequence: u32,
}

impl Emulated {
    fn with(&self, cap: DeviceCapability) -> Option<&reis::event::Device> {
        self.devices.iter().find(|(d, resumed)| *resumed && d.has_capability(cap)).map(|(d, _)| d)
    }

    /// Ready for what `kind` sends.
    fn ready(&self, kind: Kind) -> bool {
        (!kind.keyboard || self.with(DeviceCapability::Keyboard).is_some()) && (!kind.pointer || self.with(DeviceCapability::Pointer).is_some())
    }
}

/// Events of one evdev frame, sent as one EI frame.
#[derive(Default)]
struct Pending {
    keys: Vec<(u32, bool)>,
    buttons: Vec<(u32, bool)>,
    motion: (f32, f32),
    scroll: (i32, i32),
}

fn forward(node: &Path, kind: Kind, screens: &[PathBuf], stop: &AtomicBool, into: &Mutex<Option<String>>) -> Result<()> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(node)
        .with_context(|| format!("cannot read {} (join the input group, or run `plugover setup --host` again)", node.display()))?;
    let (ctx, mut conv, name) = connect(screens, stop)?;
    let mut em = Emulated::default();
    let mut grabbed = false;
    // Since when the screen could take the node, while a key is still down.
    let mut waiting: Option<Instant> = None;
    let mut held: HashSet<(u32, bool)> = HashSet::new(); // (code, is a mouse button)
    let mut frame = Pending::default();
    // A device with hi-res wheel events sends both; the hi-res ones are already in 120ths.
    let rel = bitmap(&node_sysfs(node), "rel");
    let (hires_v, hires_h) = (has(&rel, REL_WHEEL_HI_RES), has(&rel, REL_HWHEEL_HI_RES));
    let mut buf = vec![0u8; EVENT * 64];

    let result = (|| -> Result<()> {
        loop {
            if stop.load(Ordering::SeqCst) {
                return Ok(());
            }
            let ready = poll(&[ctx.as_raw_fd(), file.as_raw_fd()], TICK)?;
            if ready[0] {
                read(&ctx, &name)?;
            }
            // Also what came in with the end of the handshake, which poll does not see again.
            while let Some(pending) = ctx.pending_event() {
                if let reis::PendingRequestResult::Request(event) = pending {
                    conv.handle_event(event).map_err(|e| anyhow::anyhow!("{name}: {e}"))?;
                }
            }
            while let Some(event) = conv.next_event() {
                match event {
                    EiEvent::SeatAdded(s) => {
                        let mut caps = reis::enumflags2::BitFlags::empty();
                        if kind.keyboard {
                            caps |= DeviceCapability::Keyboard;
                        }
                        if kind.pointer {
                            caps |= DeviceCapability::Pointer | DeviceCapability::Button | DeviceCapability::Scroll;
                        }
                        s.seat.bind_capabilities(caps);
                    }
                    EiEvent::DeviceAdded(d) => em.devices.push((d.device, false)),
                    EiEvent::DeviceResumed(d) => {
                        em.sequence += 1;
                        d.device.device().start_emulating(conv.connection().serial(), em.sequence);
                        em.devices.iter_mut().filter(|(x, _)| *x == d.device).for_each(|(_, r)| *r = true);
                    }
                    EiEvent::DevicePaused(d) => em.devices.iter_mut().filter(|(x, _)| *x == d.device).for_each(|(_, r)| *r = false),
                    EiEvent::DeviceRemoved(d) => em.devices.retain(|(x, _)| *x != d.device),
                    EiEvent::Disconnected(d) => bail!("{name} closed{}", d.explanation.filter(|e| !e.is_empty()).map(|e| format!(": {e}")).unwrap_or_default()),
                    _ => {}
                }
            }
            // Take the node from the desktop only once its events have somewhere to go, and no key
            // is down: the desktop must see every key it saw pressed let go.
            if !grabbed && em.ready(kind) && (!pressed(&file) || waiting.get_or_insert_with(Instant::now).elapsed() >= LET_GO) {
                grab(&file, true).with_context(|| format!("another program holds {} for itself", node.display()))?;
                grabbed = true;
                *into.lock().unwrap() = Some(name.clone());
            }
            let _ = ctx.flush();
            if !ready[1] {
                continue;
            }
            let n = match file.read(&mut buf) {
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
                // The device was unplugged.
                Err(e) if e.raw_os_error() == Some(libc::ENODEV) => return Ok(()),
                Err(e) => return Err(e.into()),
            };
            if n == 0 {
                return Ok(());
            }
            if !grabbed {
                continue;
            }
            for (ty, code, value) in parse(&buf[..n]) {
                match (ty, code) {
                    // 2 is autorepeat, which the screen does itself.
                    (EV_KEY, c) if value != 2 && MOUSE_BUTTONS.contains(&c) => frame.buttons.push((c.into(), value != 0)),
                    (EV_KEY, c) if value != 2 && !BUTTONS.contains(&c) => frame.keys.push((c.into(), value != 0)),
                    (EV_REL, REL_X) => frame.motion.0 += value as f32,
                    (EV_REL, REL_Y) => frame.motion.1 += value as f32,
                    // Up is positive in evdev and negative in EI.
                    (EV_REL, REL_WHEEL) if !hires_v => frame.scroll.1 -= value * 120,
                    (EV_REL, REL_WHEEL_HI_RES) => frame.scroll.1 -= value,
                    (EV_REL, REL_HWHEEL) if !hires_h => frame.scroll.0 += value * 120,
                    (EV_REL, REL_HWHEEL_HI_RES) => frame.scroll.0 += value,
                    (EV_SYN, SYN_REPORT) => send(&conv, &em, std::mem::take(&mut frame), &mut held),
                    _ => {}
                }
            }
            let _ = ctx.flush();
        }
    })();

    // Nothing stays pressed in the game once the desktop has the device back.
    let release = Pending {
        keys: held.iter().filter(|(_, b)| !b).map(|&(c, _)| (c, false)).collect(),
        buttons: held.iter().filter(|(_, b)| *b).map(|&(c, _)| (c, false)).collect(),
        ..Default::default()
    };
    send(&conv, &em, release, &mut held);
    for (d, resumed) in &em.devices {
        if *resumed {
            d.device().stop_emulating(conv.connection().serial());
        }
    }
    let _ = ctx.flush();
    if grabbed {
        let _ = grab(&file, false);
    }
    *into.lock().unwrap() = None;
    result
}

/// `/dev/input/eventN` to its `inputN` directory in sysfs.
fn node_sysfs(node: &Path) -> PathBuf {
    Path::new("/sys/class/input").join(node.file_name().unwrap_or_default()).join("device")
}

/// Which presses and releases to send: a release the game never saw pressed is dropped, so
/// keys held while the forward started do not get stuck the other way round.
fn changes(list: &[(u32, bool)], button: bool, held: &mut HashSet<(u32, bool)>) -> Vec<(u32, bool)> {
    list.iter()
        .copied()
        .filter(|&(code, down)| if down { held.insert((code, button)) } else { held.remove(&(code, button)) })
        .collect()
}

fn send(conv: &EiEventConverter, em: &Emulated, f: Pending, held: &mut HashSet<(u32, bool)>) {
    let (serial, time) = (conv.connection().serial(), now_us());
    let mut touched: Vec<&reis::event::Device> = Vec::new();
    let mut touch = |d| {
        if !touched.contains(&d) {
            touched.push(d);
        }
    };
    if let Some(d) = em.with(DeviceCapability::Keyboard)
        && let Some(k) = d.interface::<ei::Keyboard>()
    {
        for (code, down) in changes(&f.keys, false, held) {
            k.key(code, if down { ei::keyboard::KeyState::Press } else { ei::keyboard::KeyState::Released });
            touch(d);
        }
    }
    if let Some(d) = em.with(DeviceCapability::Button)
        && let Some(b) = d.interface::<ei::Button>()
    {
        for (code, down) in changes(&f.buttons, true, held) {
            b.button(code, if down { ei::button::ButtonState::Press } else { ei::button::ButtonState::Released });
            touch(d);
        }
    }
    if f.motion != (0.0, 0.0)
        && let Some(d) = em.with(DeviceCapability::Pointer)
        && let Some(p) = d.interface::<ei::Pointer>()
    {
        p.motion_relative(f.motion.0, f.motion.1);
        touch(d);
    }
    if f.scroll != (0, 0)
        && let Some(d) = em.with(DeviceCapability::Scroll)
        && let Some(s) = d.interface::<ei::Scroll>()
    {
        s.scroll_discrete(f.scroll.0, f.scroll.1);
        touch(d);
    }
    for d in touched {
        d.device().frame(serial, time);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake `inputN` directory with these capability bitmaps.
    fn input(root: &Path, name: &str, key: &str, rel: &str, abs: &str) -> PathBuf {
        let dev = root.join(name);
        fs::create_dir_all(dev.join("capabilities")).unwrap();
        for (file, v) in [("key", key), ("rel", rel), ("abs", abs)] {
            fs::write(dev.join("capabilities").join(file), format!("{v}\n")).unwrap();
        }
        dev
    }

    #[test]
    fn keyboards_and_mice_are_passed_on_pads_and_wheels_are_not() {
        let root = std::env::temp_dir().join(format!("plugover-route-kinds-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        // Real bitmaps from 64-bit sysfs.
        let keyboard = input(&root, "kbd", "1000000000007 ff9f207ac14057ff febeffdfffefffff fffffffffffffffe", "0", "0");
        let mouse = input(&root, "mouse", "1f0000 0 0 0 0", "1943", "0");
        // A G29: buttons from BTN_TRIGGER up, and axes.
        let wheel = input(&root, "wheel", "ffff00000000 0 0 0 0", "0", "30027");
        // An Xbox pad: KEY_RECORD (167) for its share button, gamepad buttons and sticks.
        let xbox = input(&root, "xbox", "7cdb000000000000 0 8000000000 0 0", "0", "3003f");
        // Pad buttons with no axes, like some arcade sticks.
        let stick = input(&root, "stick", "ffff00000000 0 0 0 0", "0", "0");
        // A keyboard's media node: volume keys and ABS_VOLUME.
        let media = input(&root, "media", "3e000b00000000 0 0 1c0000000000", "0", "100000000");
        assert_eq!(classify(&keyboard), Some(Kind { keyboard: true, pointer: false }));
        assert_eq!(classify(&mouse), Some(Kind { keyboard: false, pointer: true }));
        assert_eq!(classify(&wheel), None);
        assert_eq!(classify(&xbox), None);
        assert_eq!(classify(&stick), None);
        assert_eq!(classify(&media), Some(Kind { keyboard: true, pointer: false }));
        assert_eq!(classify(&root.join("missing")), None);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn finds_the_nodes_of_the_device_on_a_port() {
        use std::os::unix::fs::symlink;
        let root = std::env::temp_dir().join(format!("plugover-route-nodes-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let sys = root.join("sys");
        let vhci = sys.join("devices/platform/vhci_hcd.0");
        fs::create_dir_all(&vhci).unwrap();
        fs::write(
            vhci.join("status"),
            "hub port sta spd dev      sockfd local_busid\n\
             hs  0000 006 002 00030002 000003 3-1\n\
             hs  0001 004 000 00000000 000000 0-0\n",
        )
        .unwrap();
        let class = sys.join("class/input");
        fs::create_dir_all(&class).unwrap();
        let node = |n: u32, parent: PathBuf, key: &str, rel: &str| {
            let dev = input(&parent, &format!("input{n}"), key, rel, "0");
            let event = dev.join(format!("event{n}"));
            fs::create_dir_all(&event).unwrap();
            symlink(&dev, event.join("device")).unwrap();
            symlink(&event, class.join(format!("event{n}"))).unwrap();
            dev
        };
        let hid = vhci.join("usb3/3-1/3-1:1.0/0003:046D:C52B.0001/input");
        node(7, hid.clone(), "fffffffffffffffe", "0");
        node(8, hid, "1f0000 0 0 0 0", "1943");
        // The same kind of device, plugged in here for real, on a busid that looks alike.
        node(9, sys.join("devices/pci0000:00/usb1/1-3-1/1-3-1:1.0/input"), "fffffffffffffffe", "0");
        // A Bluetooth keyboard recreated through uhid.
        let bt = node(10, sys.join("devices/virtual/misc/uhid/0005:046D:B342.0002/input"), "fffffffffffffffe", "0");
        fs::write(bt.join("uniq"), "8c:41:f2:97:4f:28\n").unwrap();

        let found = nodes(&sys, "1-2", 0);
        assert_eq!(found.iter().map(|(n, _)| n.to_string_lossy().into_owned()).collect::<Vec<_>>(), ["/dev/input/event7", "/dev/input/event8"]);
        assert!(found[1].1.pointer);
        assert!(nodes(&sys, "1-2", 1).is_empty(), "a free port has no device");
        let bt = nodes(&sys, "bt-8c41f2974f28", 0);
        assert_eq!(bt.len(), 1);
        assert_eq!(bt[0].0, Path::new("/dev/input/event10"));
        assert!(nodes(&sys, "bt-000000000000", 0).is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn finds_gamescope_input_sockets_newest_first() {
        use std::os::unix::net::UnixListener;
        let dir = std::env::temp_dir().join(format!("plugover-route-screens-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        // A running gamescope holds the lock next to its socket.
        let _old = UnixListener::bind(dir.join("gamescope-0-ei")).unwrap();
        let lock = File::create(dir.join("gamescope-0-ei.lock")).unwrap();
        // SAFETY: flock on a file this test owns.
        assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }, 0);
        thread::sleep(Duration::from_millis(20));
        // Without a lock file it cannot be told from a live one.
        let _new = UnixListener::bind(dir.join("gamescope-1-ei")).unwrap();
        thread::sleep(Duration::from_millis(20));
        // A crashed one leaves its socket and an unheld lock file.
        let _dead = UnixListener::bind(dir.join("gamescope-2-ei")).unwrap();
        fs::write(dir.join("gamescope-2-ei.lock"), "").unwrap();
        let _wayland = UnixListener::bind(dir.join("gamescope-0")).unwrap();
        let _kwin = UnixListener::bind(dir.join("kwin-eis-0")).unwrap();
        assert_eq!(screens(&dir), [dir.join("gamescope-1-ei"), dir.join("gamescope-0-ei")]);

        // punktfunk names the older one as its stream's screen: that one first. A relay naming a
        // dead gamescope, or a symlinked one, changes nothing.
        thread::sleep(Duration::from_millis(20));
        fs::write(dir.join("punktfunk-gamescope-ab12cd-ei"), "gamescope-0-ei\n1920x1080\n").unwrap();
        fs::write(dir.join("punktfunk-gamescope-ei"), "gamescope-2-ei").unwrap();
        std::os::unix::fs::symlink(dir.join("gamescope-1-ei"), dir.join("punktfunk-gamescope-x-ei")).unwrap();
        assert_eq!(screens(&dir), [dir.join("gamescope-0-ei"), dir.join("gamescope-1-ei")]);
        assert!(screens(&dir.join("missing")).is_empty());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn reads_input_events_and_drops_releases_never_pressed() {
        let mut raw = Vec::new();
        for (t, c, v) in [(EV_KEY, 30u16, 1i32), (EV_SYN, SYN_REPORT, 0)] {
            raw.extend([0u8; EVENT - 8]);
            raw.extend(t.to_ne_bytes());
            raw.extend(c.to_ne_bytes());
            raw.extend(v.to_ne_bytes());
        }
        assert_eq!(parse(&raw).collect::<Vec<_>>(), [(EV_KEY, 30, 1), (EV_SYN, SYN_REPORT, 0)]);
        let mut held = HashSet::new();
        assert_eq!(changes(&[(30, true), (31, false)], false, &mut held), [(30, true)]);
        assert_eq!(changes(&[(30, true)], false, &mut held), [], "already down");
        assert_eq!(changes(&[(30, false)], true, &mut held), [], "a mouse button 30 was never pressed");
        assert_eq!(changes(&[(30, false)], false, &mut held), [(30, false)]);
        assert!(held.is_empty());
    }

    /// Against a running gamescope: `PLUGOVER_ROUTE_NODE=/dev/input/eventN cargo test live -- --ignored`
    /// passes that node into the newest gamescope for `PLUGOVER_ROUTE_SECONDS` (default 5).
    #[test]
    #[ignore]
    fn live() {
        let node = PathBuf::from(std::env::var("PLUGOVER_ROUTE_NODE").expect("PLUGOVER_ROUTE_NODE"));
        let secs: u64 = std::env::var("PLUGOVER_ROUTE_SECONDS").ok().and_then(|s| s.parse().ok()).unwrap_or(5);
        let kind = classify(&fs::canonicalize(node_sysfs(&node)).unwrap()).expect("a keyboard or mouse");
        let f = Forward::start(node, kind, screens(&runtime_dir()));
        // Up to LET_GO while a key is held, then it is taken anyway.
        let until = Instant::now() + LET_GO + Duration::from_secs(1);
        while f.screen().is_none() && !f.finished() && Instant::now() < until {
            thread::sleep(TICK);
        }
        let screen = f.screen();
        assert!(screen.is_some(), "not passed on: {:?}", if f.finished() { f.stop() } else { None });
        eprintln!("passing into {}", screen.unwrap());
        thread::sleep(Duration::from_secs(secs));
        assert_eq!(f.stop(), None);
    }
}
