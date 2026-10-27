//! Relays announce themselves over mDNS so hosts find them without typing addresses.

use anyhow::Result;
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use std::{
    net::{IpAddr, SocketAddr, TcpStream},
    time::{Duration, Instant},
};

const SERVICE: &str = "_plugover._tcp.local.";

/// Keep the returned daemon alive for as long as the relay should be visible.
pub fn advertise(name: &str, id: &str, port: u16) -> Result<ServiceDaemon> {
    let daemon = ServiceDaemon::new()?;
    let info = ServiceInfo::new(SERVICE, name, &format!("{name}.local."), "", port, &[("id", id), ("v", "1")][..])?
        .enable_addr_auto();
    daemon.register(info)?;
    Ok(daemon)
}

#[derive(Debug, Clone)]
pub struct Found {
    pub id: String,
    pub name: String,
    pub addresses: Vec<SocketAddr>,
}

/// Collects relays that answer within `wait`.
pub fn browse(wait: Duration) -> Result<Vec<Found>> {
    let daemon = ServiceDaemon::new()?;
    let events = daemon.browse(SERVICE)?;
    let deadline = Instant::now() + wait;
    let mut found: Vec<Found> = Vec::new();
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        let Ok(event) = events.recv_timeout(left) else { break };
        if let ServiceEvent::ServiceResolved(s) = event {
            let Some(id) = s.get_property_val_str("id").map(str::to_string) else { continue };
            let mut addresses: Vec<SocketAddr> =
                s.get_addresses_v4().into_iter().map(|ip| SocketAddr::new(IpAddr::V4(ip), s.get_port())).collect();
            addresses.sort();
            let name = s.fullname.split('.').next().unwrap_or_default().to_string();
            found.retain(|f| f.id != id);
            found.push(Found { id, name, addresses });
        }
    }
    let _ = daemon.shutdown();
    Ok(found)
}

/// Relays announce every interface address (docker bridges, VPNs); use the first that answers.
pub fn reachable(addresses: &[SocketAddr]) -> Option<SocketAddr> {
    addresses.iter().copied().find(|a| TcpStream::connect_timeout(a, Duration::from_millis(800)).is_ok())
}
