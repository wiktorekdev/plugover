use crate::{sysfs::UsbDevice, usbip::ExportedDevice};
use socket2::{SockRef, TcpKeepalive};
use std::{
    io::{self, Read, Write},
    net::{Shutdown, TcpStream},
    time::Duration,
};

pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
/// How long unanswered data may wait before the connection counts as dead.
#[cfg(target_os = "linux")]
const DEAD_LINK: Duration = Duration::from_secs(5);

/// Prepares a socket for the kernel: no Nagle delay, quick dead-peer detection, and no read
/// timeout (the kernel honours SO_RCVTIMEO and would drop an idle wheel).
pub fn tune(s: &TcpStream) -> io::Result<()> {
    s.set_nodelay(true)?;
    // DSCP "expedited forwarding": Wi-Fi access points that honour it (WMM) send these packets
    // ahead of downloads. Some systems refuse it, which costs nothing.
    let _ = SockRef::from(s).set_tos_v4(0xb8);
    s.set_read_timeout(None)?;
    let keepalive = TcpKeepalive::new().with_time(Duration::from_secs(3)).with_interval(Duration::from_secs(1));
    #[cfg(target_os = "linux")]
    let keepalive = keepalive.with_retries(3);
    // Keepalive only notices a dead link while nothing is in flight; with a wheel sending input,
    // Linux would retransmit for up to 15 minutes before giving up on it.
    #[cfg(target_os = "linux")]
    SockRef::from(s).set_tcp_user_timeout(Some(DEAD_LINK))?;
    SockRef::from(s).set_tcp_keepalive(&keepalive)
}

/// Encrypts the connection in the kernel (kTLS), with the records `tls` describes: every byte
/// after this, including what the kernel's USB/IP code sends and receives on it, is encrypted, and
/// a tampered record drops the connection. False if this kernel cannot (no tls module); the
/// connection is then untouched and `tls::wrap` has to do it.
#[cfg(target_os = "linux")]
pub fn encrypt(s: &TcpStream, key: &crate::auth::Key, nonce: &[u8], relay: bool) -> io::Result<bool> {
    use std::os::fd::AsRawFd;
    const SOL_TLS: libc::c_int = 282;
    const TLS_TX: libc::c_int = 1;
    const TLS_RX: libc::c_int = 2;
    const TCP_ULP: libc::c_int = 31;
    const TLS_1_3: u16 = 0x0304;
    const AES_GCM_128: u16 = 51;
    /// `struct tls12_crypto_info_aes_gcm_128` from linux/tls.h.
    #[repr(C)]
    struct Info {
        version: u16,
        cipher: u16,
        iv: [u8; 8],
        key: [u8; 16],
        salt: [u8; 4],
        seq: [u8; 8],
    }
    let info = |k: crate::tls::Keys| Info {
        version: TLS_1_3,
        cipher: AES_GCM_128,
        key: k.key,
        salt: k.iv[..4].try_into().unwrap(),
        iv: k.iv[4..].try_into().unwrap(),
        seq: [0; 8],
    };
    let set = |level, name, value: *const libc::c_void, len: usize| {
        // SAFETY: `value` points at `len` initialized bytes that outlive the call.
        let r = unsafe { libc::setsockopt(s.as_raw_fd(), level, name, value, len as libc::socklen_t) };
        if r == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
    };
    if set(libc::IPPROTO_TCP, TCP_ULP, b"tls".as_ptr().cast(), 3).is_err() {
        return Ok(false);
    }
    let (tx, rx) = crate::tls::keys(key, nonce, relay);
    for (name, k) in [(TLS_TX, tx), (TLS_RX, rx)] {
        let i = info(k);
        set(SOL_TLS, name, (&raw const i).cast(), size_of::<Info>())
            .map_err(|e| io::Error::new(e.kind(), format!("the kernel could not encrypt the connection: {e}")))?;
    }
    Ok(true)
}

/// Whether this kernel encrypts connections itself, for `doctor`.
#[cfg(target_os = "linux")]
pub fn kernel_encrypts() -> bool {
    loopback().is_ok_and(|(a, _)| encrypt(&a, &[0; 32], &[0; 32], false).unwrap_or(false))
}

