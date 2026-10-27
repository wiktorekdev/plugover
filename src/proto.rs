//! Everything on the wire besides USB/IP itself. One TCP port; the first four bytes of a
//! connection pick the channel.

use crate::{auth, catalog::Identity};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::io::{self, BufRead, Read, Write};

pub const DEFAULT_PORT: u16 = 7275;
/// Lend one device: challenge, then a USB/IP import.
pub const DATA: &[u8; 4] = b"PLG1";
/// Device list and its changes, as JSON lines.
pub const CONTROL: &[u8; 4] = b"PLC1";
/// Pair a new host with the code the relay printed.
pub const PAIR: &[u8; 4] = b"PLP2";
pub const CONTROL_CONTEXT: &str = "control";

/// Host side of the challenge: announce who we are, get the relay's nonce.
pub fn challenge(s: &mut (impl Read + Write), magic: &[u8; 4], host: &auth::HostId) -> io::Result<[u8; 32]> {
    let mut hello = magic.to_vec();
    hello.extend(host);
    s.write_all(&hello)?;
    let mut nonce = [0u8; 32];
    s.read_exact(&mut nonce)?;
    Ok(nonce)
}

pub fn send<T: Serialize>(w: &mut impl Write, msg: &T) -> io::Result<()> {
    let mut line = serde_json::to_vec(msg)?;
    line.push(b'\n');
    w.write_all(&line)
}

pub fn recv<T: DeserializeOwned>(r: &mut impl BufRead) -> io::Result<T> {
    let mut line = String::new();
    if r.read_line(&mut line)? == 0 {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    serde_json::from_str(&line).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
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
    Pake { relay_id: String, relay: String, pake: String },
    Paired { tag: String },
    Refused { reason: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
}
