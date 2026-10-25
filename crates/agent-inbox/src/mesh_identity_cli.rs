//! Offline provisioning commands for experimental dynamic peer membership.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD_NO_PAD;
use ring::rand::SystemRandom;
use ring::signature::{Ed25519KeyPair, KeyPair};

use crate::mesh_membership::{
    AuthoritySet, Envelope, Manifest, ManifestSignature, Store, parse_envelope,
};
use crate::mesh_pair::HostSecret;
use crate::{Result, state};

const DOMAIN: &[u8] = b"agent-inbox-membership-v1\0";
const RENEW_BEFORE_SECS: i64 = 14 * 24 * 60 * 60;
const RENEW_VALIDITY_SECS: i64 = 28 * 24 * 60 * 60;

fn renew_single_at(
    manifest_file: &Path,
    key_file: &Path,
    state_file: &Path,
    now: i64,
) -> Result<bool> {
    let key = authority_key(key_file)?;
    let public = STANDARD_NO_PAD.encode(key.public_key().as_ref());
    let mut file = state::open_private(manifest_file, false)?;
    if file.metadata()?.len() > 64 * 1024 {
        return Err("membership envelope exceeds 64 KiB".into());
    }
    let mut raw = Vec::new();
    file.read_to_end(&mut raw)?;
    let current = parse_envelope(&raw)?;
    let payload = STANDARD_NO_PAD
        .decode(&current.payload)
        .map_err(|_| "invalid membership payload encoding")?;
    let manifest: Manifest = serde_json::from_slice(&payload)?;
    let authorities = AuthoritySet {
        threshold: 1,
        keys: vec![public.clone()],
    };
    if manifest.authorities != authorities {
        return Err("automatic renewal requires this sole pinned authority key".into());
    }
    let mut store = Store::open(
        state_file.to_path_buf(),
        manifest.network.clone(),
        authorities,
    )?;
    store.accept(&current, now)?;
    let saved = store
        .cached_manifest()
        .ok_or("signer has no verified membership")?;
    if saved.expires.saturating_sub(now) > RENEW_BEFORE_SECS {
        println!(
            "membership revision {} does not need renewal",
            saved.revision
        );
        return Ok(false);
    }
    let mut next = saved.clone();
    next.revision = next
        .revision
        .checked_add(1)
        .ok_or("membership revision exhausted")?;
    next.not_before = now.saturating_sub(60);
    next.expires = now.saturating_add(RENEW_VALIDITY_SECS);
    let payload = serde_json::to_vec(&next)?;
    let mut signed = DOMAIN.to_vec();
    signed.extend_from_slice(&payload);
    let envelope = Envelope {
        payload: STANDARD_NO_PAD.encode(payload),
        signatures: vec![ManifestSignature {
            public_key: public,
            signature: STANDARD_NO_PAD.encode(key.sign(&signed).as_ref()),
        }],
    };
    state::write_json(manifest_file, &envelope)?;
    store.accept(&envelope, now)?;
    println!(
        "renewed membership revision {} through {}",
        next.revision, next.expires
    );
    Ok(true)
}

fn option(args: &mut impl Iterator<Item = String>, expected: &str) -> Result<PathBuf> {
    if args.next().as_deref() != Some(expected) {
        return Err(format!("expected {expected} <path>").into());
    }
    Ok(args.next().ok_or("missing path")?.into())
}

fn create_private(path: &Path, bytes: &[u8]) -> Result<()> {
    state::parent(path)?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    if let Some(parent) = path.parent() {
        File::open(parent)?.sync_all()?;
    }
    Ok(())
}