/// Two ends of a loopback connection, for a driver that needs a TCP connection of its own while
/// Plugover encrypts the real one.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn loopback() -> io::Result<(TcpStream, TcpStream)> {
    let l = std::net::TcpListener::bind("127.0.0.1:0")?;
    let a = TcpStream::connect(l.local_addr()?)?;
    loop {
        let (b, from) = l.accept()?;
        // Anything else on this machine could have connected first.
        if from == a.local_addr()? {
            a.set_nodelay(true)?;
            b.set_nodelay(true)?;
            return Ok((a, b));
        }
    }
}

/// The local end `pump` copies to and from.
pub trait Local: Read + Write + Send + Sized + 'static {
    fn dup(&self) -> io::Result<Self>;
    fn close(&self);
}

impl Local for TcpStream {
    fn dup(&self) -> io::Result<Self> {
        self.try_clone()
    }
    fn close(&self) {
        let _ = self.shutdown(Shutdown::Both);
    }
}

#[cfg(unix)]
impl Local for std::os::unix::net::UnixStream {
    fn dup(&self) -> io::Result<Self> {
        self.try_clone()
    }
    fn close(&self) {
        let _ = self.shutdown(Shutdown::Both);
    }
}

/// Gives the kernel one end of a local socket through `give` (vhci or usbip-host takes its
/// descriptor) and returns the other end, for a connection the kernel cannot encrypt. A Unix
/// socket saves a TCP stack on every packet; a loopback connection is the fallback.
#[cfg(target_os = "linux")]
pub fn hand_over(mut give: impl FnMut(std::os::fd::RawFd) -> io::Result<()>) -> io::Result<Handed> {
    use std::os::fd::AsRawFd;
    let (kernel, ours) = std::os::unix::net::UnixStream::pair()?;
    if give(kernel.as_raw_fd()).is_ok() {
        return Ok(Handed::Unix(ours));
    }
    let (kernel, ours) = loopback()?;
    give(kernel.as_raw_fd())?;
    Ok(Handed::Tcp(ours))
}

#[cfg(target_os = "linux")]
pub enum Handed {
    Unix(std::os::unix::net::UnixStream),
    Tcp(TcpStream),
}

#[cfg(target_os = "linux")]
impl Handed {
    /// `pump` between the kernel's socket and the remote connection.
    pub fn pump(self, remote: TcpStream, from_remote: impl Read + Send + 'static, to_remote: impl Write) -> io::Result<()> {
        match self {
            Handed::Unix(l) => pump(l, remote, from_remote, to_remote),
            Handed::Tcp(l) => pump(l, remote, from_remote, to_remote),
        }
    }
}

/// Copies between a local connection and the remote one, through `from_remote` and `to_remote`
/// (the remote connection itself, or `tls::wrap` of it), until either side hangs up.
pub fn pump(local: impl Local, remote: TcpStream, mut from_remote: impl Read + Send + 'static, mut to_remote: impl Write) -> io::Result<()> {
    let (mut local_r, mut local_w) = (local.dup()?, local);
    let back = std::thread::spawn(move || {
        let _ = io::copy(&mut from_remote, &mut local_w);
        local_w.close();
    });
    let _ = io::copy(&mut local_r, &mut to_remote);
    let _ = remote.shutdown(Shutdown::Both);
    let _ = back.join();
    Ok(())
}

