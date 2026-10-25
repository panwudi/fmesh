//! The signed roster: who is in the mesh. It replaces a central database.
//!
//! ```text
//! flytomesh-roster 1
//! version 7
//! relay sh 203.0.113.10:41900 <relay X25519 public key>
//! node 1 laptop1 10.249.0.1 <node WireGuard public key>
//! sig <Ed25519 signature over every byte before this line>
//! ```
//!
//! The admin Ed25519 key lives only on the admin machine; nodes and relays hold its public
//! key and accept only a roster that verifies and has a larger version than the one they have.

use crate::{b64, decode_key, invalid};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use ring::signature::{Ed25519KeyPair, KeyPair, UnparsedPublicKey, ED25519};
use std::io;
use std::net::{Ipv4Addr, SocketAddrV4};

const MAGIC: &str = "flytomesh-roster 1";

#[derive(Clone, Debug)]
pub struct Relay {
    pub name: String,
    pub addr: SocketAddrV4,
    pub key: [u8; 32],
}

#[derive(Clone, Debug)]
pub struct Node {
    /// Small number used on the wire (relay header, WireGuard session index). Never reused.
    pub id: u16,
    pub name: String,
    pub ip: Ipv4Addr,
    pub key: [u8; 32],
}

#[derive(Clone, Debug)]
pub struct Roster {
    pub version: u64,
    pub relays: Vec<Relay>,
    pub nodes: Vec<Node>,
    /// The exact signed bytes, as served to nodes that fetch the roster.
    pub raw: Vec<u8>,
}

impl Roster {
    /// Parses a roster and checks its signature against the admin public key.
    pub fn parse_verified(raw: &[u8], admin_pub: &[u8; 32]) -> io::Result<Roster> {
        let text = std::str::from_utf8(raw).map_err(|_| invalid("roster is not UTF-8"))?;
        let sig_at = text.rfind("\nsig ").ok_or_else(|| invalid("roster has no sig line"))? + 1;
        let sig = B64
            .decode(text[sig_at + 4..].trim())
            .map_err(|_| invalid("roster sig is not base64"))?;
        UnparsedPublicKey::new(&ED25519, admin_pub)
            .verify(&raw[..sig_at], &sig)
            .map_err(|_| invalid("roster signature does not verify"))?;
        let mut r = parse_body(&text[..sig_at])?;
        r.raw = raw.to_vec();
        Ok(r)
    }

    pub fn node_by_key(&self, key: &[u8; 32]) -> Option<&Node> {
        self.nodes.iter().find(|n| &n.key == key)
    }

    pub fn relay_by_key(&self, key: &[u8; 32]) -> Option<&Relay> {
        self.relays.iter().find(|r| &r.key == key)
    }
}

/// Parses the unsigned part. Also used by `sign` to refuse signing a malformed body.
fn parse_body(text: &str) -> io::Result<Roster> {
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#'));
    if lines.next() != Some(MAGIC) {
        return Err(invalid("roster does not start with 'flytomesh-roster 1'"));
    }
    let mut r = Roster { version: 0, relays: vec![], nodes: vec![], raw: vec![] };
    for line in lines {
        let f: Vec<&str> = line.split_whitespace().collect();
        let bad = || invalid(&format!("bad roster line: {line}"));
        match f.as_slice() {
            ["version", v] => r.version = v.parse().map_err(|_| bad())?,
            ["relay", name, addr, key] => r.relays.push(Relay {
                name: name.to_string(),
                addr: addr.parse().map_err(|_| bad())?,
                key: decode_key(key)?,
            }),
            ["node", id, name, ip, key] => r.nodes.push(Node {
                id: id.parse().map_err(|_| bad())?,
                name: name.to_string(),
                ip: ip.parse().map_err(|_| bad())?,
                key: decode_key(key)?,
            }),
            _ => return Err(bad()),
        }
    }
    if r.version == 0 {
        return Err(invalid("roster has no version"));
    }
    for (i, a) in r.nodes.iter().enumerate() {
        if a.id == 0 {
            return Err(invalid("node id 0 is reserved"));
        }
        for b in &r.nodes[i + 1..] {
            if a.id == b.id || a.name == b.name || a.ip == b.ip || a.key == b.key {
                return Err(invalid(&format!("nodes {} and {} share an id, name, ip or key", a.name, b.name)));
            }
        }
    }
    Ok(r)
}

/// Signs a roster body (everything but the sig line) with the admin key seed.
pub fn sign(body: &str, admin_seed: &[u8; 32]) -> io::Result<String> {
    parse_body(body)?;
    let kp = Ed25519KeyPair::from_seed_unchecked(admin_seed).map_err(|_| invalid("bad admin key"))?;
    let mut body = body.to_string();
    if !body.ends_with('\n') {
        body.push('\n');
    }
    let sig = kp.sign(body.as_bytes());
    Ok(format!("{body}sig {}\n", b64(sig.as_ref())))
}

pub fn admin_public(admin_seed: &[u8; 32]) -> io::Result<[u8; 32]> {
    let kp = Ed25519KeyPair::from_seed_unchecked(admin_seed).map_err(|_| invalid("bad admin key"))?;
    Ok(kp.public_key().as_ref().try_into().expect("ed25519 public key is 32 bytes"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const BODY: &str = "flytomesh-roster 1\nversion 3\n\
        relay hk 203.0.113.20:41900 AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=\n\
        node 1 laptop1 10.249.0.1 AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=\n\
        node 2 laptop2 10.249.0.2 AgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgI=\n";

    #[test]
    fn sign_then_verify() {
        let seed = [7u8; 32];
        let signed = sign(BODY, &seed).unwrap();
        let r = Roster::parse_verified(signed.as_bytes(), &admin_public(&seed).unwrap()).unwrap();
        assert_eq!(r.version, 3);
        assert_eq!(r.nodes.len(), 2);
        assert_eq!(r.relays[0].addr.port(), 41900);
    }

    #[test]
    fn tampering_breaks_the_signature() {
        let seed = [7u8; 32];
        let signed = sign(BODY, &seed).unwrap().replace("10.249.0.2", "10.249.0.9");
        assert!(Roster::parse_verified(signed.as_bytes(), &admin_public(&seed).unwrap()).is_err());
    }

    #[test]
    fn duplicate_ids_are_refused() {
        let body = BODY.replace("node 2 laptop2", "node 1 laptop2");
        assert!(sign(&body, &[7u8; 32]).is_err());
    }
}
