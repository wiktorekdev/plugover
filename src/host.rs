//! Talking to a relay from the host: pairing and watching its devices. Plugging one in lives in
//! `vhci`, because it differs per OS.

use crate::{
    auth, net,
    tls,
    proto::{self, FromRelay, PairConfirm, PairHello, PairReply, Ping, Snapshot},
    state::{self, PairedRelay},
};
use anyhow::{Context, Result, bail, ensure};
use std::{
    io::{self, BufReader, Read, Write},
    net::{Shutdown, SocketAddr, TcpStream, ToSocketAddrs},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

/// Relays send a snapshot at least every 2 s (every 5 s before 0.5.1, checked every 2 s); silence
/// for longer means the link is dead.
const SILENCE: Duration = Duration::from_secs(8);
/// How often the link is timed.
const PING: Duration = Duration::from_secs(1);
/// Pings without a pong before giving up: relays before 0.5.1 never read them.
const UNANSWERED: u64 = 5;

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
    let (relay_id, relay, relay_msg, version) = match proto::recv(&mut r)? {
        PairReply::Pake { relay_id, relay, pake, version } => (relay_id, relay, pake, version),
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
                encrypts: !version.is_empty(),
                unencrypted: false,
            })
        }
        PairReply::Refused { reason } => bail!("the relay refused: {reason}"),
        PairReply::Pake { .. } => bail!("unexpected pairing message"),
    }
}

/// Opens the control channel, encrypted unless the relay is too old to. Returns the connection,
/// its nonce and whether it is encrypted.
fn open_control(relay: &PairedRelay, id: &auth::HostId) -> Result<(TcpStream, [u8; 32], bool)> {
    let mut s = connect(&relay.address)?;
    match proto::challenge(&mut s, proto::CONTROL_ENCRYPTED, id) {
        Ok(nonce) => Ok((s, nonce, true)),
        // Relays before 0.6 hang up on a channel they do not know.
        Err(e) if matches!(e.kind(), io::ErrorKind::UnexpectedEof | io::ErrorKind::ConnectionReset | io::ErrorKind::ConnectionAborted) => {
            ensure!(
                !relay.encrypts,
                "{} stopped encrypting. If you did not put an older Plugover on it, someone on the network may be \
                 interfering; if you did, forget it and pair again",
                relay.name
            );
            let mut s = connect(&relay.address)?;
            let nonce = proto::challenge(&mut s, proto::CONTROL, id)?;
            Ok((s, nonce, false))
        }
        Err(e) => Err(e.into()),
    }
}

/// Streams the relay's device snapshots into `on_snapshot` until the link fails or the
/// callback returns false, and each round trip of the link into `on_rtt`.
pub fn watch(relay: &PairedRelay, mut on_snapshot: impl FnMut(Snapshot) -> bool, mut on_rtt: impl FnMut(Duration)) -> Result<()> {
    let (id, key) = credentials(relay)?;
    let (mut s, nonce, encrypted) = open_control(relay, &id)?;
    s.write_all(&auth::sign(&key, &nonce, proto::CONTROL_CONTEXT))?;
    net::tune(&s)?;
    s.set_read_timeout(Some(SILENCE))?;
    let (r, mut w): (Box<dyn Read + Send>, Box<dyn Write + Send>) = if encrypted {
        let (r, w) = tls::wrap(s.try_clone()?, s.try_clone()?, &key, &nonce, false);
        (Box::new(r), Box::new(w))
    } else {
        (Box::new(s.try_clone()?), Box::new(s.try_clone()?))
    };
    let last_ping: Arc<Mutex<(u64, Instant)>> = Arc::new(Mutex::new((0, Instant::now())));
    let answered = Arc::new(AtomicBool::new(false));
    let (pinged, heard) = (last_ping.clone(), answered.clone());
    thread::spawn(move || {
        for n in 1.. {
            thread::sleep(PING);
            // An older relay would only pile them up unread.
            if n > UNANSWERED && !heard.load(Ordering::Relaxed) {
                break;
            }
            *pinged.lock().unwrap() = (n, Instant::now());
            // Fails once `watch` returns and shuts the socket down.
            if proto::send(&mut w, &Ping { ping: n }).is_err() {
                break;
            }
        }
    });
    let mut r = BufReader::new(r);
    // A pong that waited while `on_snapshot` ran (it may plug devices in) says nothing about the
    // network, so only pings sent after that are timed.
    let mut idle_since = Instant::now();
    let result = loop {
        match proto::recv(&mut r) {
            Ok(FromRelay::Snapshot(mut snap)) => {
                snap.encrypted = encrypted;
                if !on_snapshot(snap) {
                    break Ok(());
                }
                idle_since = Instant::now();
            }
            Ok(FromRelay::Pong(p)) => {
                answered.store(true, Ordering::Relaxed);
                let (n, at) = *last_ping.lock().unwrap();
                if p.pong == n && at >= idle_since {
                    on_rtt(at.elapsed());
                }
            }
            Err(e) => break Err(e.into()),
        }
    };
    let _ = s.shutdown(Shutdown::Both);
    result
}
