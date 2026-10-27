use crate::{sysfs::UsbDevice, usbip::ExportedDevice};
use socket2::{SockRef, TcpKeepalive};
use std::{io, net::TcpStream, time::Duration};

pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// Prepares a socket for the kernel: no Nagle delay, quick dead-peer detection, and no read
/// timeout (the kernel honours SO_RCVTIMEO and would drop an idle wheel).
pub fn tune(s: &TcpStream) -> io::Result<()> {
    s.set_nodelay(true)?;
    s.set_read_timeout(None)?;
    let keepalive = TcpKeepalive::new().with_time(Duration::from_secs(3)).with_interval(Duration::from_secs(1));
    #[cfg(target_os = "linux")]
    let keepalive = keepalive.with_retries(3);
    SockRef::from(s).set_tcp_keepalive(&keepalive)
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
