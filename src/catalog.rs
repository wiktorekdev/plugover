//! Turns `046d:c24f` into "Logitech G29 Driving Force, steering wheel": our own catalog first,
//! then the system USB database, then what the device says about itself.

use crate::sysfs::UsbDevice;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, fs, sync::LazyLock};

/// Declaration order is priority order when a device looks like several things.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Wheel,
    Pedals,
    Joystick,
    Gamepad,
    Keyboard,
    Mouse,
    Other,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::Wheel => "Steering wheel",
            Kind::Pedals => "Pedals",
            Kind::Joystick => "Flight stick",
            Kind::Gamepad => "Controller",
            Kind::Keyboard => "Keyboard",
            Kind::Mouse => "Mouse",
            Kind::Other => "USB device",
        }
    }

    fn is_game(self) -> bool {
        self <= Kind::Gamepad
    }
}

/// Where the name came from, so the UI can say how sure it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    Catalog,
    UsbIds,
    Device,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Identity {
    /// What the UI shows: "Logitech G29 Driving Force", without repeating the maker.
    pub title: String,
    pub name: String,
    pub maker: String,
    pub kind: Kind,
    pub source: Source,
    /// Set for devices that must never leave the relay.
    pub blocked: Option<String>,
}

#[derive(Deserialize)]
struct Entry {
    id: String,
    name: String,
    maker: String,
    kind: Kind,
}

#[derive(Deserialize)]
struct CatalogFile {
    device: Vec<Entry>,
}

static CATALOG: LazyLock<HashMap<(u16, u16), Entry>> = LazyLock::new(|| {
    let file: CatalogFile = toml::from_str(include_str!("../data/catalog.toml")).expect("data/catalog.toml is valid");
    file.device
        .into_iter()
        .map(|e| {
            let (v, p) = e.id.split_once(':').expect("catalog ids look like 046d:c24f");
            let id = (u16::from_str_radix(v, 16).unwrap(), u16::from_str_radix(p, 16).unwrap());
            (id, e)
        })
        .collect()
});

/// The usb.ids database that usbutils/hwdata install.
pub struct UsbIds(String);

impl UsbIds {
    pub fn load() -> Option<Self> {
        ["/usr/share/hwdata/usb.ids", "/usr/share/misc/usb.ids", "/var/lib/usbutils/usb.ids"]
            .iter()
            .find_map(|p| fs::read_to_string(p).ok())
            .map(Self)
    }

    /// (vendor, product) names.
    pub fn lookup(&self, vid: u16, pid: u16) -> (Option<&str>, Option<&str>) {
        let (v, p) = (format!("{vid:04x}  "), format!("{pid:04x}  "));
        let mut vendor = None;
        for line in self.0.lines() {
            if line.starts_with('#') || line.is_empty() || line.starts_with("\t\t") {
                continue;
            }
            match (vendor, line.strip_prefix('\t')) {
                (None, None) => vendor = line.strip_prefix(&v),
                (None, Some(_)) => {}
                (Some(_), Some(rest)) => {
                    if let Some(name) = rest.strip_prefix(&p) {
                        return (vendor, Some(name));
                    }
                }
                (Some(_), None) => break,
            }
        }
        (vendor, None)
    }
}

/// "Logitech, Inc." reads better as "Logitech".
fn tidy_vendor(v: &str) -> String {
    let mut v = v.trim();
    for suffix in [", Inc.", " Inc.", ", Ltd", " Co., Ltd", ", Ltd.", " Ltd.", " Corp.", " Corporation", ", LLC", " GmbH"] {
        v = v.strip_suffix(suffix).unwrap_or(v);
    }
    v.to_string()
}

pub fn blocked_reason(dev: &UsbDevice) -> Option<&'static str> {
    if dev.class == 0x09 {
        return Some("USB hub");
    }
    if dev.class == 0x08 || dev.interfaces.iter().any(|i| i.class == 0x08) {
        return Some("storage device");
    }
    None
}

pub fn identify(dev: &UsbDevice, ids: Option<&UsbIds>) -> Identity {
    let blocked = blocked_reason(dev);
    if let Some(e) = CATALOG.get(&(dev.vid, dev.pid)) {
        return Identity::new(e.name.clone(), e.maker.clone(), e.kind, Source::Catalog, blocked);
    }
    let (vendor, product) = ids.map(|i| i.lookup(dev.vid, dev.pid)).unwrap_or_default();
    let (name, source) = match (product, &dev.product) {
        (Some(p), _) => (p.to_string(), Source::UsbIds),
        (None, Some(p)) => (p.clone(), Source::Device),
        (None, None) => (Kind::Other.label().to_string(), Source::Unknown),
    };
    let maker = vendor
        .map(tidy_vendor)
        .or_else(|| dev.manufacturer.clone())
        .unwrap_or_else(|| "Unknown maker".into());
    let kind = classify(dev, &name);
    Identity::new(name, maker, kind, source, blocked)
}

