//! The relay on Windows. usbipd-win serves the devices; Plugover runs its challenge, then
//! connects to usbipd on loopback and only copies bytes, like the Windows host does with
//! usbip-win2. usbipd has no authentication of its own, so the installer blocks its port from
//! the network.

use crate::sysfs::{Interface, UsbDevice};
use anyhow::{Context, Result};
use serde::Deserialize;

#[cfg_attr(not(windows), allow(dead_code))]
pub const PORT: u16 = 3240;

#[derive(Deserialize)]
struct State {
    #[serde(rename = "Devices")]
    devices: Vec<Entry>,
}

#[derive(Deserialize)]
struct Entry {
    #[serde(rename = "BusId")]
    bus_id: Option<String>,
    #[serde(rename = "ClientIPAddress")]
    client: Option<String>,
    #[serde(rename = "Description")]
    description: String,
    #[serde(rename = "InstanceId")]
    instance_id: String,
    #[serde(rename = "PersistedGuid")]
    persisted_guid: Option<String>,
}

/// A device plugged into the relay. Linux fills in only `dev`.
pub struct Device {
    pub dev: UsbDevice,
    /// Some USB/IP client has it right now (us, or something else such as WSL).
    pub attached: bool,
    /// Shared in usbipd; we only unbind what we bound.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub bound: bool,
}

/// `USB\VID_046D&PID_C24F\5&2A...` to (0x046d, 0xc24f).
fn vid_pid(instance_id: &str) -> Option<(u16, u16)> {
    let up = instance_id.to_ascii_uppercase();
    let hex_after = |tag: &str| u16::from_str_radix(up.split(tag).nth(1)?.get(..4)?, 16).ok();
    Some((hex_after("VID_")?, hex_after("PID_")?))
}

/// Devices plugged in right now, from `usbipd state`.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn parse(json: &str) -> Result<Vec<Device>> {
    let state: State = serde_json::from_str(json).context("unexpected `usbipd state` output")?;
    Ok(state
        .devices
        .into_iter()
        .filter_map(|e| {
            // Unplugged devices have no bus id; devices behind odd hubs get "IncompatibleHub".
            let busid = e.bus_id.filter(|b| b.contains('-'))?;
            let (vid, pid) = vid_pid(&e.instance_id)?;
            let (busnum, devnum) = busid.split_once('-').and_then(|(b, p)| Some((b.parse().ok()?, p.parse().ok()?)))?;
            // ponytail: usbipd reports no USB classes, so hubs and storage are recognised by the
            // Windows device names; read the class through SetupAPI if a device slips through.
            let d = e.description.to_lowercase();
            let class = if d.contains("usb hub") { 0x09 } else { 0 };
            let storage = d.contains("mass storage") || d.contains("disk drive") || d.contains("uasp");
            let interfaces = if storage { vec![Interface { class: 0x08, subclass: 0, protocol: 0, hid: None }] } else { Vec::new() };
            Some(Device {
                attached: e.client.is_some(),
                bound: e.persisted_guid.is_some(),
                dev: UsbDevice {
                    path: String::new(),
                    busnum,
                    devnum,
                    speed: 0,
                    vid,
                    pid,
                    bcd_device: 0,
                    class,
                    subclass: 0,
                    protocol: 0,
                    config_value: 0,
                    num_configs: 0,
                    manufacturer: None,
                    product: Some(e.description),
                    driver: None,
                    interfaces,
                    busid,
                },
            })
        })
        .collect())
}

#[cfg(windows)]
mod imp {
    use super::*;
    use crate::{net, usbip};
    use anyhow::bail;
    use std::{
        env, io,
        net::{Shutdown, TcpStream},
        os::windows::process::CommandExt,
        path::PathBuf,
        process::Command,
        thread,
    };

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    fn exe() -> PathBuf {
        if let Some(p) = env::var_os("PLUGOVER_USBIPD") {
            return p.into();
        }
        let base = env::var_os("ProgramFiles").map(PathBuf::from).unwrap_or_else(|| r"C:\Program Files".into());
        base.join("usbipd-win").join("usbipd.exe")
    }