pub fn exported(d: &UsbDevice) -> ExportedDevice {
    ExportedDevice {
        path: d.path.clone(),
        busid: d.busid.clone(),
        busnum: d.busnum,
        devnum: d.devnum,
        speed: d.speed,
        vid: d.vid,
        pid: d.pid,
        bcd_device: d.bcd_device,
        class: d.class,
        subclass: d.subclass,
        protocol: d.protocol,
        config_value: d.config_value,
        num_configs: d.num_configs,
        num_interfaces: d.interfaces.len() as u8,
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use crate::tls;

    /// Needs the kernel's tls module loaded; says so and passes when it is not.
    #[test]
    fn kernel_and_plugover_encrypt_alike() {
        let (key, nonce) = ([7u8; 32], [9u8; 32]);
        let (host, relay) = loopback().unwrap();
        if !encrypt(&relay, &key, &nonce, true).unwrap() {
            return eprintln!("skipped: no tls module");
        }
        // The host encrypts here, the relay in the kernel, and back.
        let (mut r, mut w) = tls::wrap(host.try_clone().unwrap(), host.try_clone().unwrap(), &key, &nonce, false);
        w.write_all(b"submit urb").unwrap();
        let mut got = [0u8; 10];
        (&relay).read_exact(&mut got).unwrap();
        assert_eq!(&got, b"submit urb");
        (&relay).write_all(b"urb done").unwrap();
        let mut got = [0u8; 8];
        r.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"urb done");

        // The kernel refuses a record sent with another key.
        let (host, relay) = loopback().unwrap();
        assert!(encrypt(&relay, &key, &nonce, true).unwrap());
        tls::wrap(io::empty(), &host, &[8u8; 32], &nonce, false).1.write_all(b"submit urb").unwrap();
        assert!((&relay).read_exact(&mut [0u8; 10]).is_err());
        assert!(kernel_encrypts());
    }
}

/// Round trips of a 64-byte message, like a URB and its answer, through each way Plugover can
/// carry USB traffic. `cargo test --release bench -- --ignored --nocapture`
#[cfg(all(test, target_os = "linux"))]
mod bench {
    use super::*;
    use crate::tls;
    use std::{os::unix::net::UnixStream, time::Instant};

    const ROUNDS: usize = 20_000;

    trait Stream: Read + Write + Send {}
    impl<T: Read + Write + Send> Stream for T {}

    fn report(what: &str, mut a: Box<dyn Stream>, mut b: Box<dyn Stream + 'static>) {
        let echo = std::thread::spawn(move || {
            let mut m = [0u8; 64];
            while b.read_exact(&mut m).is_ok() && b.write_all(&m).is_ok() {}
        });
        let mut m = [7u8; 64];
        let mut times: Vec<u128> = (0..ROUNDS)
            .map(|_| {
                let t = Instant::now();
                a.write_all(&m).unwrap();
                a.read_exact(&mut m).unwrap();
                t.elapsed().as_nanos()
            })
            .collect();
        drop(a);
        let _ = echo.join();
        times.sort_unstable();
        let us = |q: f64| times[((times.len() - 1) as f64 * q) as usize] as f64 / 1000.0;
        eprintln!("{what:<34} median {:6.1} µs   p99 {:6.1} µs", us(0.5), us(0.99));
    }

    /// The application end of a connection that Plugover encrypts on both sides, as on a host
    /// and a relay without kernel TLS.
    fn through_plugover(kernel_relay: bool) -> Option<(Box<dyn Stream>, Box<dyn Stream>)> {
        let (key, nonce) = ([7u8; 32], [9u8; 32]);
        let (host_net, relay_net) = loopback().unwrap();
        let (host_app, host_ours) = UnixStream::pair().unwrap();
        let (hr, hw) = tls::wrap(host_net.try_clone().unwrap(), host_net.try_clone().unwrap(), &key, &nonce, false);
        std::thread::spawn(move || pump(host_ours, host_net, hr, hw));
        if kernel_relay {
            return encrypt(&relay_net, &key, &nonce, true).unwrap().then_some((Box::new(host_app), Box::new(relay_net)));
        }
        let (relay_app, relay_ours) = UnixStream::pair().unwrap();
        let (rr, rw) = tls::wrap(relay_net.try_clone().unwrap(), relay_net.try_clone().unwrap(), &key, &nonce, true);
        std::thread::spawn(move || pump(relay_ours, relay_net, rr, rw));
        Some((Box::new(host_app), Box::new(relay_app)))
    }

    #[test]
    #[ignore]
    fn bench() {
        let (a, b) = loopback().unwrap();
        report("plain", Box::new(a), Box::new(b));
        let (a, b) = loopback().unwrap();
        if encrypt(&a, &[7; 32], &[9; 32], false).unwrap() && encrypt(&b, &[7; 32], &[9; 32], true).unwrap() {
            report("kernel on both sides", Box::new(a), Box::new(b));
        }
        if let Some((a, b)) = through_plugover(true) {
            report("Plugover on the host, kernel relay", a, b);
        }
        let (a, b) = through_plugover(false).unwrap();
        report("Plugover on both sides", a, b);
    }
}