fn authority_key(path: &Path) -> Result<Ed25519KeyPair> {
    let mut file = state::open_private(path, false)?;
    if file.metadata()?.len() > 4096 {
        return Err("authority key file is too large".into());
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ed25519KeyPair::from_pkcs8(&bytes).map_err(|_| "invalid authority Ed25519 key".into())
}

pub fn run(mut args: impl Iterator<Item = String>) -> Result<()> {
    match args.next().as_deref() {
        Some("host-keygen") => {
            let path = option(&mut args, "--file")?;
            if args.next().is_some() {
                return Err("unexpected host-keygen argument".into());
            }
            let key = HostSecret::generate(&path)?;
            println!("{}", STANDARD_NO_PAD.encode(key.public_key()));
            Ok(())
        }
        Some("authority-keygen") => {
            let path = option(&mut args, "--file")?;
            if args.next().is_some() {
                return Err("unexpected authority-keygen argument".into());
            }
            let generated = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new())
                .map_err(|_| "cannot generate authority key")?;
            create_private(&path, generated.as_ref())?;
            let key = Ed25519KeyPair::from_pkcs8(generated.as_ref())
                .map_err(|_| "generated authority key is invalid")?;
            println!("{}", STANDARD_NO_PAD.encode(key.public_key().as_ref()));
            Ok(())
        }
        Some("manifest-sign") => {
            let payload_file = option(&mut args, "--payload")?;
            let key_file = option(&mut args, "--key")?;
            let output = option(&mut args, "--output")?;
            if args.next().is_some() {
                return Err("unexpected manifest-sign argument".into());
            }
            let payload = std::fs::read(payload_file)?;
            if payload.len() > 48 * 1024 {
                return Err("membership payload exceeds 48 KiB".into());
            }
            let _: Manifest = serde_json::from_slice(&payload)
                .map_err(|_| "invalid membership payload structure")?;
            let encoded = STANDARD_NO_PAD.encode(&payload);
            let mut envelope = if output.exists() {
                let saved = parse_envelope(&std::fs::read(&output)?)?;
                if saved.payload != encoded {
                    return Err("output already signs a different payload".into());
                }
                saved
            } else {
                Envelope {
                    payload: encoded,
                    signatures: Vec::new(),
                }
            };
            let key = authority_key(&key_file)?;
            let public_key = STANDARD_NO_PAD.encode(key.public_key().as_ref());
            if envelope
                .signatures
                .iter()
                .any(|signature| signature.public_key == public_key)
            {
                return Err("authority already signed this payload".into());
            }
            let mut signed = DOMAIN.to_vec();
            signed.extend_from_slice(&payload);
            envelope.signatures.push(ManifestSignature {
                public_key,
                signature: STANDARD_NO_PAD.encode(key.sign(&signed).as_ref()),
            });
            state::write_json(&output, &envelope)?;
            println!("{} signature(s) recorded", envelope.signatures.len());
            Ok(())
        }
        Some("renew-single") => {
            let key_file = option(&mut args, "--key")?;
            let manifest_file = option(&mut args, "--manifest")?;
            let state_file = option(&mut args, "--state")?;
            if args.next().is_some() {
                return Err("unexpected renew-single argument".into());
            }
            renew_single_at(
                &manifest_file,
                &key_file,
                &state_file,
                chrono::Utc::now().timestamp(),
            )?;
            Ok(())
        }
        _ => Err(
            "mesh-identity requires host-keygen, authority-keygen, manifest-sign or renew-single"
                .into(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mesh_membership::Member;

    #[test]
    fn single_key_renewal_advances_once_and_rejects_tampering_or_rollback() {
        let root = std::env::temp_dir().join(format!("inbox-renew-{}", state::id().unwrap()));
        state::parent(&root.join("key")).unwrap();
        let key_file = root.join("authority.key");
        let key_bytes = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).unwrap();
        create_private(&key_file, key_bytes.as_ref()).unwrap();
        let key = authority_key(&key_file).unwrap();
        let public = STANDARD_NO_PAD.encode(key.public_key().as_ref());
        let host = HostSecret::generate(&root.join("host.key")).unwrap();
        let now = 1_800_000_000_i64;
        let manifest = Manifest {
            version: 1,
            network: "renewal-test".into(),
            revision: 1,
            token_epoch: 7,
            not_before: now - 27 * 24 * 3600,
            expires: now + 24 * 3600,
            authorities: AuthoritySet {
                threshold: 1,
                keys: vec![public.clone()],
            },
            members: vec![Member {
                host: "alpha".into(),
                mesh_node_id: Some(1),
                transport_key: STANDARD_NO_PAD.encode(host.public_key()),
            }],
        };
        let payload = serde_json::to_vec(&manifest).unwrap();
        let mut signed = DOMAIN.to_vec();
        signed.extend_from_slice(&payload);
        let old = Envelope {
            payload: STANDARD_NO_PAD.encode(&payload),
            signatures: vec![ManifestSignature {
                public_key: public,
                signature: STANDARD_NO_PAD.encode(key.sign(&signed).as_ref()),
            }],
        };
        let manifest_file = root.join("membership.json");
        let state_file = root.join("signer-state.json");
        state::write_json(&manifest_file, &old).unwrap();
        assert!(renew_single_at(&manifest_file, &key_file, &state_file, now).unwrap());
        let renewed = parse_envelope(&std::fs::read(&manifest_file).unwrap()).unwrap();
        let renewed_payload: Manifest =
            serde_json::from_slice(&STANDARD_NO_PAD.decode(&renewed.payload).unwrap()).unwrap();
        assert_eq!(renewed_payload.revision, 2);
        assert_eq!(renewed_payload.token_epoch, 7);
        assert_eq!(renewed_payload.members, manifest.members);
        assert_eq!(renewed_payload.expires, now + RENEW_VALIDITY_SECS);
        assert!(!renew_single_at(&manifest_file, &key_file, &state_file, now).unwrap());
        let mut tampered = renewed.clone();
        let mut changed = renewed_payload.clone();
        changed.expires += 1;
        tampered.payload = STANDARD_NO_PAD.encode(serde_json::to_vec(&changed).unwrap());
        state::write_json(&manifest_file, &tampered).unwrap();
        assert!(renew_single_at(&manifest_file, &key_file, &state_file, now).is_err());
        state::write_json(&manifest_file, &old).unwrap();
        assert!(renew_single_at(&manifest_file, &key_file, &state_file, now).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}
