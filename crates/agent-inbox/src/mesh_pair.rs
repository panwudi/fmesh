//! Derive directed peer MAC keys from signed host X25519 identities.

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD_NO_PAD;
use ring::digest::{SHA256, digest};
use ring::hkdf::{HKDF_SHA256, Salt};
use ring::hmac;
use serde::{Deserialize, Serialize};
use x25519_dalek::{PublicKey, StaticSecret};

use crate::{Result, hub, state};

const DOMAIN: &[u8] = b"agent-inbox-mesh-pair-v1\0";
const REQUEST_DOMAIN: &[u8] = b"agent-inbox-mesh-request-v1\0";
const CLOCK_SKEW_SECS: i64 = 120;
const REPLAY_WINDOW_SECS: i64 = 300;
const MAX_SEEN_NONCES: usize = 4096;
const MAX_PEER_NONCES: usize = 256;

pub struct HostSecret(StaticSecret);

pub struct PairKeys {
    network: String,
    own_host: String,
    peer_host: String,
    epoch: u64,
    outbound: hmac::Key,
    inbound: hmac::Key,
}

#[derive(Clone, Copy)]
pub struct MeshRequest<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub body: &'a [u8],
    pub from: &'a str,
}

pub struct ReplayGuard {
    path: PathBuf,
    state: Mutex<ReplayState>,
    _lock: File,
}

#[derive(Clone, Deserialize, Serialize)]
struct ReplayState {
    version: u32,
    entries: VecDeque<NonceEntry>,
}

#[derive(Clone, Deserialize, Serialize)]
struct NonceEntry {
    peer: String,
    nonce: [u8; 16],
    expires: i64,
}

impl HostSecret {
    pub fn generate(path: &Path) -> Result<Self> {
        state::parent(path)?;
        let mut bytes = [0u8; 32];
        File::open("/dev/urandom")?.read_exact(&mut bytes)?;
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let mut file = options.open(path)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        if let Some(parent) = path.parent() {
            File::open(parent)?.sync_all()?;
        }
        let secret = Self(StaticSecret::from(bytes));
        bytes.fill(0);
        Ok(secret)
    }

    pub fn read(path: &Path) -> Result<Self> {
        let mut file = state::open_private(path, false)?;
        if file.metadata()?.len() != 32 {
            return Err("inbox host private key must be 32 bytes".into());
        }
        let mut bytes = [0u8; 32];
        file.read_exact(&mut bytes)?;
        let secret = Self(StaticSecret::from(bytes));
        bytes.fill(0);
        Ok(secret)
    }

    pub fn public_key(&self) -> [u8; 32] {
        PublicKey::from(&self.0).to_bytes()
    }

    pub fn derive_keys(
        &self,
        network: &str,
        own_host: &str,
        peer_host: &str,
        peer_key: [u8; 32],
        epoch: u64,
    ) -> Result<PairKeys> {
        hub::validate_component(network)?;
        hub::validate_component(own_host)?;
        hub::validate_component(peer_host)?;
        if own_host == peer_host || peer_key == self.public_key() || epoch == 0 {
            return Err("peer identity must differ from this host".into());
        }
        let shared = self.0.diffie_hellman(&PublicKey::from(peer_key));
        if !shared.was_contributory() {
            return Err("invalid low-order peer key".into());
        }
        let mut info = DOMAIN.to_vec();
        append_part(&mut info, network.as_bytes());
        let own_key = self.public_key();
        let pairs = if own_host < peer_host {
            [(own_host, own_key), (peer_host, peer_key)]
        } else {
            [(peer_host, peer_key), (own_host, own_key)]
        };
        for (host, key) in pairs {
            append_part(&mut info, host.as_bytes());
            append_part(&mut info, &key);
        }
        append_part(&mut info, &epoch.to_be_bytes());
        let prk = Salt::new(HKDF_SHA256, DOMAIN).extract(shared.as_bytes());
        let directional = |from: &str, to: &str| -> Result<hmac::Key> {
            let mut context = info.clone();
            append_part(&mut context, from.as_bytes());
            append_part(&mut context, to.as_bytes());
            let parts = [&context[..]];
            let okm = prk
                .expand(&parts, HKDF_SHA256)
                .map_err(|_| "cannot expand directed peer key")?;
            let mut bytes = [0u8; 32];
            okm.fill(&mut bytes)
                .map_err(|_| "cannot fill directed peer key")?;
            let key = hmac::Key::new(hmac::HMAC_SHA256, &bytes);
            bytes.fill(0);
            Ok(key)
        };
        Ok(PairKeys {
            network: network.to_owned(),
            own_host: own_host.to_owned(),
            peer_host: peer_host.to_owned(),
            epoch,
            outbound: directional(own_host, peer_host)?,
            inbound: directional(peer_host, own_host)?,
        })
    }
}

