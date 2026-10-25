//! flytoMesh shared protocol: wire format, signed roster, control-plane crypto.
//!
//! Everything a node and a relay must agree on lives here; the data plane itself is
//! WireGuard (boringtun) and is not wrapped, except for the 4-byte relay header.
//! Design and rationale: `docs/network.md`, section 3.

pub mod crypto;
pub mod release;
pub mod roster;
pub mod service;
pub mod wire;

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use std::io;

pub use boringtun::x25519;

/// Default UDP port of a node.
pub const NODE_PORT: u16 = 41820;
/// Default UDP port of a relay.
pub const RELAY_PORT: u16 = 41900;

pub fn b64(bytes: &[u8]) -> String {
    B64.encode(bytes)
}

/// Decodes a base64 key of exactly 32 bytes.
pub fn decode_key(s: &str) -> io::Result<[u8; 32]> {
    B64.decode(s.trim())
        .ok()
        .and_then(|v| <[u8; 32]>::try_from(v).ok())
        .ok_or_else(|| invalid("bad key (want base64 of 32 bytes)"))
}

pub fn fill_random(buf: &mut [u8]) {
    use ring::rand::SecureRandom;
    ring::rand::SystemRandom::new().fill(buf).expect("kernel randomness");
}

pub fn random_u64() -> u64 {
    let mut b = [0u8; 8];
    fill_random(&mut b);
    u64::from_ne_bytes(b)
}

/// A new X25519 private key (node WireGuard key, relay key).
pub fn new_secret() -> x25519::StaticSecret {
    let mut k = [0u8; 32];
    fill_random(&mut k);
    x25519::StaticSecret::from(k)
}

pub fn read_secret(path: &str) -> io::Result<x25519::StaticSecret> {
    let s = std::fs::read_to_string(path).map_err(|e| io::Error::new(e.kind(), format!("{path}: {e}")))?;
    Ok(x25519::StaticSecret::from(decode_key(&s)?))
}

/// Wall clock in nanoseconds since the Unix epoch: HELLO timestamps, so they must agree
/// between machines (NTP), and stay strictly increasing per node.
pub fn now_nanos() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

pub fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}
