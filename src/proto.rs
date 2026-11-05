//! Everything on the wire besides USB/IP itself. One TCP port; the first four bytes of a
//! connection pick the channel.

use crate::{auth, catalog::Identity};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::io::{self, BufRead, Read, Write};

pub const DEFAULT_PORT: u16 = 7275;
/// Lend one device: challenge, then a USB/IP import.
pub const DATA: &[u8; 4] = b"PLG1";
/// The same, and everything after the import request is encrypted (`tls`). Since 0.6.
pub const DATA_ENCRYPTED: &[u8; 4] = b"PLG2";
/// Device list and its changes, as JSON lines.
pub const CONTROL: &[u8; 4] = b"PLC1";
/// The same, encrypted after the challenge. Since 0.6.
pub const CONTROL_ENCRYPTED: &[u8; 4] = b"PLC2";
/// Pair a new host with the code the relay printed.
pub const PAIR: &[u8; 4] = b"PLP2";
pub const CONTROL_CONTEXT: &str = "control";
/// Sent instead of a nonce to a host the relay does not know (never paired, or unpaired since).
/// Older hosts just fail the challenge with it.
pub const UNPAIRED: &[u8; 32] = b"plugover: this host is unpaired.";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Host side of the challenge: announce who we are, get the relay's nonce.
pub fn challenge(s: &mut (impl Read + Write), magic: &[u8; 4], host: &auth::HostId) -> io::Result<[u8; 32]> {
    let mut hello = magic.to_vec();
    hello.extend(host);
    s.write_all(&hello)?;
    let mut nonce = [0u8; 32];
    s.read_exact(&mut nonce)?;
    if &nonce == UNPAIRED {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "the relay does not know this host anymore; pair again"));
    }
    Ok(nonce)
}

pub fn send<T: Serialize>(w: &mut impl Write, msg: &T) -> io::Result<()> {
    let mut line = serde_json::to_vec(msg)?;
    line.push(b'\n');
    w.write_all(&line)
}

/// Longest line anyone sends; a snapshot of a busy relay is a few KiB. Stops a peer from
/// growing our memory with a line that never ends.
const MAX_LINE: u64 = 1 << 20;

/// One line without its newline; `None` at the end of the stream.
pub fn read_line(r: &mut impl BufRead) -> io::Result<Option<String>> {
    let mut line = String::new();
    if r.take(MAX_LINE).read_line(&mut line)? == 0 {
        return Ok(None);
    }
    if !line.ends_with('\n') && line.len() as u64 >= MAX_LINE {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "line too long"));
    }
    Ok(Some(line))
}

pub fn recv<T: DeserializeOwned>(r: &mut impl BufRead) -> io::Result<T> {
    let line = read_line(r)?.ok_or(io::ErrorKind::UnexpectedEof)?;
    serde_json::from_str(&line).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Sent by the host on the control channel to time the link; the relay answers with `Pong`.
/// Relays before 0.5.1 never read the channel and do not answer.
#[derive(Debug, Serialize, Deserialize)]
pub struct Ping {
    pub ping: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Pong {
    pub pong: u64,
}

/// What the relay sends on the control channel.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum FromRelay {
    Pong(Pong),
    Snapshot(Snapshot),
}

/// Pairing, as JSON lines: hello, the relay's SPAKE2 answer, the host proves it has the code,
/// then the relay does.
#[derive(Debug, Serialize, Deserialize)]
pub struct PairHello {
    pub name: String,
    /// Random id the host picks for itself towards this relay, hex.
    pub host_id: String,
    /// SPAKE2 message, hex.
    pub pake: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PairConfirm {
    /// HMAC over the host id with the new key, hex.
    pub tag: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum PairReply {
    Pake {
        relay_id: String,
        relay: String,
        pake: String,
        /// Plugover on the relay; relays before 0.6 send none, and cannot encrypt.
        #[serde(default)]
        version: String,
    },
    Paired { tag: String },
    Refused { reason: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct DeviceInfo {
    pub busid: String,
    /// vendor:product, e.g. 046d:c24f.
    pub id: String,
    #[serde(flatten)]
    pub identity: Identity,
    /// Name of the host that has it right now.
    pub lent_to: Option<String>,
}

/// Sent by the relay on connect, on every change, and every few seconds as a heartbeat.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub relay_id: String,
    pub relay: String,
    pub devices: Vec<DeviceInfo>,
    /// Plugover version on the relay; empty before 0.5.
    #[serde(default)]
    pub version: String,
    /// Came over an encrypted channel; set by the host, not sent.
    #[serde(skip)]
    pub encrypted: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_lines_round_trip() {
        let mut buf = Vec::new();
        send(&mut buf, &PairReply::Paired { tag: "ab".into() }).unwrap();
        send(&mut buf, &PairReply::Refused { reason: "timed out".into() }).unwrap();
        let mut r = io::BufReader::new(&buf[..]);
        assert!(matches!(recv(&mut r).unwrap(), PairReply::Paired { tag } if tag == "ab"));
        assert!(matches!(recv(&mut r).unwrap(), PairReply::Refused { .. }));
        assert_eq!(recv::<PairReply>(&mut r).unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn endless_lines_are_refused() {
        let flood = vec![b'x'; MAX_LINE as usize + 10];
        assert_eq!(recv::<Ping>(&mut io::BufReader::new(&flood[..])).unwrap_err().kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn pongs_and_snapshots_tell_apart() {
        let snap = Snapshot { relay_id: "r".into(), relay: "rig".into(), devices: vec![], version: "0.5.1".into(), encrypted: false };
        let mut buf = Vec::new();
        send(&mut buf, &Pong { pong: 7 }).unwrap();
        send(&mut buf, &snap).unwrap();
        let mut r = io::BufReader::new(&buf[..]);
        assert!(matches!(recv(&mut r).unwrap(), FromRelay::Pong(Pong { pong: 7 })));
        assert!(matches!(recv(&mut r).unwrap(), FromRelay::Snapshot(s) if s == snap));
    }
}
