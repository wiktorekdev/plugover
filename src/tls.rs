//! Encryption of Plugover's connections: TLS 1.3 records with AES-128-GCM, byte for byte what the
//! kernel's TLS (kTLS) writes, so either side can encrypt in the kernel or here and they still
//! understand each other. There is no TLS handshake: both sides derive the keys from the pairing
//! key and the connection's nonce, so every connection gets its own.
//!
//! The kernel does it for USB traffic on Linux when its tls module is there (`net::encrypt`);
//! Plugover does it for the control channel, on Windows, and where the kernel cannot.

use crate::auth;
use aes_gcm::{
    Aes128Gcm, KeyInit,
    aead::{AeadInOut, array::Array},
};
use std::io::{self, Read, Write};

/// Largest plaintext in one record, as TLS 1.3 allows.
const MAX_RECORD: usize = 1 << 14;
const TAG: usize = 16;
const HEADER: usize = 5;
/// TLS 1.3 records all say application data, TLS 1.2, on the outside.
const APPLICATION_DATA: u8 = 0x17;

/// One direction's key and IV. The IV is the kernel's salt (4 bytes) followed by its iv (8).
pub struct Keys {
    pub key: [u8; 16],
    pub iv: [u8; 12],
}

/// The keys this side sends with and receives with.
pub fn keys(pairing: &auth::Key, nonce: &[u8], relay: bool) -> (Keys, Keys) {
    let derive = |direction| {
        let t = auth::sign(pairing, nonce, direction);
        Keys { key: t[..16].try_into().unwrap(), iv: t[16..28].try_into().unwrap() }
    };
    let (to_host, to_relay) = (derive("plugover relay to host"), derive("plugover host to relay"));
    if relay { (to_host, to_relay) } else { (to_relay, to_host) }
}

struct Cipher {
    aes: Aes128Gcm,
    iv: [u8; 12],
    seq: u64,
}

impl Cipher {
    fn new(k: Keys) -> Self {
        Self { aes: Aes128Gcm::new(&k.key.into()), iv: k.iv, seq: 0 }
    }

    /// The IV with the record's sequence number mixed into its last 8 bytes.
    fn next_nonce(&mut self) -> Array<u8, aes_gcm::aead::consts::U12> {
        let mut n = self.iv;
        for (b, s) in n[4..].iter_mut().zip(self.seq.to_be_bytes()) {
            *b ^= s;
        }
        self.seq += 1;
        n.into()
    }
}

/// Encrypts everything written to it into records on `inner`.
pub struct Writer<W> {
    inner: W,
    cipher: Cipher,
    record: Vec<u8>,
}

impl<W: Write> Write for Writer<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        for chunk in buf.chunks(MAX_RECORD) {
            let len = chunk.len() + 1 + TAG;
            let r = &mut self.record;
            r.clear();
            r.extend([APPLICATION_DATA, 3, 3]);
            r.extend((len as u16).to_be_bytes());
            r.extend(chunk);
            // The real content type goes inside, after the data.
            r.push(APPLICATION_DATA);
            let nonce = self.cipher.next_nonce();
            let (header, body) = r.split_at_mut(HEADER);
            let tag = self.cipher.aes.encrypt_inout_detached(&nonce, header, body.into()).map_err(|_| io::Error::other("encryption failed"))?;
            r.extend_from_slice(&tag);
            self.inner.write_all(r)?;
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Decrypts records from `inner`. A record that was tampered with, or sent with another key,
/// is an error; nothing of it is returned.
pub struct Reader<R> {
    /// Buffered, so a record usually takes one read from the socket rather than two.
    inner: io::BufReader<R>,
    cipher: Cipher,
    plain: Vec<u8>,
    at: usize,
}

impl<R: Read> Reader<R> {
    /// Reads the next record into `plain`; false at the end of the stream.
    fn fill(&mut self) -> io::Result<bool> {
        let bad = |what: &str| io::Error::new(io::ErrorKind::InvalidData, format!("{what} (is the other side using another key?)"));
        let mut header = [0u8; HEADER];
        match self.inner.read_exact(&mut header) {
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(false),
            r => r?,
        }
        let len = u16::from_be_bytes([header[3], header[4]]) as usize;
        if header[..3] != [APPLICATION_DATA, 3, 3] || !(TAG + 1..=MAX_RECORD + 256).contains(&len) {
            return Err(bad("not an encrypted record"));
        }
        self.plain.resize(len, 0);
        self.inner.read_exact(&mut self.plain)?;
        let (body, tag) = self.plain.split_at_mut(len - TAG);
        let tag = Array::try_from(&*tag).expect("16 bytes");
        let nonce = self.cipher.next_nonce();
        self.cipher.aes.decrypt_inout_detached(&nonce, &header, body.into(), &tag).map_err(|_| bad("a record did not decrypt"))?;
        self.plain.truncate(len - TAG);
        // Padding zeros, then the content type.
        while self.plain.last() == Some(&0) {
            self.plain.pop();
        }
        if self.plain.pop() != Some(APPLICATION_DATA) {
            return Err(bad("a record that is not data"));
        }
        self.at = 0;
        Ok(true)
    }
}

impl<R: Read> Read for Reader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        while self.at == self.plain.len() {
            if !self.fill()? {
                return Ok(0);
            }
        }
        let n = buf.len().min(self.plain.len() - self.at);
        buf[..n].copy_from_slice(&self.plain[self.at..self.at + n]);
        self.at += n;
        Ok(n)
    }
}

