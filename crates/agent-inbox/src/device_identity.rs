//! A host's persistent signing identity, created once on its first configured start.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD_NO_PAD;
use ring::digest::{SHA256, digest};
use ring::rand::SystemRandom;
use ring::signature::{ED25519, Ed25519KeyPair, KeyPair, UnparsedPublicKey};

use crate::{Result, state};

const DOMAIN: &[u8] = b"agent-inbox-device-v1\0";

pub struct DeviceIdentity(Ed25519KeyPair);

impl DeviceIdentity {
    pub fn open_or_create(path: &Path) -> Result<Self> {
        if path.exists() {
            return Self::read(path);
        }
        state::parent(path)?;
        let generated = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new())
            .map_err(|_| "cannot generate device identity")?;
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        match options.open(path) {
            Ok(mut file) => {
                file.write_all(generated.as_ref())?;
                file.sync_all()?;
                if let Some(parent) = path.parent() {
                    File::open(parent)?.sync_all()?;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
        Self::read(path)
    }

    pub fn read(path: &Path) -> Result<Self> {
        let mut file = state::open_private(path, false)?;
        if file.metadata()?.len() > 4096 {
            return Err("device identity key is too large".into());
        }
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        Ok(Self(
            Ed25519KeyPair::from_pkcs8(&bytes)
                .map_err(|_| "invalid device identity private key")?,
        ))
    }

    pub fn public_key(&self) -> String {
        STANDARD_NO_PAD.encode(self.0.public_key().as_ref())
    }

    pub fn fingerprint(&self) -> String {
        digest(&SHA256, self.0.public_key().as_ref())
            .as_ref()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    pub fn sign(&self, payload: &[u8]) -> String {
        let mut signed = DOMAIN.to_vec();
        signed.extend_from_slice(payload);
        STANDARD_NO_PAD.encode(self.0.sign(&signed).as_ref())
    }

    pub fn verify(public_key: &str, payload: &[u8], signature: &str) -> Result<()> {
        let public = STANDARD_NO_PAD
            .decode(public_key)
            .map_err(|_| "invalid device public key")?;
        let signature = STANDARD_NO_PAD
            .decode(signature)
            .map_err(|_| "invalid device signature")?;
        let mut signed = DOMAIN.to_vec();
        signed.extend_from_slice(payload);
        UnparsedPublicKey::new(&ED25519, public)
            .verify(&signed, &signature)
            .map_err(|_| "device signature did not verify".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_open_creates_a_private_stable_identity_and_rejects_wrong_payload() {
        let root = std::env::temp_dir().join(format!("inbox-device-{}", state::id().unwrap()));
        let path = root.join("device.key");
        let identity = DeviceIdentity::open_or_create(&path).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let public = identity.public_key();
        let signature = identity.sign(b"device-claim");
        DeviceIdentity::verify(&public, b"device-claim", &signature).unwrap();
        assert!(DeviceIdentity::verify(&public, b"different", &signature).is_err());
        let reopened = DeviceIdentity::open_or_create(&path).unwrap();
        assert_eq!(reopened.public_key(), public);
        assert_eq!(reopened.fingerprint(), identity.fingerprint());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn invalid_existing_key_fails_without_replacing_it() {
        let root = std::env::temp_dir().join(format!("inbox-device-{}", state::id().unwrap()));
        let path = root.join("device.key");
        state::parent(&path).unwrap();
        std::fs::write(&path, b"broken").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        assert!(DeviceIdentity::open_or_create(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"broken");
        std::fs::remove_dir_all(root).unwrap();
    }
}
