//! USB/IP wire format (big-endian), only the part Plugover needs: importing one device.

use std::io::{self, Read, Write};

const VERSION: u16 = 0x0111;
const OP_REQ_IMPORT: u16 = 0x8003;
const OP_REP_IMPORT: u16 = 0x0003;
const HEADER_LEN: usize = 8;
const BUSID_LEN: usize = 32;
const PATH_LEN: usize = 256;
const DEVICE_LEN: usize = PATH_LEN + BUSID_LEN + 24;

/// `struct usbip_usb_device` from the kernel's usbip headers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportedDevice {
    pub path: String,
    pub busid: String,
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
    pub num_interfaces: u8,
}

impl ExportedDevice {
    /// The id vhci_hcd expects on attach.
    pub fn devid(&self) -> u32 {
        self.busnum << 16 | self.devnum
    }

    fn encode(&self, out: &mut Vec<u8>) {
        put_str(out, &self.path, PATH_LEN);
        put_str(out, &self.busid, BUSID_LEN);
        for v in [self.busnum, self.devnum, self.speed] {
            out.extend(v.to_be_bytes());
        }
        for v in [self.vid, self.pid, self.bcd_device] {
            out.extend(v.to_be_bytes());
        }
        out.extend([
            self.class,
            self.subclass,
            self.protocol,
            self.config_value,
            self.num_configs,
            self.num_interfaces,
        ]);
    }

    fn decode(b: &[u8; DEVICE_LEN]) -> Self {
        let u32_at = |i: usize| u32::from_be_bytes(b[i..i + 4].try_into().unwrap());
        let u16_at = |i: usize| u16::from_be_bytes(b[i..i + 2].try_into().unwrap());
        let o = PATH_LEN + BUSID_LEN;
        Self {
            path: get_str(&b[..PATH_LEN]),
            busid: get_str(&b[PATH_LEN..o]),
            busnum: u32_at(o),
            devnum: u32_at(o + 4),
            speed: u32_at(o + 8),
            vid: u16_at(o + 12),
            pid: u16_at(o + 14),
            bcd_device: u16_at(o + 16),
            class: b[o + 18],
            subclass: b[o + 19],
            protocol: b[o + 20],
            config_value: b[o + 21],
            num_configs: b[o + 22],
            num_interfaces: b[o + 23],
        }
    }
}

fn put_str(out: &mut Vec<u8>, s: &str, len: usize) {
    let mut field = vec![0u8; len];
    let n = s.len().min(len - 1);
    field[..n].copy_from_slice(&s.as_bytes()[..n]);
    out.extend(field);
}

fn get_str(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).into_owned()
}

fn header(code: u16, status: u32) -> Vec<u8> {
    let mut h = Vec::with_capacity(HEADER_LEN);
    h.extend(VERSION.to_be_bytes());
    h.extend(code.to_be_bytes());
    h.extend(status.to_be_bytes());
    h
}

/// Returns the status field.
fn check_header(b: &[u8], code: u16) -> io::Result<u32> {
    let version = u16::from_be_bytes([b[0], b[1]]);
    let got = u16::from_be_bytes([b[2], b[3]]);
    if version != VERSION || got != code {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unexpected USB/IP message (version {version:#06x}, code {got:#06x})"),
        ));
    }
    Ok(u32::from_be_bytes([b[4], b[5], b[6], b[7]]))
}

pub fn import_request(busid: &str) -> Vec<u8> {
    let mut m = header(OP_REQ_IMPORT, 0);
    put_str(&mut m, busid, BUSID_LEN);
    m
}

pub fn read_import_request(r: &mut impl Read) -> io::Result<String> {
    let mut b = [0u8; HEADER_LEN + BUSID_LEN];
    r.read_exact(&mut b)?;
    check_header(&b, OP_REQ_IMPORT)?;
    Ok(get_str(&b[HEADER_LEN..]))
}

/// `None` tells the importer the device is not available.
pub fn write_import_reply(w: &mut impl Write, dev: Option<&ExportedDevice>) -> io::Result<()> {
    let mut m = header(OP_REP_IMPORT, dev.is_none() as u32);
    if let Some(d) = dev {
        d.encode(&mut m);
    }
    w.write_all(&m)
}

pub fn read_import_reply(r: &mut impl Read) -> io::Result<ExportedDevice> {
    let mut h = [0u8; HEADER_LEN];
    r.read_exact(&mut h)?;
    if check_header(&h, OP_REP_IMPORT)? != 0 {
        return Err(io::Error::other("the relay refused the device"));
    }
    let mut b = [0u8; DEVICE_LEN];
    r.read_exact(&mut b)?;
    Ok(ExportedDevice::decode(&b))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> ExportedDevice {
        ExportedDevice {
            path: "/sys/devices/pci0000:00/usb1/1-2".into(),
            busid: "1-2".into(),
            busnum: 1,
            devnum: 5,
            speed: 2,
            vid: 0x046d,
            pid: 0xc24f,
            bcd_device: 0x0100,
            class: 0,
            subclass: 0,
            protocol: 0,
            config_value: 1,
            num_configs: 1,
            num_interfaces: 1,
        }
    }

    #[test]
    fn import_round_trip() {
        let req = import_request("1-2");
        assert_eq!(req.len(), 40);
        assert_eq!(read_import_request(&mut &req[..]).unwrap(), "1-2");

        let mut rep = Vec::new();
        write_import_reply(&mut rep, Some(&sample())).unwrap();
        assert_eq!(rep.len(), HEADER_LEN + 312);
        let dev = read_import_reply(&mut &rep[..]).unwrap();
        assert_eq!(dev, sample());
        assert_eq!(dev.devid(), 0x0001_0005);
    }

    #[test]
    fn refusal_and_garbage_are_errors() {
        let mut rep = Vec::new();
        write_import_reply(&mut rep, None).unwrap();
        assert!(read_import_reply(&mut &rep[..]).is_err());
        assert!(read_import_request(&mut &[0u8; 40][..]).is_err());
    }
}
