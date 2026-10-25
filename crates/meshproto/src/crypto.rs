//! Control-plane encryption between two X25519 identities (node <-> relay, node <-> node).
//!
//! key = HKDF-SHA256(salt "flytomesh v1", X25519(my secret, their public)) -> ChaCha20-Poly1305.
//! The key is symmetric (both sides derive the same one), so every sealed message carries
//! its sender in the clear header, which is bound in as associated data. Nonces are 12
//! random bytes per message: control traffic is a few packets per second at most.

use crate::x25519::{PublicKey, StaticSecret};
use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, CHACHA20_POLY1305, NONCE_LEN};
use ring::hkdf::{Salt, HKDF_SHA256};

pub const TAG_LEN: usize = 16;
pub use ring::aead::NONCE_LEN as NONCE;

/// Bytes a sealed message adds to its plaintext: nonce in front, tag at the end.
pub const OVERHEAD: usize = NONCE_LEN + TAG_LEN;

pub struct PairKey(LessSafeKey);

impl PairKey {
    pub fn new(mine: &StaticSecret, theirs: &[u8; 32]) -> PairKey {
        let shared = mine.diffie_hellman(&PublicKey::from(*theirs));
        let prk = Salt::new(HKDF_SHA256, b"flytomesh v1").extract(shared.as_bytes());
        let okm = prk.expand(&[b"control"], &CHACHA20_POLY1305).expect("hkdf length");
        PairKey(LessSafeKey::new(UnboundKey::from(okm)))
    }

    /// Appends `nonce || ciphertext || tag` of `plain` to `out`; `out` so far is the clear
    /// header and is authenticated as associated data.
    pub fn seal(&self, out: &mut Vec<u8>, plain: &[u8]) {
        let hdr_len = out.len();
        let mut nonce = [0u8; NONCE_LEN];
        crate::fill_random(&mut nonce);
        out.extend_from_slice(&nonce);
        out.extend_from_slice(plain);
        let (hdr, rest) = out.split_at_mut(hdr_len);
        let body = &mut rest[NONCE_LEN..];
        let tag = self
            .0
            .seal_in_place_separate_tag(Nonce::assume_unique_for_key(nonce), Aad::from(&*hdr), body)
            .expect("seal");
        out.extend_from_slice(tag.as_ref());
    }

    /// Opens a message sealed by `seal`: `hdr` is the clear header, `sealed` is what follows
    /// it. Decrypts in place and returns the plaintext.
    pub fn open<'a>(&self, hdr: &[u8], sealed: &'a mut [u8]) -> Option<&'a mut [u8]> {
        if sealed.len() < OVERHEAD {
            return None;
        }
        let (nonce, body) = sealed.split_at_mut(NONCE_LEN);
        let nonce = Nonce::try_assume_unique_for_key(nonce).ok()?;
        self.0.open_in_place(nonce, Aad::from(hdr), body).ok().map(|p| &mut p[..])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_sides_derive_the_same_key() {
        let a = crate::new_secret();
        let b = crate::new_secret();
        let ka = PairKey::new(&a, PublicKey::from(&b).as_bytes());
        let kb = PairKey::new(&b, PublicKey::from(&a).as_bytes());
        let mut msg = vec![0xFA, 0x10, 0, 7];
        ka.seal(&mut msg, b"hello");
        let (hdr, sealed) = msg.split_at_mut(4);
        assert_eq!(kb.open(hdr, sealed).unwrap(), b"hello");
    }

    #[test]
    fn header_is_authenticated() {
        let a = crate::new_secret();
        let b = crate::new_secret();
        let ka = PairKey::new(&a, PublicKey::from(&b).as_bytes());
        let mut msg = vec![0xFA, 0x10, 0, 7];
        ka.seal(&mut msg, b"hello");
        msg[3] = 8; // claim another sender id
        let (hdr, sealed) = msg.split_at_mut(4);
        assert!(ka.open(hdr, sealed).is_none());
    }
}