    fn run(args: &[&str]) -> Result<String> {
        let exe = exe();
        let out = Command::new(&exe)
            .args(args)
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .with_context(|| format!("running {} (is usbipd-win installed?)", exe.display()))?;
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        if !out.status.success() {
            bail!("usbipd {}: {}{}", args.join(" "), text.trim(), String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(text)
    }

    pub fn list() -> Result<Vec<Device>> {
        parse(&run(&["state"])?)
    }

    /// Serves one import that already passed Plugover's challenge, until either side hangs up.
    pub fn lend(mut host: TcpStream, dev: &Device) -> Result<()> {
        let busid = dev.dev.busid.as_str();
        if !dev.bound {
            run(&["bind", "--busid", busid])?;
        }
        let result = (|| -> Result<()> {
            let mut up = TcpStream::connect(("127.0.0.1", PORT)).context("usbipd is not running")?;
            io::Write::write_all(&mut up, &usbip::import_request(busid))?;
            net::tune(&up)?;
            net::tune(&host)?;
            let (mut host_w, mut up_r) = (host.try_clone()?, up.try_clone()?);
            let back = thread::spawn(move || {
                let _ = io::copy(&mut up_r, &mut host_w);
                let _ = host_w.shutdown(Shutdown::Both);
            });
            let _ = io::copy(&mut host, &mut up);
            let _ = up.shutdown(Shutdown::Both);
            let _ = back.join();
            Ok(())
        })();
        if !dev.bound {
            // Gives the device back to Windows the way it was.
            if let Err(e) = run(&["unbind", "--busid", busid]) {
                eprintln!("could not unbind {busid}: {e:#}");
            }
        }
        result
    }
}

#[cfg(windows)]
pub use imp::{lend, list};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_usbipd_state() {
        let json = r#"{
          "Devices": [
            { "BusId": "1-2", "ClientIPAddress": null, "Description": "Logitech G HUB G29 Driving Force Racing Wheel USB, USB Input Device",
              "InstanceId": "USB\\VID_046D&PID_C24F\\5&2A3B4C&0&2", "IsForced": false, "PersistedGuid": null, "StubInstanceId": null },
            { "BusId": "1-3", "ClientIPAddress": "127.0.0.1", "Description": "USB Mass Storage Device",
              "InstanceId": "USB\\VID_0BC2&PID_AB24\\NA8B", "IsForced": false, "PersistedGuid": "7d3c...", "StubInstanceId": null },
            { "BusId": null, "ClientIPAddress": null, "Description": "Gone",
              "InstanceId": "USB\\VID_1234&PID_0001\\X", "IsForced": false, "PersistedGuid": null, "StubInstanceId": null },
            { "BusId": "IncompatibleHub", "ClientIPAddress": null, "Description": "Behind a hub",
              "InstanceId": "USB\\VID_1234&PID_0002\\Y", "IsForced": false, "PersistedGuid": null, "StubInstanceId": null }
          ]
        }"#;
        let devs = parse(json).unwrap();
        assert_eq!(devs.len(), 2, "unplugged and incompatible devices are skipped");
        assert_eq!((devs[0].dev.busid.as_str(), devs[0].dev.vid, devs[0].dev.pid), ("1-2", 0x046d, 0xc24f));
        assert!(!devs[0].attached && !devs[0].bound);
        assert!(devs[1].attached && devs[1].bound);
        assert_eq!(crate::catalog::blocked_reason(&devs[1].dev), Some("storage device"));
        // "G HUB" is Logitech's software, not a USB hub; only a hub's own name should match.
        assert_eq!(crate::catalog::blocked_reason(&devs[0].dev), None);
    }
}