fn append_part(output: &mut Vec<u8>, part: &[u8]) {
    output.extend_from_slice(&(part.len() as u16).to_be_bytes());
    output.extend_from_slice(part);
}

fn request_data(
    network: &str,
    epoch: u64,
    to: &str,
    request: MeshRequest<'_>,
    timestamp: i64,
    nonce: &[u8; 16],
) -> Vec<u8> {
    let mut data = REQUEST_DOMAIN.to_vec();
    for part in [
        network.as_bytes(),
        &epoch.to_be_bytes(),
        request.from.as_bytes(),
        to.as_bytes(),
        request.method.as_bytes(),
        request.path.as_bytes(),
        digest(&SHA256, request.body).as_ref(),
        &timestamp.to_be_bytes(),
        nonce,
    ] {
        append_part(&mut data, part);
    }
    data
}

impl PairKeys {
    pub fn sign_request(&self, request: MeshRequest<'_>, now: i64) -> Result<String> {
        let mut nonce = [0u8; 16];
        File::open("/dev/urandom")?.read_exact(&mut nonce)?;
        self.sign_with_nonce(request, now, nonce)
    }

    fn sign_with_nonce(
        &self,
        request: MeshRequest<'_>,
        now: i64,
        nonce: [u8; 16],
    ) -> Result<String> {
        if hub::qualified(request.from)?.0 != self.own_host {
            return Err("mesh request sender is outside the local host".into());
        }
        let data = request_data(
            &self.network,
            self.epoch,
            &self.peer_host,
            request,
            now,
            &nonce,
        );
        let tag = hmac::sign(&self.outbound, &data);
        Ok(format!(
            "MeshMAC {} {} {} {} {}",
            self.own_host,
            self.epoch,
            now,
            STANDARD_NO_PAD.encode(nonce),
            STANDARD_NO_PAD.encode(tag.as_ref())
        ))
    }

    pub fn verify_request(
        &self,
        header: &[u8],
        request: MeshRequest<'_>,
        now: i64,
        replay: &ReplayGuard,
    ) -> Result<()> {
        if hub::qualified(request.from)?.0 != self.peer_host {
            return Err("mesh request sender is outside the authenticated host".into());
        }
        let header = std::str::from_utf8(header).map_err(|_| "invalid mesh MAC header")?;
        let words: Vec<_> = header.split_whitespace().collect();
        let ["MeshMAC", claimed_host, epoch, timestamp, nonce, mac] = words.as_slice() else {
            return Err("invalid mesh MAC header".into());
        };
        let epoch: u64 = epoch.parse()?;
        let timestamp: i64 = timestamp.parse()?;
        if *claimed_host != self.peer_host
            || epoch != self.epoch
            || now.abs_diff(timestamp) > CLOCK_SKEW_SECS as u64
        {
            return Err("mesh MAC host, epoch or time mismatch".into());
        }
        let nonce: [u8; 16] = STANDARD_NO_PAD
            .decode(nonce)
            .map_err(|_| "invalid mesh MAC nonce")?
            .try_into()
            .map_err(|_| "invalid mesh MAC nonce length")?;
        let mac = STANDARD_NO_PAD
            .decode(mac)
            .map_err(|_| "invalid mesh MAC encoding")?;
        if mac.len() != 32 {
            return Err("invalid mesh MAC length".into());
        }
        let data = request_data(
            &self.network,
            self.epoch,
            &self.own_host,
            request,
            timestamp,
            &nonce,
        );
        hmac::verify(&self.inbound, &data, &mac).map_err(|_| "mesh MAC did not verify")?;
        replay.accept(&self.peer_host, nonce, timestamp, now)
    }
}

impl ReplayGuard {
    pub fn open(path: PathBuf) -> Result<Self> {
        state::parent(&path)?;
        let lock = state::open_private(&path.with_extension("replay-lock"), true)?;
        lock.try_lock()
            .map_err(|_| "another process owns the mesh replay cache")?;
        let state = if path.exists() {
            let saved: ReplayState = state::read_json(&path)?;
            if saved.version != 1 || saved.entries.len() > MAX_SEEN_NONCES {
                return Err("invalid mesh replay cache".into());
            }
            saved
        } else {
            let initial = ReplayState {
                version: 1,
                entries: VecDeque::new(),
            };
            state::write_json(&path, &initial)?;
            initial
        };
        Ok(Self {
            path,
            state: Mutex::new(state),
            _lock: lock,
        })
    }

