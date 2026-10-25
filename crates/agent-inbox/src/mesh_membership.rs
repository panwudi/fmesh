//! Offline-signed host membership for experimental dynamic peer discovery.

use std::collections::HashSet;
use std::fs::File;
use std::path::PathBuf;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD_NO_PAD;
use ring::digest::{SHA256, digest};
use ring::signature::{ED25519, UnparsedPublicKey};
use serde::{Deserialize, Serialize};
use x25519_dalek::{PublicKey, StaticSecret};

use crate::{Result, hub, state};

const DOMAIN: &[u8] = b"agent-inbox-membership-v1\0";
const MAX_ENVELOPE_BYTES: usize = 64 * 1024;
const MAX_MEMBERS: usize = 256;
const MAX_AUTHORITIES: usize = 9;
const MAX_SIGNATURES: usize = 18;
const MAX_VALIDITY_SECS: i64 = 30 * 24 * 60 * 60;
const CLOCK_SKEW_SECS: i64 = 120;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AuthoritySet {
    pub threshold: usize,
    pub keys: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Member {
    pub host: String,
    pub mesh_node_id: Option<u16>,
    pub transport_key: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub version: u32,
    pub network: String,
    pub revision: u64,
    pub token_epoch: u64,
    pub not_before: i64,
    pub expires: i64,
    pub authorities: AuthoritySet,
    pub members: Vec<Member>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestSignature {
    pub public_key: String,
    pub signature: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Envelope {
    pub payload: String,
    pub signatures: Vec<ManifestSignature>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    network: String,
    initial_authorities_hash: String,
    highest_revision: u64,
    payload_hash: Option<String>,
    authorities: AuthoritySet,
    manifest: Option<Manifest>,
    envelope: Option<Envelope>,
}

pub struct Store {
    path: PathBuf,
    snapshot: Snapshot,
    _lock: File,
}

fn decode_key(encoded: &str) -> Result<Vec<u8>> {
    let key = STANDARD_NO_PAD
        .decode(encoded)
        .map_err(|_| "invalid membership public key encoding")?;
    if key.len() != 32 {
        return Err("membership public key must be Ed25519".into());
    }
    Ok(key)
}

fn validate_authorities(value: &AuthoritySet) -> Result<()> {
    if value.keys.is_empty()
        || value.keys.len() > MAX_AUTHORITIES
        || value.threshold == 0
        || value.threshold > value.keys.len()
    {
        return Err("invalid membership signature threshold".into());
    }
    let mut previous = None;
    for key in &value.keys {
        decode_key(key)?;
        if previous.is_some_and(|prior: &String| prior >= key) {
            return Err("membership authority keys must be unique and sorted".into());
        }
        previous = Some(key);
    }
    Ok(())
}

fn validate_manifest(value: &Manifest, network: &str, now: i64) -> Result<()> {
    if value.version != 1
        || value.network != network
        || value.revision == 0
        || value.token_epoch == 0
    {
        return Err("invalid membership network, version or revision".into());
    }
    if value.not_before > now.saturating_add(CLOCK_SKEW_SECS)
        || value.expires <= now.saturating_sub(CLOCK_SKEW_SECS)
        || value.expires <= value.not_before
        || value.expires.saturating_sub(value.not_before) > MAX_VALIDITY_SECS
    {
        return Err("membership manifest is expired or outside its validity window".into());
    }
    validate_authorities(&value.authorities)?;
    if value.members.is_empty() || value.members.len() > MAX_MEMBERS {
        return Err("invalid membership member count".into());
    }
    let mut hosts = HashSet::new();
    let mut ids = HashSet::new();
    let mut keys = HashSet::new();
    for member in &value.members {
        hub::validate_component(&member.host)?;
        let key: [u8; 32] = decode_key(&member.transport_key)?
            .try_into()
            .map_err(|_| "invalid transport key length")?;
        if !StaticSecret::from([1u8; 32])
            .diffie_hellman(&PublicKey::from(key))
            .was_contributory()
        {
            return Err("membership transport key is low order".into());
        }
        if !hosts.insert(&member.host)
            || !keys.insert(&member.transport_key)
            || member
                .mesh_node_id
                .is_some_and(|id| id == 0 || !ids.insert(id))
        {
            return Err("duplicate or invalid membership identity".into());
        }
    }
    Ok(())
}

fn authority_hash(value: &AuthoritySet) -> Result<String> {
    validate_authorities(value)?;
    let encoded = serde_json::to_vec(value)?;
    Ok(STANDARD_NO_PAD.encode(digest(&SHA256, &encoded).as_ref()))
}

fn count_signatures(
    envelope: &Envelope,
    signed: &[u8],
    current: &AuthoritySet,
    next: &AuthoritySet,
) -> Result<(usize, usize)> {
    if envelope.signatures.len() > MAX_SIGNATURES {
        return Err("too many membership signatures".into());
    }
    let mut seen = HashSet::new();
    let mut old_count = 0;
    let mut new_count = 0;
    for signature in &envelope.signatures {
        if !seen.insert(&signature.public_key) {
            return Err("duplicate membership signature".into());
        }
        let old = current.keys.contains(&signature.public_key);
        let new = next.keys.contains(&signature.public_key);
        if !old && !new {
            return Err("membership signature has an unknown authority".into());
        }
        let key = decode_key(&signature.public_key)?;
        let bytes = STANDARD_NO_PAD
            .decode(&signature.signature)
            .map_err(|_| "invalid membership signature encoding")?;
        UnparsedPublicKey::new(&ED25519, key)
            .verify(signed, &bytes)
            .map_err(|_| "invalid membership signature")?;
        old_count += usize::from(old);
        new_count += usize::from(new);
    }
    Ok((old_count, new_count))
}

impl Store {
    pub fn open(path: PathBuf, network: String, initial: AuthoritySet) -> Result<Self> {
        hub::validate_component(&network)?;
        let anchor = authority_hash(&initial)?;
        state::parent(&path)?;
        let lock = state::open_private(&path.with_extension("membership-lock"), true)?;
        lock.try_lock()
            .map_err(|_| "another process owns this membership state")?;
        let snapshot = if path.exists() {
            let snapshot: Snapshot = state::read_json(&path)?;
            if snapshot.network != network || snapshot.initial_authorities_hash != anchor {
                return Err("membership state trust anchor differs from provisioning".into());
            }
            validate_authorities(&snapshot.authorities)?;
            if (snapshot.highest_revision == 0
                && (snapshot.manifest.is_some()
                    || snapshot.payload_hash.is_some()
                    || snapshot.envelope.is_some()
                    || snapshot.authorities != initial))
                || (snapshot.highest_revision > 0
                    && (snapshot.payload_hash.is_none()
                        || snapshot.envelope.is_none()
                        || snapshot.manifest.as_ref().is_none_or(|manifest| {
                            manifest.revision != snapshot.highest_revision
                                || manifest.network != snapshot.network
                                || manifest.authorities != snapshot.authorities
                        })))
            {
                return Err("invalid persisted membership state".into());
            }
            snapshot
        } else {
            let snapshot = Snapshot {
                network,
                initial_authorities_hash: anchor,
                highest_revision: 0,
                payload_hash: None,
                authorities: initial,
                manifest: None,
                envelope: None,
            };
            state::write_json(&path, &snapshot)?;
            snapshot
        };
        Ok(Self {
            path,
            snapshot,
            _lock: lock,
        })
    }

    pub fn accept(&mut self, envelope: &Envelope, now: i64) -> Result<bool> {
        if serde_json::to_vec(envelope)?.len() > MAX_ENVELOPE_BYTES {
            return Err("membership envelope exceeds 64 KiB".into());
        }
        let payload = STANDARD_NO_PAD
            .decode(&envelope.payload)
            .map_err(|_| "invalid membership payload encoding")?;
        if payload.len() > MAX_ENVELOPE_BYTES {
            return Err("membership payload exceeds 64 KiB".into());
        }
        let manifest: Manifest =
            serde_json::from_slice(&payload).map_err(|_| "invalid membership manifest payload")?;
        validate_manifest(&manifest, &self.snapshot.network, now)?;
        if manifest.revision < self.snapshot.highest_revision {
            return Err("older signed membership revision rejected".into());
        }
        let hash = STANDARD_NO_PAD.encode(digest(&SHA256, &payload).as_ref());
        let mut signed = Vec::with_capacity(DOMAIN.len() + payload.len());
        signed.extend_from_slice(DOMAIN);
        signed.extend_from_slice(&payload);
        let (old, new) = count_signatures(
            envelope,
            &signed,
            &self.snapshot.authorities,
            &manifest.authorities,
        )?;
        if old < self.snapshot.authorities.threshold
            || (manifest.authorities != self.snapshot.authorities
                && new < manifest.authorities.threshold)
        {
            return Err("membership signature quorum not met".into());
        }
        if manifest.revision == self.snapshot.highest_revision {
            if self.snapshot.payload_hash.as_deref() == Some(&hash) {
                return Ok(false);
            }
            return Err("conflicting membership revision rejected".into());
        }
        let next = Snapshot {
            network: self.snapshot.network.clone(),
            initial_authorities_hash: self.snapshot.initial_authorities_hash.clone(),
            highest_revision: manifest.revision,
            payload_hash: Some(hash),
            authorities: manifest.authorities.clone(),
            manifest: Some(manifest),
            envelope: Some(envelope.clone()),
        };
        state::write_json(&self.path, &next)?;
        self.snapshot = next;
        Ok(true)
    }

    pub fn manifest(&self, now: i64) -> Option<&Manifest> {
        self.snapshot
            .manifest
            .as_ref()
            .filter(|manifest| manifest.not_before <= now && manifest.expires > now)
    }

    pub fn cached_manifest(&self) -> Option<&Manifest> {
        self.snapshot.manifest.as_ref()
    }

    pub fn highest_revision(&self) -> u64 {
        self.snapshot.highest_revision
    }

    pub fn current_envelope(&self) -> Option<&Envelope> {
        self.snapshot.envelope.as_ref()
    }
}

pub fn parse_envelope(bytes: &[u8]) -> Result<Envelope> {
    if bytes.len() > MAX_ENVELOPE_BYTES {
        return Err("membership envelope exceeds 64 KiB".into());
    }
    serde_json::from_slice(bytes).map_err(|_| "invalid membership envelope".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::rand::SystemRandom;
    use ring::signature::{Ed25519KeyPair, KeyPair};

    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            Self(
                std::env::temp_dir()
                    .join(format!("agent-inbox-membership-{}", state::id().unwrap())),
            )
        }
        fn path(&self) -> PathBuf {
            self.0.join("state.json")
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn key() -> Ed25519KeyPair {
        let generated = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).unwrap();
        Ed25519KeyPair::from_pkcs8(generated.as_ref()).unwrap()
    }
    fn public(key: &Ed25519KeyPair) -> String {
        STANDARD_NO_PAD.encode(key.public_key().as_ref())
    }
    fn authorities(keys: &[&Ed25519KeyPair]) -> AuthoritySet {
        let mut names: Vec<_> = keys.iter().map(|key| public(key)).collect();
        names.sort();
        AuthoritySet {
            threshold: 2,
            keys: names,
        }
    }
    fn transport_key() -> [u8; 32] {
        PublicKey::from(&StaticSecret::from([9u8; 32])).to_bytes()
    }
    fn manifest(revision: u64, roots: AuthoritySet, transport: [u8; 32]) -> Manifest {
        Manifest {
            version: 1,
            network: "team-alpha".into(),
            revision,
            token_epoch: 1,
            not_before: 900,
            expires: 1900,
            authorities: roots,
            members: vec![Member {
                host: "alpha".into(),
                mesh_node_id: Some(1),
                transport_key: STANDARD_NO_PAD.encode(transport),
            }],
        }
    }
    fn signed(manifest: &Manifest, keys: &[&Ed25519KeyPair]) -> Envelope {
        let payload = serde_json::to_vec(manifest).unwrap();
        let mut body = DOMAIN.to_vec();
        body.extend_from_slice(&payload);
        Envelope {
            payload: STANDARD_NO_PAD.encode(&payload),
            signatures: keys
                .iter()
                .map(|key| ManifestSignature {
                    public_key: public(key),
                    signature: STANDARD_NO_PAD.encode(key.sign(&body).as_ref()),
                })
                .collect(),
        }
    }

    #[test]
    fn quorum_survives_restart_and_rejects_rollback_or_fork() {
        let temp = Temp::new();
        let roots = [key(), key(), key()];
        let transport = transport_key();
        let authorities = authorities(&[&roots[0], &roots[1], &roots[2]]);
        let first = manifest(1, authorities.clone(), transport);
        let envelope = signed(&first, &[&roots[0], &roots[1]]);
        {
            let mut store =
                Store::open(temp.path(), "team-alpha".into(), authorities.clone()).unwrap();
            assert!(store.accept(&envelope, 1000).unwrap());
            assert!(!store.accept(&envelope, 1000).unwrap());
            assert!(store.manifest(1000).is_some());
        }
        let mut store = Store::open(temp.path(), "team-alpha".into(), authorities.clone()).unwrap();
        assert_eq!(store.highest_revision(), 1);
        let mut fork = first.clone();
        fork.members[0].host = "other".into();
        assert!(
            store
                .accept(&signed(&fork, &[&roots[0], &roots[1]]), 1000)
                .is_err()
        );
        let second = manifest(2, authorities, transport);
        assert!(store.accept(&signed(&second, &[&roots[0]]), 1000).is_err());
        assert!(
            store
                .accept(&signed(&second, &[&roots[0], &roots[1]]), 1000)
                .unwrap()
        );
        assert!(store.accept(&envelope, 1000).is_err());
        assert!(store.manifest(1900).is_none());
    }

    #[test]
    fn authority_rotation_needs_old_and_new_quorums() {
        let temp = Temp::new();
        let old = [key(), key(), key()];
        let next = [key(), key(), key()];
        let transport = transport_key();
        let old_set = authorities(&[&old[0], &old[1], &old[2]]);
        let next_set = authorities(&[&next[0], &next[1], &next[2]]);
        let mut store = Store::open(temp.path(), "team-alpha".into(), old_set.clone()).unwrap();
        store
            .accept(
                &signed(&manifest(1, old_set, transport), &[&old[0], &old[1]]),
                1000,
            )
            .unwrap();
        let rotated = manifest(2, next_set.clone(), transport);
        assert!(
            store
                .accept(&signed(&rotated, &[&old[0], &old[1]]), 1000)
                .is_err()
        );
        assert!(
            store
                .accept(
                    &signed(&rotated, &[&old[0], &old[1], &next[0], &next[1]]),
                    1000,
                )
                .unwrap()
        );
        assert!(
            store
                .accept(
                    &signed(&manifest(3, next_set, transport), &[&next[0], &next[1]]),
                    1000,
                )
                .unwrap()
        );
    }

    #[test]
    fn rejects_expired_duplicate_and_unsigned_equal_revision() {
        let temp = Temp::new();
        let roots = [key(), key()];
        let transport = transport_key();
        let authorities = authorities(&[&roots[0], &roots[1]]);
        let mut store = Store::open(temp.path(), "team-alpha".into(), authorities.clone()).unwrap();
        let mut first = manifest(1, authorities, transport);
        let envelope = signed(&first, &[&roots[0], &roots[1]]);
        assert!(store.accept(&envelope, 3000).is_err());
        first.members.push(first.members[0].clone());
        assert!(
            store
                .accept(&signed(&first, &[&roots[0], &roots[1]]), 1000)
                .is_err()
        );
        assert!(store.accept(&envelope, 1000).unwrap());
        let mut unsigned = envelope;
        unsigned.signatures.clear();
        assert!(store.accept(&unsigned, 1000).is_err());
    }
}
