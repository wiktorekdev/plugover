//! Linux USB through sysfs: listing devices, lending them out with usbip-host (relay side)
//! and plugging them in with vhci_hcd (host side). Every function takes the sysfs root so
//! tests can point it at a fake tree.

use std::{fs, io, path::Path};
#[cfg(unix)]
use std::os::fd::RawFd;

#[derive(Debug, Clone)]
pub struct Interface {
    pub class: u8,
    pub subclass: u8,
    pub protocol: u8,
    /// HID report descriptor, present only while the interface is bound to usbhid.
    pub hid: Option<Vec<u8>>,
}

#[derive(Debug, Clone)]
pub struct UsbDevice {
    pub busid: String,
    pub path: String,
    pub busnum: u32,
    pub devnum: u32,
    pub speed: u32,
    pub vid: u16,
    pub pid: u16,
    pub bcd_device: u16,
    pub class: u8,
    pub subclass: u8,
    pub protocol: u8,
    pub config_value: u8,
    pub num_configs: u8,
    pub manufacturer: Option<String>,
    pub product: Option<String>,
    pub driver: Option<String>,
    pub interfaces: Vec<Interface>,
}

fn read(dir: &Path, file: &str) -> Option<String> {
    let s = fs::read_to_string(dir.join(file)).ok()?;
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_string())
}

fn hex<T: TryFrom<u32>>(dir: &Path, file: &str) -> Option<T> {
    T::try_from(u32::from_str_radix(&read(dir, file)?, 16).ok()?).ok()
}

fn dec<T: std::str::FromStr>(dir: &Path, file: &str) -> Option<T> {
    read(dir, file)?.parse().ok()
}

/// sysfs `speed` (Mbit/s) to the kernel's `enum usb_device_speed`, which USB/IP carries.
fn speed_code(s: &str) -> u32 {
    match s {
        "1.5" => 1,
        "12" => 2,
        "480" => 3,
        "53.3-480" => 4,
        "5000" => 5,
        "10000" | "20000" => 6,
        _ => 0,
    }
}

fn driver_name(dir: &Path) -> Option<String> {
    Some(fs::read_link(dir.join("driver")).ok()?.file_name()?.to_string_lossy().into_owned())
}

pub fn list(sys: &Path) -> io::Result<Vec<UsbDevice>> {
    let dir = sys.join("bus/usb/devices");
    let mut out = Vec::new();
    for entry in fs::read_dir(&dir)? {
        let name = entry?.file_name().to_string_lossy().into_owned();
        // `1-2:1.0` is an interface and `usb1` a root hub; neither can be lent out.
        if name.contains(':') || name.starts_with("usb") {
            continue;
        }
        if let Some(d) = read_device(&dir, &name) {
            out.push(d);
        }
    }
    out.sort_by(|a, b| a.busid.cmp(&b.busid));
    Ok(out)
}

fn read_device(devices: &Path, busid: &str) -> Option<UsbDevice> {
    let d = devices.join(busid);
    let prefix = format!("{busid}:");
    let mut ifaces: Vec<_> = fs::read_dir(&d)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with(&prefix))
        .collect();
    ifaces.sort();
    Some(UsbDevice {
        busid: busid.to_string(),
        path: fs::canonicalize(&d).unwrap_or(d.clone()).to_string_lossy().into_owned(),
        busnum: dec(&d, "busnum")?,
        devnum: dec(&d, "devnum")?,
        speed: speed_code(&read(&d, "speed").unwrap_or_default()),
        vid: hex(&d, "idVendor")?,
        pid: hex(&d, "idProduct")?,
        bcd_device: hex(&d, "bcdDevice").unwrap_or(0),
        class: hex(&d, "bDeviceClass").unwrap_or(0),
        subclass: hex(&d, "bDeviceSubClass").unwrap_or(0),
        protocol: hex(&d, "bDeviceProtocol").unwrap_or(0),
        config_value: dec(&d, "bConfigurationValue").unwrap_or(0),
        num_configs: dec(&d, "bNumConfigurations").unwrap_or(0),
        manufacturer: read(&d, "manufacturer"),
        product: read(&d, "product"),
        driver: driver_name(&d),
        interfaces: ifaces.iter().map(|n| read_interface(&d.join(n))).collect(),
    })
}

fn read_interface(i: &Path) -> Interface {
    // The HID node is a child named like `0003:046D:C24F.0001`.
    let hid = fs::read_dir(i)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .find_map(|e| fs::read(e.path().join("report_descriptor")).ok());
    Interface {
        class: hex(i, "bInterfaceClass").unwrap_or(0),
        subclass: hex(i, "bInterfaceSubClass").unwrap_or(0),
        protocol: hex(i, "bInterfaceProtocol").unwrap_or(0),
        hid,
    }
}

// Relay side: usbip-host. Needs root.

fn write(path: &Path, value: &str) -> io::Result<()> {
    fs::write(path, value).map_err(|e| io::Error::new(e.kind(), format!("writing {}: {e}", path.display())))
}

/// Detach the device from its local driver and give it to usbip-host.
pub fn export_bind(sys: &Path, busid: &str) -> io::Result<()> {
    let dev = sys.join("bus/usb/devices").join(busid);
    let stub = sys.join("bus/usb/drivers/usbip-host");
    match driver_name(&dev).as_deref() {
        Some("usbip-host") => return Ok(()),
        Some(_) => write(&dev.join("driver/unbind"), busid)?,
        None => {}
    }
    write(&stub.join("match_busid"), &format!("add {busid}"))?;
    write(&stub.join("bind"), busid)
}

/// Hand the connected socket to the kernel; from here on USB traffic never leaves it.
#[cfg(unix)]
pub fn export_socket(sys: &Path, busid: &str, fd: RawFd) -> io::Result<()> {
    write(&sys.join("bus/usb/devices").join(busid).join("usbip_sockfd"), &fd.to_string())
}

