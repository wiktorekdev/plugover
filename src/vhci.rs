//! Plugging a relay's device into this machine. Linux hands the authenticated socket straight to
//! the kernel's vhci_hcd. On Windows the usbip-win2 driver insists on opening the connection
//! itself, so it connects to a loopback proxy that does Plugover's challenge with the relay and
//! then only copies bytes.

use crate::state::PairedRelay;
use anyhow::Result;
use std::path::Path;

pub struct Attachment {
    /// Virtual hub port, as the OS numbers it.
    pub port: u32,
    #[cfg(windows)]
    alive: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

#[cfg(target_os = "linux")]
mod imp {
    use super::*;
    use crate::{auth, host, net, proto, sysfs, tls, usbip};
    use anyhow::ensure;
    use std::{io::Write, os::fd::AsRawFd, thread};

    pub fn attach(sys: &Path, relay: &PairedRelay, busid: &str, encrypt: bool) -> Result<Attachment> {
        let (id, key) = host::credentials(relay)?;
        let mut s = host::connect(&relay.address)?;
        let nonce = proto::challenge(&mut s, if encrypt { proto::DATA_ENCRYPTED } else { proto::DATA }, &id)?;
        let mut msg = auth::sign(&key, &nonce, busid).to_vec();
        msg.extend(usbip::import_request(busid));
        s.write_all(&msg)?;
        // The relay's reply is the first encrypted record; one that arrives before this waits.
        if !encrypt || net::encrypt(&s, &key, &nonce, false)? {
            let dev = usbip::read_import_reply(&mut s)?;
            ensure!(dev.busid == busid, "the relay answered for {} instead of {busid}", dev.busid);
            net::tune(&s)?;
            // The kernel keeps its own reference; dropping our handle afterwards is fine.
            let port = sysfs::vhci_attach(sys, s.as_raw_fd(), dev.devid(), dev.speed)?;
            return Ok(Attachment { port });
        }
        // This kernel cannot encrypt, so it gets one end of a local socket and Plugover encrypts
        // between the other end and the relay.
        let (mut r, w) = tls::wrap(s.try_clone()?, s.try_clone()?, &key, &nonce, false);
        let dev = usbip::read_import_reply(&mut r)?;
        ensure!(dev.busid == busid, "the relay answered for {} instead of {busid}", dev.busid);
        net::tune(&s)?;
        let mut port = 0;
        let ours = net::hand_over(|fd| sysfs::vhci_attach(sys, fd, dev.devid(), dev.speed).map(|p| port = p))?;
        // Ends when either side hangs up; the kernel then frees the port, which `alive` sees.
        thread::spawn(move || ours.pump(s, r, w));
        Ok(Attachment { port })
    }

    pub fn alive(sys: &Path, a: &Attachment) -> bool {
        sysfs::vhci_port_used(sys, a.port)
    }

    pub fn detach(sys: &Path, a: &Attachment) -> Result<()> {
        Ok(sysfs::vhci_detach(sys, a.port)?)
    }
}

#[cfg(windows)]
mod imp {
    use super::*;
    use crate::{auth, host, net, proto, tls, usbip};
    use anyhow::{Context, bail, ensure};
    use std::{
        env, io,
        net::{TcpListener, TcpStream},
        os::windows::process::CommandExt,
        path::PathBuf,
        process::Command,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        thread,
        time::Duration,
    };

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    /// usbip-win2's command-line tool.
    pub fn usbip_exe() -> PathBuf {
        if let Some(p) = env::var_os("PLUGOVER_USBIP") {
            return p.into();
        }
        let base = env::var_os("ProgramFiles").map(PathBuf::from).unwrap_or_else(|| r"C:\Program Files".into());
        base.join("USBip").join("usbip.exe")
    }

    fn usbip(args: &[&str]) -> Result<String> {
        let exe = usbip_exe();
        let out = Command::new(&exe)
            .args(args)
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .with_context(|| format!("running {} (is usbip-win2 installed?)", exe.display()))?;
        let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        if !out.status.success() {
            bail!("usbip {}: {}", args.join(" "), text.trim());
        }
        Ok(text)
    }

    /// Serves the driver's connection: Plugover's challenge upstream, then a byte pipe that
    /// encrypts towards the relay.
    fn proxy(mut driver: TcpStream, relay: &PairedRelay, busid: &str, encrypt: bool) -> Result<()> {
        driver.set_read_timeout(Some(net::HANDSHAKE_TIMEOUT))?;
        let asked = usbip::read_import_request(&mut driver)?;
        ensure!(asked == busid, "the driver asked for {asked} instead of {busid}");
        let (id, key) = host::credentials(relay)?;
        let mut up = host::connect(&relay.address)?;
        let nonce = proto::challenge(&mut up, if encrypt { proto::DATA_ENCRYPTED } else { proto::DATA }, &id)?;
        let mut msg = auth::sign(&key, &nonce, busid).to_vec();
        msg.extend(usbip::import_request(busid));
        io::Write::write_all(&mut up, &msg)?;
        net::tune(&up)?;
        net::tune(&driver)?;
        if encrypt {
            let (r, w) = tls::wrap(up.try_clone()?, up.try_clone()?, &key, &nonce, false);
            net::pump(driver, up, r, w)?;
        } else {
            net::pump(driver, up.try_clone()?, up.try_clone()?, up)?;
        }
        Ok(())
    }

    pub fn attach(_sys: &Path, relay: &PairedRelay, busid: &str, encrypt: bool) -> Result<Attachment> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let local = listener.local_addr()?.port().to_string();
        let alive = Arc::new(AtomicBool::new(true));
        let (relay2, busid2, alive2) = (relay.clone(), busid.to_string(), alive.clone());
        thread::spawn(move || {
            // `--once` means the driver connects a single time; the listener goes with this thread.
            if let Ok((driver, _)) = listener.accept()
                && let Err(e) = proxy(driver, &relay2, &busid2, encrypt)
            {
                eprintln!("{busid2} from {}: {e:#}", relay2.name);
            }
            alive2.store(false, Ordering::SeqCst);
        });
        let out = usbip(&[
            "--tcp-port",
            &local,
            "attach",
            "-r",
            "127.0.0.1",
            "-b",
            busid,
            "--terse",
            "--once",
            "--receive-mode",
            "low-latency",
        ]);
        let out = match out {
            Ok(o) => o,
            Err(e) => {
                // Unblock the proxy thread waiting in accept().
                let _ = TcpStream::connect_timeout(&format!("127.0.0.1:{local}").parse()?, Duration::from_secs(1));
                return Err(e);
            }
        };
        let port = out
            .lines()
            .rev()
            .find_map(|l| l.trim().parse().ok())
            .with_context(|| format!("usbip attach printed no port: {}", out.trim()))?;
        Ok(Attachment { port, alive })
    }

    pub fn alive(_sys: &Path, a: &Attachment) -> bool {
        a.alive.load(Ordering::SeqCst)
    }

    pub fn detach(_sys: &Path, a: &Attachment) -> Result<()> {
        usbip(&["detach", "-p", &a.port.to_string()]).map(|_| ())
    }
}

pub use imp::{alive, attach, detach};
#[cfg(windows)]
pub use imp::usbip_exe;
