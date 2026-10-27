//! Talking to a relay from the host: pairing and watching its devices. Plugging one in lives in
//! `vhci`, because it differs per OS.

use crate::{
    auth, net,
    proto::{self, PairConfirm, PairHello, PairReply, Snapshot},
    state::{self, PairedRelay},
};
use anyhow::{Context, Result, bail};
use std::{
    io::{BufReader, Write},
    net::{SocketAddr, TcpStream, ToSocketAddrs},
    time::Duration,
};

/// Relays send a snapshot at least every 5 s; silence for longer means the link is dead.
const SILENCE: Duration = Duration::from_secs(12);

pub fn resolve(address: &str) -> Result<SocketAddr> {
    let target = if address.contains(':') { address.to_string() } else { format!("{address}:{}", proto::DEFAULT_PORT) };
    target.to_socket_addrs()?.next().with_context(|| format!("cannot resolve {address}"))
}

pub fn connect(address: &str) -> Result<TcpStream> {
    let s = TcpStream::connect_timeout(&resolve(address)?, net::HANDSHAKE_TIMEOUT)
        .with_context(|| format!("connecting to {address}"))?;
    s.set_read_timeout(Some(net::HANDSHAKE_TIMEOUT))?;
    Ok(s)
}

pub fn credentials(relay: &PairedRelay) -> Result<(auth::HostId, auth::Key)> {
    let id = hex::decode(&relay.host_id).ok().and_then(|b| b.try_into().ok()).context("stored host id is malformed")?;
    Ok((id, state::decode_key(&relay.key)?))
}

/// Pairs with the relay at `address` using the code it printed.
pub fn pair(address: &str, our_name: &str, code: &str) -> Result<PairedRelay> {
    let code = auth::normalize_code(code).context("the code has six digits")?;
    let mut s = connect(address)?;
    let host_id: auth::HostId = auth::random();
    let (pake, msg) = auth::pake_start(&code, true);
    s.write_all(proto::PAIR)?;
    proto::send(&mut s, &PairHello { name: our_name.into(), host_id: hex::encode(host_id), pake: hex::encode(msg) })?;
    let mut r = BufReader::new(s.try_clone()?);
    let (relay_id, relay, relay_msg) = match proto::recv(&mut r)? {
        PairReply::Pake { relay_id, relay, pake } => (relay_id, relay, pake),
        PairReply::Refused { reason } => bail!("the relay refused: {reason}"),
        PairReply::Paired { .. } => bail!("unexpected pairing message"),
    };
    let key = hex::decode(relay_msg).ok().and_then(|m| auth::pake_finish(pake, &m)).context("the relay sent a malformed key")?;
    proto::send(&mut s, &PairConfirm { tag: hex::encode(auth::sign(&key, &host_id, auth::CONFIRM_HOST)) })?;
    match proto::recv(&mut r)? {
        PairReply::Paired { tag } => {
            let tag = hex::decode(tag).unwrap_or_default();
            // Whoever answered without the code is not the relay that printed it.
            if !auth::verify(&key, &host_id, auth::CONFIRM_RELAY, &tag) {
                bail!("{relay} could not prove it knows the code; is that really your relay?");
            }
            Ok(PairedRelay {
                id: relay_id,
                name: relay,
                address: s.peer_addr()?.to_string(),
                host_id: hex::encode(host_id),
                key: hex::encode(key),
            })
        }
        PairReply::Refused { reason } => bail!("the relay refused: {reason}"),
        PairReply::Pake { .. } => bail!("unexpected pairing message"),
    }
}

/// Streams the relay's device snapshots into `on_snapshot` until the link fails or the
/// callback returns false.
pub fn watch(relay: &PairedRelay, mut on_snapshot: impl FnMut(Snapshot) -> bool) -> Result<()> {
    let (id, key) = credentials(relay)?;
    let mut s = connect(&relay.address)?;
    let nonce = proto::challenge(&mut s, proto::CONTROL, &id)?;
    s.write_all(&auth::sign(&key, &nonce, proto::CONTROL_CONTEXT))?;
    s.set_read_timeout(Some(SILENCE))?;
    let mut r = BufReader::new(s);
    while on_snapshot(proto::recv(&mut r)?) {}
    Ok(())
}