/// Encrypts a connection here: the reader and writer to use instead of the stream.
pub fn wrap<R: Read, W: Write>(read: R, write: W, pairing: &auth::Key, nonce: &[u8], relay: bool) -> (Reader<R>, Writer<W>) {
    let (tx, rx) = keys(pairing, nonce, relay);
    let inner = io::BufReader::with_capacity(HEADER + MAX_RECORD + 256, read);
    (Reader { inner, cipher: Cipher::new(rx), plain: Vec::new(), at: 0 }, Writer { inner: write, cipher: Cipher::new(tx), record: Vec::new() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{TcpListener, TcpStream};

    fn pair() -> (TcpStream, TcpStream) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let a = TcpStream::connect(l.local_addr().unwrap()).unwrap();
        (a, l.accept().unwrap().0)
    }

    fn wrapped(s: &TcpStream, key: &auth::Key, relay: bool) -> (Reader<TcpStream>, Writer<TcpStream>) {
        wrap(s.try_clone().unwrap(), s.try_clone().unwrap(), key, &[9u8; 32], relay)
    }

    #[test]
    fn records_round_trip_and_hide_the_data() {
        let (host, relay) = pair();
        let ((mut hr, mut hw), (mut rr, mut rw)) = (wrapped(&host, &[7; 32], false), wrapped(&relay, &[7; 32], true));
        let big = vec![0xab; MAX_RECORD * 2 + 5];
        hw.write_all(b"submit urb").unwrap();
        hw.write_all(&big).unwrap();
        let mut got = vec![0u8; 10 + big.len()];
        rr.read_exact(&mut got).unwrap();
        assert_eq!((&got[..10], &got[10..]), (&b"submit urb"[..], &big[..]));
        rw.write_all(b"urb done").unwrap();
        let mut got = [0u8; 8];
        hr.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"urb done");

        let (host, mut plain) = pair();
        wrapped(&host, &[7; 32], false).1.write_all(b"submit urb").unwrap();
        let mut wire = [0u8; HEADER + 10 + 1 + TAG];
        plain.read_exact(&mut wire).unwrap();
        assert_eq!(wire[..3], [0x17, 0x03, 0x03]);
        assert!(!wire.windows(10).any(|w| w == b"submit urb"));
    }

    #[test]
    fn tampering_and_wrong_keys_fail() {
        let (host, relay) = pair();
        wrapped(&host, &[7; 32], false).1.write_all(b"submit urb").unwrap();
        assert!(wrapped(&relay, &[8; 32], true).0.read_exact(&mut [0u8; 10]).is_err(), "another key");

        // One flipped bit on the way.
        let mut record = Vec::new();
        wrap(io::empty(), &mut record, &[7; 32], &[9; 32], false).1.write_all(b"press A").unwrap();
        record[HEADER + 2] ^= 1;
        let (mut r, _) = wrap(&record[..], io::sink(), &[7; 32], &[9; 32], true);
        assert_eq!(r.read(&mut [0u8; 7]).unwrap_err().kind(), io::ErrorKind::InvalidData);
    }
}
