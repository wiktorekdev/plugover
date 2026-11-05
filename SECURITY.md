# Security

## Reporting a vulnerability

Report it privately with **Report a vulnerability** on the
[Security tab](https://github.com/wiktorekdev/plugover/security), not in a public issue. Say which
version you tested and how to reproduce it.

Only the latest release gets fixes.

## What Plugover protects, and what it does not

- **Pairing** uses a one-time six-digit code in a SPAKE2 key exchange, so the code never crosses
  the network and cannot be guessed offline. A code works once, for 15 minutes, and the relay
  closes pairing after five wrong guesses.
- **Every connection** to the relay must prove it holds the key from pairing. Unpaired machines get
  no device list and cannot connect devices.
- **On Windows**, setup blocks usbipd-win's own port (TCP 3240), which has no authentication, so
  Plugover's pairing is the only way in.
- **The device list and USB traffic are encrypted** with AES-128-GCM in TLS 1.3 records. There is
  no TLS handshake: both sides derive the keys from the pairing key and a fresh nonce for every
  connection. A record that was tampered with ends the connection. On Linux the kernel encrypts USB
  traffic (kTLS) when its `tls` module is there, and Plugover does otherwise.
- **Relays before 0.6 cannot encrypt.** A host connects their devices only after you allow it for
  that relay. Once a relay has encrypted, the host never accepts it unencrypted again.
- **Releases** are signed: `plugover update` installs only a release whose `SHA256SUMS` carries a
  valid signature from the key in `packaging/release-key.pem`.
- **Not hidden:** that two machines run Plugover, which relay port a host asks for, and how much
  traffic flows when. Someone on the network can also cut a connection; they cannot read or change
  it.
