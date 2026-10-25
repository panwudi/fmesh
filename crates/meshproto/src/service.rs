//! Service announcements: which local services (name, port, opaque ident) a node offers, sent
//! to its peers in a sealed DISCO message (kind `wire::SERVICES`). The sender is the node id in
//! the authenticated header, never a payload field; each node announces only its own services.
//!
//! An announcement is a discovery hint, not an authorization: any local user of the sender can
//! register a service, and `ident` is whatever the service wrote. Admission is the service's
//! own job (docs/design.md 7b).
//!
//! Body after the kind byte:
//!
//! ```text
//! ts u64        sender wall clock (ns), strictly increasing per sender
//! ttl u16       seconds the receiver may keep the list (capped by the receiver)
//! n u8          number of entries, at most MAX_SERVICES; 0 withdraws everything
//! n x [name_len u8][name][port u16][ident_len u8][ident]
//! ```

pub const MAX_SERVICES: usize = 8;
pub const MAX_NAME: usize = 32;
pub const MAX_IDENT: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Service {
    pub name: String,
    pub port: u16,
    /// Opaque, chosen by the service (e.g. a key fingerprint); may be empty.
    pub ident: String,
}

/// Service names: 1-32 of `[a-z0-9-]`, not starting with `-`.
pub fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_NAME
        && !s.starts_with('-')
        && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Idents: up to 64 printable ASCII characters without spaces (they end up in the status file,
/// where `-` stands for an empty one, so `-` itself is not allowed).
pub fn valid_ident(s: &str) -> bool {
    s.len() <= MAX_IDENT && s != "-" && s.bytes().all(|b| (0x21..=0x7e).contains(&b))
}

fn valid(s: &Service) -> bool {
    valid_name(&s.name) && s.port != 0 && valid_ident(&s.ident)
}

/// Encodes an announcement body; `None` if an entry is invalid or there are too many.
pub fn encode(ts: u64, ttl_secs: u16, list: &[Service]) -> Option<Vec<u8>> {
    if list.len() > MAX_SERVICES || !list.iter().all(valid) {
        return None;
    }
    let mut out = Vec::with_capacity(11 + list.len() * (4 + MAX_NAME + MAX_IDENT));
    out.extend_from_slice(&ts.to_be_bytes());
    out.extend_from_slice(&ttl_secs.to_be_bytes());
    out.push(list.len() as u8);
    for s in list {
        out.push(s.name.len() as u8);
        out.extend_from_slice(s.name.as_bytes());
        out.extend_from_slice(&s.port.to_be_bytes());
        out.push(s.ident.len() as u8);
        out.extend_from_slice(s.ident.as_bytes());
    }
    Some(out)
}

/// Decodes an announcement body: `(ts, ttl seconds, entries)`. Rejects anything malformed,
/// invalid, duplicated or with trailing bytes, so a bad list never half-applies.
pub fn decode(b: &[u8]) -> Option<(u64, u16, Vec<Service>)> {
    let ts = crate::wire::get_u64(b)?;
    let ttl = u16::from_be_bytes(b.get(8..10)?.try_into().ok()?);
    let n = *b.get(10)? as usize;
    if n > MAX_SERVICES {
        return None;
    }
    let mut i = 11;
    let mut list: Vec<Service> = Vec::with_capacity(n);
    for _ in 0..n {
        let nl = *b.get(i)? as usize;
        let name = std::str::from_utf8(b.get(i + 1..i + 1 + nl)?).ok()?.to_string();
        i += 1 + nl;
        let port = u16::from_be_bytes(b.get(i..i + 2)?.try_into().ok()?);
        let il = *b.get(i + 2)? as usize;
        let ident = std::str::from_utf8(b.get(i + 3..i + 3 + il)?).ok()?.to_string();
        i += 3 + il;
        let s = Service { name, port, ident };
        if !valid(&s) || list.iter().any(|x| x.name == s.name) {
            return None;
        }
        list.push(s);
    }
    (i == b.len()).then_some((ts, ttl, list))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn svc(name: &str, port: u16, ident: &str) -> Service {
        Service { name: name.into(), port, ident: ident.into() }
    }

    #[test]
    fn round_trip() {
        let list = vec![svc("agent-inbox", 8832, "sha256:ab12"), svc("x", 1, "")];
        let b = encode(7, 120, &list).unwrap();
        assert_eq!(decode(&b), Some((7, 120, list)));
        assert_eq!(decode(&encode(9, 60, &[]).unwrap()), Some((9, 60, vec![])));
    }

    #[test]
    fn full_list_fits_one_packet() {
        let list: Vec<Service> = (0..MAX_SERVICES)
            .map(|k| svc(&format!("{k}{}", "a".repeat(MAX_NAME - 1)), 65535, &"z".repeat(MAX_IDENT)))
            .collect();
        let b = encode(1, 1, &list).unwrap();
        assert!(b.len() < 1000, "{} bytes", b.len());
        assert_eq!(decode(&b).unwrap().2, list);
    }

    #[test]
    fn rejects_invalid_entries() {
        for bad in [svc("", 1, ""), svc("Agent", 1, ""), svc("-x", 1, ""), svc("a b", 1, ""), svc("x", 0, ""), svc("x", 1, "has space")] {
            assert!(encode(1, 1, std::slice::from_ref(&bad)).is_none(), "{bad:?}");
        }
        assert!(encode(1, 1, &vec![svc("x", 1, ""); MAX_SERVICES + 1]).is_none());
        assert!(!valid_ident(&"z".repeat(MAX_IDENT + 1)));
        assert!(!valid_ident("-"));
    }

    #[test]
    fn rejects_malformed_bodies() {
        let good = encode(5, 30, &[svc("agent-inbox", 8832, "id")]).unwrap();
        for cut in 0..good.len() {
            assert!(decode(&good[..cut]).is_none(), "truncated at {cut}");
        }
        let mut trailing = good.clone();
        trailing.push(0);
        assert!(decode(&trailing).is_none());
        let mut count = good.clone();
        count[10] = MAX_SERVICES as u8 + 1;
        assert!(decode(&count).is_none());
        let mut charset = good.clone();
        charset[12] = b'A'; // first byte of the name
        assert!(decode(&charset).is_none());
        // Duplicate names: two copies of the same entry.
        let mut dup = encode(5, 30, &[svc("a", 1, ""), svc("b", 1, "")]).unwrap();
        let b_at = dup.len() - 5;
        dup[b_at + 1] = b'a';
        assert!(decode(&dup).is_none());
    }
}