impl Identity {
    fn new(name: String, maker: String, kind: Kind, source: Source, blocked: Option<&'static str>) -> Self {
        let brand = maker.split_whitespace().next().unwrap_or_default();
        let title = if name.to_lowercase().starts_with(&brand.to_lowercase()) { name.clone() } else { format!("{maker} {name}") };
        Self { title, name, maker, kind, source, blocked: blocked.map(str::to_string) }
    }
}

fn classify(dev: &UsbDevice, name: &str) -> Kind {
    let mut game: Option<Kind> = None;
    let mut basic: Option<Kind> = None;
    let mut both = false;
    for i in &dev.interfaces {
        let kind = i.hid.as_deref().and_then(hid_kind).or(match (i.class, i.subclass, i.protocol) {
            (0x03, 0x01, 0x01) => Some(Kind::Keyboard),
            (0x03, 0x01, 0x02) => Some(Kind::Mouse),
            // Xbox 360 and Xbox One pads are vendor-class, not HID.
            (0xff, 0x5d, 0x01) | (0xff, 0x47, 0xd0) => Some(Kind::Gamepad),
            _ => None,
        });
        match kind {
            Some(k) if k.is_game() => game = Some(game.map_or(k, |g| g.min(k))),
            Some(k) => {
                both |= basic.is_some_and(|b| b != k);
                basic.get_or_insert(k);
            }
            None => {}
        }
    }
    // Mouse dongles often expose a keyboard interface first (and vice versa); the name settles it.
    if game.is_none() && both {
        let name = name.to_lowercase();
        if name.contains("mouse") {
            return Kind::Mouse;
        }
        if name.contains("keyboard") {
            return Kind::Keyboard;
        }
    }
    game.or(basic).unwrap_or(Kind::Other)
}

