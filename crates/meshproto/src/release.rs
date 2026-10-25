//! Signed release manifest: which meshd binary each platform should run. Nodes update
//! themselves from it through the relays, so a new version is published once, not installed
//! machine by machine.
//!
//! ```text
//! flytomesh-release 1
//! version 3
//! meshd macos-aarch64 <sha256 hex> <size>
//! meshd linux-x86_64 <sha256 hex> <size>
//! sig <Ed25519 signature over every byte before this line>
//! ```
//!
//! Signed with the same admin key as the roster. A node takes a binary only if the manifest
//! verifies and the downloaded bytes hash to the listed SHA-256, so relays cannot alter it.

use crate::{b64, invalid};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use ring::signature::{Ed25519KeyPair, UnparsedPublicKey, ED25519};
use std::io;

const MAGIC: &str = "flytomesh-release 1";

/// This build's platform name, as used in the manifest: `<os>-<arch>`.
pub fn platform() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

pub fn sha256(data: &[u8]) -> [u8; 32] {
    ring::digest::digest(&ring::digest::SHA256, data).as_ref().try_into().expect("32 bytes")
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex32(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

#[derive(Clone, Debug)]
pub struct Binary {
    pub platform: String,
    pub sha256: [u8; 32],
    pub size: u64,
}

#[derive(Clone, Debug)]
pub struct Release {
    pub version: u64,
    pub meshd: Vec<Binary>,
}

impl Release {
    pub fn parse_verified(raw: &[u8], admin_pub: &[u8; 32]) -> io::Result<Release> {
        let text = std::str::from_utf8(raw).map_err(|_| invalid("release is not UTF-8"))?;
        let sig_at = text.rfind("\nsig ").ok_or_else(|| invalid("release has no sig line"))? + 1;
        let sig = B64
            .decode(text[sig_at + 4..].trim())
            .map_err(|_| invalid("release sig is not base64"))?;
        UnparsedPublicKey::new(&ED25519, admin_pub)
            .verify(&raw[..sig_at], &sig)
            .map_err(|_| invalid("release signature does not verify"))?;
        parse_body(&text[..sig_at])
    }

    pub fn for_platform(&self, platform: &str) -> Option<&Binary> {
        self.meshd.iter().find(|b| b.platform == platform)
    }
}

fn parse_body(text: &str) -> io::Result<Release> {
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#'));
    if lines.next() != Some(MAGIC) {
        return Err(invalid("release does not start with 'flytomesh-release 1'"));
    }
    let mut r = Release { version: 0, meshd: vec![] };
    for line in lines {
        let f: Vec<&str> = line.split_whitespace().collect();
        let bad = || invalid(&format!("bad release line: {line}"));
        match f.as_slice() {
            ["version", v] => r.version = v.parse().map_err(|_| bad())?,
            ["meshd", platform, hash, size] => r.meshd.push(Binary {
                platform: platform.to_string(),
                sha256: unhex32(hash).ok_or_else(bad)?,
                size: size.parse().map_err(|_| bad())?,
            }),
            _ => return Err(bad()),
        }
    }
    if r.version == 0 {
        return Err(invalid("release has no version"));
    }
    Ok(r)
}

/// Signs a release body (everything but the sig line) with the admin key seed.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_verify_and_lookup() {
        let seed = [9u8; 32];
        let h = hex(&sha256(b"binary"));
        let body = format!("flytomesh-release 1\nversion 4\nmeshd linux-x86_64 {h} 6\n");
        let signed = sign(&body, &seed).unwrap();
        let admin = crate::roster::admin_public(&seed).unwrap();
        let r = Release::parse_verified(signed.as_bytes(), &admin).unwrap();
        assert_eq!(r.version, 4);
        assert_eq!(r.for_platform("linux-x86_64").unwrap().sha256, sha256(b"binary"));
        assert!(Release::parse_verified(signed.replace("version 4", "version 5").as_bytes(), &admin).is_err());
    }
}
