//! Packet formats. Everything shares the node's single UDP port, so the first bytes decide:
//! WireGuard messages start with a little-endian u32 type 1-4 (so byte 0 is 1-4 and bytes
//! 1-3 are zero); our control messages start with `MAGIC` and a type byte.
//!
//! Every control message has a 4-byte clear header `[MAGIC, type, u16 big-endian]`; the u16
//! is a node id (sender, destination or source, depending on the type) or zero.

use crate::crypto::PairKey;
use std::net::{Ipv4Addr, SocketAddrV4};

pub const MAGIC: u8 = 0xFA;
pub const HDR: usize = 4;

/// node -> relay: `[id][sealed: ts u64, roster version u64]`. Register / keep the NAT open.
pub const HELLO: u8 = 0x01;
/// relay -> node: `[0][sealed: echoed ts u64, node's public addr, relay's roster version u64,
/// release version u64, release manifest sha256 32]` (the release part is absent when the
/// relay has no release; older nodes ignore it).
pub const HELLO_ACK: u8 = 0x02;
/// node -> relay: `[dst id][payload]`. Forward the payload to node `dst`.
pub const FWD: u8 = 0x03;
/// relay -> node: `[src id][payload]`. A payload forwarded from node `src`.
pub const FWD_FROM: u8 = 0x04;
/// node -> relay: `[0][offset u32]`. Fetch the roster from `offset`.
pub const ROSTER_GET: u8 = 0x05;
/// relay -> node: `[0][total u32][offset u32][bytes]`.
pub const ROSTER_PART: u8 = 0x06;
/// relay -> node: `[dst id][sealed: ts u64]`. `dst` has not registered with this relay; try
/// another. `ts` echoes the sender's last accepted HELLO, so a forged or old NO_ROUTE cannot
/// move a node off a working relay. Releases before v9 send the bare header and ignore the rest.
pub const NO_ROUTE: u8 = 0x07;
/// node -> relay: `[0][sha256 32][offset u32][count u8]`. Fetch up to `count` chunks of the
/// blob with that SHA-256 (the release manifest or a binary), starting at `offset`.
pub const BLOB_GET: u8 = 0x08;
/// relay -> node: `[0][sha256 32][total u32][offset u32][bytes]`.
pub const BLOB_PART: u8 = 0x09;
/// Most chunks answered per BLOB_GET (registered nodes only, within the relay's per-node budget).
pub const BLOB_WINDOW: u8 = 16;
/// node <-> node, direct or inside FWD: `[src id][sealed: kind u8, body]`.
pub const DISCO: u8 = 0x10;

/// Roster bytes per ROSTER_PART, well under any path MTU.
pub const ROSTER_CHUNK: usize = 1200;

/// DISCO kinds. CALL body: `[n u8][n x addr (ip4, port)][m u8][m x (relay index u8, rtt ms u16)]`,
/// the sender's candidate addresses and its RTT to each live relay (relay index = position
/// in the roster). PING body: `[txid u64]`. PONG body: `[txid u64][addr the ping came from]`.
/// SERVICES body: see `crate::service` (older nodes ignore unknown kinds).
pub const CALL: u8 = 1;
pub const PING: u8 = 2;
pub const PONG: u8 = 3;
pub const SERVICES: u8 = 4;

pub fn is_wireguard(p: &[u8]) -> bool {
    p.len() >= 4 && (1..=4).contains(&p[0]) && p[1..4] == [0, 0, 0]
}

/// `(type, u16 field)` of a control message.
pub fn control(p: &[u8]) -> Option<(u8, u16)> {
    if p.len() >= HDR && p[0] == MAGIC {
        Some((p[1], u16::from_be_bytes([p[2], p[3]])))
    } else {
        None
    }
}

pub fn header(kind: u8, field: u16) -> [u8; HDR] {
    let f = field.to_be_bytes();
    [MAGIC, kind, f[0], f[1]]
}

pub fn put_addr(out: &mut Vec<u8>, a: SocketAddrV4) {
    out.extend_from_slice(&a.ip().octets());
    out.extend_from_slice(&a.port().to_be_bytes());
}

pub fn get_addr(b: &[u8]) -> Option<SocketAddrV4> {
    if b.len() < 6 {
        return None;
    }
    Some(SocketAddrV4::new(Ipv4Addr::new(b[0], b[1], b[2], b[3]), u16::from_be_bytes([b[4], b[5]])))
}

pub fn get_u64(b: &[u8]) -> Option<u64> {
    Some(u64::from_be_bytes(b.get(..8)?.try_into().ok()?))
}

pub fn get_u32(b: &[u8]) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(..4)?.try_into().ok()?))
}

/// Builds a sealed control message: clear header, then `plain` encrypted with `key`.
pub fn sealed(kind: u8, field: u16, key: &PairKey, plain: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HDR + plain.len() + crate::crypto::OVERHEAD);
    out.extend_from_slice(&header(kind, field));
    key.seal(&mut out, plain);
    out
}

/// Opens a sealed control message in place; returns the plaintext.
pub fn open<'a>(key: &PairKey, msg: &'a mut [u8]) -> Option<&'a mut [u8]> {
    if msg.len() < HDR {
        return None;
    }
    let (hdr, rest) = msg.split_at_mut(HDR);
    key.open(hdr, rest)
}

/// A decrypted HELLO_ACK.
#[derive(Debug, PartialEq)]
pub struct HelloAck {
    pub ts: u64,
    pub public: SocketAddrV4,
    pub roster_version: u64,
    /// Release version and manifest SHA-256, when the relay publishes one.
    pub release: Option<(u64, [u8; 32])>,
}

/// Parses a HELLO_ACK plaintext. Short or malformed input is `None`, never a panic.
pub fn parse_hello_ack(plain: &[u8]) -> Option<HelloAck> {
    let ts = get_u64(plain)?;
    let public = get_addr(plain.get(8..)?)?;
    let roster_version = get_u64(plain.get(14..)?)?;
    let release = plain.get(22..62).and_then(|b| Some((get_u64(b)?, <[u8; 32]>::try_from(&b[8..40]).ok()?)));
    Some(HelloAck { ts, public, roster_version, release })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ack(release: bool) -> Vec<u8> {
        let mut b = 7u64.to_be_bytes().to_vec();
        put_addr(&mut b, SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 1), 41820));
        b.extend_from_slice(&3u64.to_be_bytes());
        if release {
            b.extend_from_slice(&9u64.to_be_bytes());
            b.extend_from_slice(&[0xAB; 32]);
        }
        b
    }

    #[test]
    fn hello_ack_parses_with_and_without_release() {
        let a = parse_hello_ack(&ack(false)).unwrap();
        assert_eq!((a.ts, a.roster_version, a.release), (7, 3, None));
        assert_eq!(a.public, SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 1), 41820));
        assert_eq!(parse_hello_ack(&ack(true)).unwrap().release, Some((9, [0xAB; 32])));
    }

    #[test]
    fn short_hello_ack_is_rejected_without_panicking() {
        let full = ack(true);
        for cut in 0..22 {
            assert!(parse_hello_ack(&full[..cut]).is_none(), "accepted {cut} bytes");
        }
        // A truncated release part is ignored, not an error.
        assert_eq!(parse_hello_ack(&full[..40]).unwrap().release, None);
    }
}