/// usbip-host state: 1 available, 2 in use, 3 error. `None` once the device is gone.
pub fn export_status(sys: &Path, busid: &str) -> Option<u8> {
    dec(&sys.join("bus/usb/devices").join(busid), "usbip_status")
}

/// Give the device back to its normal local driver.
pub fn export_release(sys: &Path, busid: &str) -> io::Result<()> {
    let stub = sys.join("bus/usb/drivers/usbip-host");
    if driver_name(&sys.join("bus/usb/devices").join(busid)).as_deref() == Some("usbip-host") {
        write(&stub.join("unbind"), busid)?;
    }
    // While the busid is still in usbip-host's match table the generic driver will not take the
    // device, which leaves it with no driver at all. `rebind` re-probes it and removes the entry
    // itself, so it must come before any `del`.
    write(&stub.join("rebind"), busid)
}

/// busids usbip-host would still claim. A relay that died mid-lend leaves these behind, and the
/// device stays driverless until they are released.
pub fn export_claimed(sys: &Path) -> Vec<String> {
    read(&sys.join("bus/usb/drivers/usbip-host"), "match_busid")
        .map(|s| s.split_whitespace().map(str::to_string).collect())
        .unwrap_or_default()
}

// Host side: vhci_hcd. punktfunk-host's udev rule makes attach/detach writable for the
// `punktfunk` group, so this works without root there.

// ponytail: only the first vhci controller (8 ports per speed); add vhci_hcd.N if that fills up.
const VHCI: &str = "devices/platform/vhci_hcd.0";
const VDEV_ST_NULL: u32 = 4;

fn free_port(status: &str, speed: u32) -> Option<u32> {
    let hub = if speed >= 5 { "ss" } else { "hs" };
    status.lines().skip(1).find_map(|l| {
        let f: Vec<&str> = l.split_whitespace().collect();
        if f.len() >= 3 && f[0] == hub && f[2].parse() == Ok(VDEV_ST_NULL) {
            f[1].parse().ok()
        } else {
            None
        }
    })
}

/// Plugs the device in and returns the vhci port it landed on.
#[cfg(unix)]
pub fn vhci_attach(sys: &Path, fd: RawFd, devid: u32, speed: u32) -> io::Result<u32> {
    let vhci = sys.join(VHCI);
    let status = fs::read_to_string(vhci.join("status"))
        .map_err(|e| io::Error::new(e.kind(), format!("vhci_hcd is not loaded ({e}); run `modprobe vhci-hcd`")))?;
    let port = free_port(&status, speed).ok_or_else(|| io::Error::other("every vhci port is in use"))?;
    write(&vhci.join("attach"), &format!("{port} {fd} {devid} {speed}"))?;
    Ok(port)
}

/// False once the kernel has dropped the device (relay gone, cable pulled).
pub fn vhci_port_used(sys: &Path, port: u32) -> bool {
    let Ok(status) = fs::read_to_string(sys.join(VHCI).join("status")) else { return false };
    status.lines().skip(1).any(|l| {
        let f: Vec<&str> = l.split_whitespace().collect();
        f.len() >= 3 && f[1].parse() == Ok(port) && f[2].parse() != Ok(VDEV_ST_NULL)
    })
}

pub fn vhci_detach(sys: &Path, port: u32) -> io::Result<()> {
    write(&sys.join(VHCI).join("detach"), &port.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fake_sys() -> PathBuf {
        let root = std::env::temp_dir().join(format!("plugover-sysfs-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let dev = root.join("bus/usb/devices/1-2");
        let iface = dev.join("1-2:1.0");
        fs::create_dir_all(iface.join("0003:046D:C24F.0001")).unwrap();
        fs::create_dir_all(root.join("bus/usb/devices/usb1")).unwrap();
        for (f, v) in [
            ("busnum", "1"),
            ("devnum", "5"),
            ("speed", "12"),
            ("idVendor", "046d"),
            ("idProduct", "c24f"),
            ("bDeviceClass", "00"),
            ("product", "G29 Driving Force Racing Wheel\n"),
        ] {
            fs::write(dev.join(f), v).unwrap();
        }
        fs::write(iface.join("bInterfaceClass"), "03").unwrap();
        fs::write(iface.join("0003:046D:C24F.0001/report_descriptor"), [0x05, 0x01]).unwrap();
        root
    }

    #[test]
    fn lists_devices_with_interfaces() {
        let sys = fake_sys();
        let devs = list(&sys).unwrap();
        assert_eq!(devs.len(), 1, "root hubs and interfaces are skipped");
        let d = &devs[0];
        assert_eq!((d.busid.as_str(), d.vid, d.pid, d.speed, d.devnum), ("1-2", 0x046d, 0xc24f, 2, 5));
        assert_eq!(d.product.as_deref(), Some("G29 Driving Force Racing Wheel"));
        assert_eq!(d.interfaces[0].class, 3);
        assert_eq!(d.interfaces[0].hid.as_deref(), Some(&[0x05, 0x01][..]));
        fs::remove_dir_all(sys).unwrap();
    }

    #[test]
    fn picks_a_free_port_on_the_right_hub() {
        let status = "hub port sta spd dev      sockfd local_busid\n\
                      hs  0000 006 002 00010005 000003 1-2\n\
                      hs  0001 004 000 00000000 000000 0-0\n\
                      ss  0008 004 000 00000000 000000 0-0\n";
        assert_eq!(free_port(status, 2), Some(1));
        assert_eq!(free_port(status, 5), Some(8));
        assert_eq!(free_port("hub port sta\nhs  0000 006\n", 3), None);
    }
}
