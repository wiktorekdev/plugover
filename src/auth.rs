//! USB/IP has no authentication, so every connection starts with a challenge: the relay sends a
//! fresh nonce and the host answers HMAC(key, nonce || context) with the key both sides agreed
//! on when they were paired.
//!
//! Pairing runs SPAKE2 on a six-digit code the relay prints. An eavesdropper learns nothing from
//! the exchange and someone guessing gets one try per connection, which the relay limits, so a
//! short code is enough.

use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};
use spake2::{Ed25519Group, Identity, Password, Spake2};

pub const TAG_LEN: usize = 32;

pub type Key = [u8; 32];
/// Short public name of a paired host, derived from its pairing key.
pub type HostId = [u8; 8];

pub fn random<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    getrandom::fill(&mut b).expect("the OS random source failed");
    b
}

fn mac(key: &Key, nonce: &[u8], context: &str) -> Hmac<Sha256> {
    let mut m = Hmac::<Sha256>::new_from_slice(key).expect("HMAC takes any key length");
    m.update(nonce);
    m.update(context.as_bytes());
    m
}

pub fn sign(key: &Key, nonce: &[u8], context: &str) -> [u8; TAG_LEN] {
    let mut tag = [0u8; TAG_LEN];
    tag.copy_from_slice(&mac(key, nonce, context).finalize().into_bytes());
    tag
}

/// Constant-time check.
pub fn verify(key: &Key, nonce: &[u8], context: &str, tag: &[u8]) -> bool {
    mac(key, nonce, context).verify_slice(tag).is_ok()
}

pub type Pake = Spake2<Ed25519Group>;

pub fn new_code() -> String {
    format!("{:06}", u64::from_ne_bytes(random()) % 1_000_000)
}

/// Accepts the code as people type it: "482913", "482 913", "482-913".
pub fn normalize_code(input: &str) -> Option<String> {
    let code: String = input.chars().filter(|c| !c.is_whitespace() && *c != '-').collect();
    (code.len() == 6 && code.bytes().all(|b| b.is_ascii_digit())).then_some(code)
}

/// Host side is SPAKE2's A, the relay B. Returns the state and the message to send.
pub fn pake_start(code: &str, host: bool) -> (Pake, Vec<u8>) {
    let (pw, a, b) = (Password::new(code), Identity::new(b"plugover host"), Identity::new(b"plugover relay"));
    if host { Pake::start_a(&pw, &a, &b) } else { Pake::start_b(&pw, &a, &b) }
}

/// The long-term pairing key. Both sides only get the same one if they used the same code,
/// which the confirmation tags then prove.
pub fn pake_finish(state: Pake, peer_msg: &[u8]) -> Option<Key> {
    let k = state.finish(peer_msg).ok()?;
    let mut h = Sha256::new();
    h.update(b"plugover pair v2");
    h.update(&k);
    Some(h.finalize().into())
}

pub const CONFIRM_HOST: &str = "pair confirm host";
pub const CONFIRM_RELAY: &str = "pair confirm relay";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tag_binds_key_nonce_and_context() {
        let key = [7u8; 32];
        let n: [u8; 32] = random();
        let tag = sign(&key, &n, "1-2");
        assert!(verify(&key, &n, "1-2", &tag));
        assert!(!verify(&key, &n, "1-3", &tag));
        assert!(!verify(&[8u8; 32], &n, "1-2", &tag));
        assert!(!verify(&key, &random::<32>(), "1-2", &tag));
    }

    #[test]
    fn same_code_same_key_wrong_code_no_match() {
        let pair = |host_code: &str, relay_code: &str| {
            let (h, hm) = pake_start(host_code, true);
            let (r, rm) = pake_start(relay_code, false);
            (pake_finish(h, &rm).unwrap(), pake_finish(r, &hm).unwrap())
        };
        let (h, r) = pair("482913", "482913");
        assert_eq!(h, r);
        let (h, r) = pair("482913", "482914");
        assert_ne!(h, r);
    }

    #[test]
    fn codes() {
        assert_eq!(new_code().len(), 6);
        assert_eq!(normalize_code(" 482-913 ").as_deref(), Some("482913"));
        assert_eq!(normalize_code("482 91"), None);
        assert_eq!(normalize_code("48a913"), None);
    }
}