    fn accept(&self, peer: &str, nonce: [u8; 16], timestamp: i64, now: i64) -> Result<()> {
        let mut current = self.state.lock().map_err(|_| "mesh replay lock failed")?;
        let mut next = current.clone();
        next.entries.retain(|entry| entry.expires >= now);
        if next
            .entries
            .iter()
            .any(|entry| entry.peer == peer && entry.nonce == nonce)
        {
            return Err("mesh request nonce was already used".into());
        }
        if next
            .entries
            .iter()
            .filter(|entry| entry.peer == peer)
            .count()
            >= MAX_PEER_NONCES
        {
            return Err("mesh peer replay cache is full".into());
        }
        if next.entries.len() >= MAX_SEEN_NONCES {
            return Err("mesh replay cache is full".into());
        }
        next.entries.push_back(NonceEntry {
            peer: peer.to_owned(),
            nonce,
            expires: timestamp.saturating_add(REPLAY_WINDOW_SECS),
        });
        state::write_json(&self.path, &next)?;
        *current = next;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_peer_cannot_exhaust_the_replay_cache_for_other_peers() {
        let path = std::env::temp_dir().join(format!("mesh-peer-cap-{}", state::id().unwrap()));
        let entries = (0..MAX_PEER_NONCES)
            .map(|index| NonceEntry {
                peer: "alpha".into(),
                nonce: (index as u128).to_be_bytes(),
                expires: 1000,
            })
            .collect();
        state::write_json(
            &path,
            &ReplayState {
                version: 1,
                entries,
            },
        )
        .unwrap();
        let guard = ReplayGuard::open(path.clone()).unwrap();
        assert!(guard.accept("alpha", [255; 16], 900, 900).is_err());
        assert!(guard.accept("beta", [255; 16], 900, 900).is_ok());
        drop(guard);
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_file(path.with_extension("replay-lock")).unwrap();
    }

    #[test]
    fn directed_keys_authenticate_requests_without_pair_files() {
        let alpha = HostSecret(StaticSecret::from([1u8; 32]));
        let beta = HostSecret(StaticSecret::from([2u8; 32]));
        let gamma = HostSecret(StaticSecret::from([3u8; 32]));
        let ab = alpha
            .derive_keys("team", "alpha", "beta", beta.public_key(), 1)
            .unwrap();
        let ba = beta
            .derive_keys("team", "beta", "alpha", alpha.public_key(), 1)
            .unwrap();
        let ac = alpha
            .derive_keys("team", "alpha", "gamma", gamma.public_key(), 1)
            .unwrap();
        let ab_next = alpha
            .derive_keys("team", "alpha", "beta", beta.public_key(), 2)
            .unwrap();
        let temp = std::env::temp_dir().join(format!("mesh-replay-{}", state::id().unwrap()));
        let guard = ReplayGuard::open(temp.clone()).unwrap();
        let request = MeshRequest {
            method: "POST",
            path: "/mesh/directory",
            body: b"{}",
            from: "alpha.agent",
        };
        let header = ab.sign_with_nonce(request, 1000, [7; 16]).unwrap();
        assert!(
            ba.verify_request(header.as_bytes(), request, 1000, &guard)
                .is_ok()
        );
        assert!(
            ba.verify_request(header.as_bytes(), request, 1000, &guard)
                .is_err()
        );
        assert!(
            ba.verify_request(
                header.as_bytes(),
                MeshRequest {
                    body: b"changed",
                    ..request
                },
                1000,
                &guard,
            )
            .is_err()
        );
        assert!(
            ac.verify_request(header.as_bytes(), request, 1000, &guard)
                .is_err()
        );
        let rotated_header = ab_next.sign_with_nonce(request, 1000, [8; 16]).unwrap();
        assert!(
            ba.verify_request(rotated_header.as_bytes(), request, 1000, &guard)
                .is_err()
        );
        drop(guard);
        let reopened = ReplayGuard::open(temp.clone()).unwrap();
        assert!(
            ba.verify_request(header.as_bytes(), request, 1000, &reopened)
                .is_err()
        );
        drop(reopened);
        let _ = std::fs::remove_file(&temp);
        let _ = std::fs::remove_file(temp.with_extension("replay-lock"));
        assert!(
            alpha
                .derive_keys("team", "alpha", "beta", [0u8; 32], 1)
                .is_err()
        );
    }
}