/// Reads the top-level application collections and simulation usages of a HID report descriptor.
pub fn hid_kind(desc: &[u8]) -> Option<Kind> {
    let (mut page, mut depth) = (0u16, 0u32);
    let mut usage: Option<(u16, u16)> = None;
    let mut apps = Vec::new();
    let mut sim = Vec::new();
    let mut i = 0;
    while i < desc.len() {
        let b = desc[i];
        if b == 0xfe {
            // Long item: size byte, tag byte, data.
            i += 3 + *desc.get(i + 1)? as usize;
            continue;
        }
        let n = [0, 1, 2, 4][(b & 3) as usize];
        let data = desc.get(i + 1..i + 1 + n)?.iter().rev().fold(0u32, |a, &x| a << 8 | x as u32);
        i += 1 + n;
        match b & 0xfc {
            0x04 => page = data as u16,
            0x08 => {
                let u = if n == 4 { ((data >> 16) as u16, data as u16) } else { (page, data as u16) };
                if u.0 == 0x02 {
                    sim.push(u.1);
                }
                usage.get_or_insert(u);
            }
            0xa0 => {
                if depth == 0 && data == 1 {
                    apps.extend(usage);
                }
                depth += 1;
                usage = None;
            }
            0xc0 => {
                depth = depth.saturating_sub(1);
                usage = None;
            }
            0x80 | 0x90 | 0xb0 => usage = None,
            _ => {}
        }
    }

    // Simulation Controls page: 0x02 automobile device, 0xc8 steering, 0xc4-0xc6 pedals.
    if sim.contains(&0xc8) || apps.contains(&(0x02, 0x02)) {
        return Some(Kind::Wheel);
    }
    let pedals = sim.iter().any(|u| matches!(u, 0xc4..=0xc6));
    let app = |u| apps.contains(&(0x01, u));
    if pedals {
        return Some(Kind::Pedals);
    }
    if app(0x05) {
        return Some(Kind::Gamepad);
    }
    if app(0x04) || apps.contains(&(0x02, 0x01)) {
        return Some(Kind::Joystick);
    }
    apps.iter().find_map(|&a| match a {
        (0x01, 0x06) => Some(Kind::Keyboard),
        (0x01, 0x02) => Some(Kind::Mouse),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sysfs::Interface;

    // HID spec appendix B.1, boot keyboard (trimmed to the collection header).
    const KEYBOARD: &[u8] = &[0x05, 0x01, 0x09, 0x06, 0xa1, 0x01, 0x05, 0x07, 0x19, 0xe0, 0x29, 0xe7, 0x81, 0x02, 0xc0];
    const GAMEPAD: &[u8] = &[0x05, 0x01, 0x09, 0x05, 0xa1, 0x01, 0x09, 0x30, 0x81, 0x02, 0xc0];
    const WHEEL: &[u8] = &[0x05, 0x01, 0x09, 0x04, 0xa1, 0x01, 0x05, 0x02, 0x09, 0xc8, 0x09, 0xc4, 0x81, 0x02, 0xc0];
    const PEDALS: &[u8] = &[0x05, 0x01, 0x09, 0x04, 0xa1, 0x01, 0x05, 0x02, 0x09, 0xc4, 0x09, 0xc5, 0x81, 0x02, 0xc0];
    // A mouse that also exposes a keyboard collection: the first collection wins.
    const MOUSE_KB: &[u8] = &[0x05, 0x01, 0x09, 0x02, 0xa1, 0x01, 0xc0, 0x05, 0x01, 0x09, 0x06, 0xa1, 0x01, 0xc0];

    #[test]
    fn classifies_hid_descriptors() {
        assert_eq!(hid_kind(KEYBOARD), Some(Kind::Keyboard));
        assert_eq!(hid_kind(GAMEPAD), Some(Kind::Gamepad));
        assert_eq!(hid_kind(WHEEL), Some(Kind::Wheel));
        assert_eq!(hid_kind(PEDALS), Some(Kind::Pedals));
        assert_eq!(hid_kind(MOUSE_KB), Some(Kind::Mouse));
        assert_eq!(hid_kind(&[0x05]), None, "truncated descriptors are ignored");
    }

    fn device(vid: u16, pid: u16, class: u8, ifaces: Vec<Interface>) -> UsbDevice {
        UsbDevice {
            busid: "1-2".into(),
            path: String::new(),
            busnum: 1,
            devnum: 2,
            speed: 2,
            vid,
            pid,
            bcd_device: 0,
            class,
            subclass: 0,
            protocol: 0,
            config_value: 1,
            num_configs: 1,
            manufacturer: Some("ACME".into()),
            product: Some("Thing".into()),
            driver: None,
            interfaces: ifaces,
        }
    }

    fn iface(class: u8, hid: Option<&[u8]>) -> Interface {
        Interface { class, subclass: 0, protocol: 0, hid: hid.map(<[u8]>::to_vec) }
    }

    #[test]
    fn identifies_devices() {
        let g29 = identify(&device(0x046d, 0xc24f, 0, vec![iface(3, Some(GAMEPAD))]), None);
        assert_eq!((g29.title.as_str(), g29.kind, g29.source), ("Logitech G29 Driving Force", Kind::Wheel, Source::Catalog));

        // A gaming keyboard with a gamepad interface counts as a game device.
        let combo = identify(&device(0x1234, 0x0001, 0, vec![iface(3, Some(KEYBOARD)), iface(3, Some(GAMEPAD))]), None);
        assert_eq!((combo.kind, combo.source, combo.name.as_str()), (Kind::Gamepad, Source::Device, "Thing"));

        let kb = Interface { class: 3, subclass: 1, protocol: 1, hid: None };
        let mouse = Interface { class: 3, subclass: 1, protocol: 2, hid: None };
        let mut dongle = device(0x373b, 0x101b, 0, vec![kb, mouse]);
        dongle.product = Some("ATK Mouse 8K Dongle".into());
        let dongle = identify(&dongle, None);
        assert_eq!((dongle.kind, dongle.title.as_str()), (Kind::Mouse, "ACME ATK Mouse 8K Dongle"));

        let disk = identify(&device(0x0bc2, 0xab24, 0, vec![iface(8, None)]), None);
        assert_eq!(disk.blocked.as_deref(), Some("storage device"));
        assert_eq!(identify(&device(1, 1, 9, vec![]), None).blocked.as_deref(), Some("USB hub"));
    }

    #[test]
    fn reads_usb_ids() {
        let ids = UsbIds(
            "# comment\n046d  Logitech, Inc.\n\tc24f  G29 Driving Force Racing Wheel [PS3]\n\t\t0001  iface\n\
             046e  Behavior Tech\n\tc24f  Not this one\n"
                .into(),
        );
        assert_eq!(ids.lookup(0x046d, 0xc24f), (Some("Logitech, Inc."), Some("G29 Driving Force Racing Wheel [PS3]")));
        assert_eq!(ids.lookup(0x046d, 0xffff), (Some("Logitech, Inc."), None));
        assert_eq!(ids.lookup(0xdead, 0xc24f), (None, None));
        assert_eq!(tidy_vendor("Logitech, Inc."), "Logitech");
    }
}
