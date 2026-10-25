//! Verify FlytoMesh's public roster and bind status addresses to signed node IDs.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::path::Path;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use ring::signature::{ED25519, UnparsedPublicKey};

use crate::{Result, hub};

const MAX_ROSTER_BYTES: u64 = 64 * 1024;
const MAX_STATUS_BYTES: u64 = 512 * 1024;
const MAX_STATUS_AGE: Duration = Duration::from_secs(10);
const MAX_HANDSHAKE_AGE: u64 = 180;
const MAX_NODES: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Node {
    pub id: u16,
    pub name: String,
    pub ip: Ipv4Addr,
    pub key: [u8; 32],
}

#[derive(Clone, Debug)]
pub struct Roster {
    pub version: u64,
    pub nodes: HashMap<u16, Node>,
}

#[derive(Clone, Debug)]
pub struct StatusPeer {
    pub node: Node,
    pub handshake_age: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct Status {
    pub roster_version: u64,
    pub local: Node,
    pub peers: HashMap<u16, StatusPeer>,
    pub services: HashMap<(u16, String), StatusService>,
}

#[derive(Clone, Debug)]
pub struct StatusService {
    pub node: Node,
    pub name: String,
    pub port: u16,
    pub ident: Option<String>,
    pub expires: u64,
    pub uid: Option<u32>,
}

fn decode_key(encoded: &str) -> Result<[u8; 32]> {
    STANDARD
        .decode(encoded)
        .map_err(|_| "invalid FlytoMesh public key encoding")?
        .try_into()
        .map_err(|_| "FlytoMesh public key must be 32 bytes".into())
}

fn valid_ip(ip: Ipv4Addr) -> Result<Ipv4Addr> {
    if ip.is_unspecified() || ip.is_multicast() {
        return Err("invalid FlytoMesh node IP".into());
    }
    Ok(ip)
}

fn signed_body(raw: &[u8]) -> Result<(&[u8], Vec<u8>)> {
    let text = std::str::from_utf8(raw).map_err(|_| "FlytoMesh roster is not UTF-8")?;
    let position = text
        .rfind("\nsig ")
        .ok_or("FlytoMesh roster has no signature line")?
        + 1;
    let signature = STANDARD
        .decode(text[position + 4..].trim())
        .map_err(|_| "invalid FlytoMesh roster signature encoding")?;
    if signature.len() != 64 {
        return Err("invalid FlytoMesh roster signature length".into());
    }
    Ok((&raw[..position], signature))
}

pub fn verify_roster(raw: &[u8], admin_key: &[u8; 32]) -> Result<Roster> {
    if raw.len() as u64 > MAX_ROSTER_BYTES {
        return Err("FlytoMesh roster exceeds 64 KiB".into());
    }
    let (body, signature) = signed_body(raw)?;
    UnparsedPublicKey::new(&ED25519, admin_key)
        .verify(body, &signature)
        .map_err(|_| "FlytoMesh roster signature did not verify")?;
    let text = std::str::from_utf8(body)?;
    let mut lines = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'));
    if lines.next() != Some("flytomesh-roster 1") {
        return Err("unsupported FlytoMesh roster format".into());
    }
    let mut version = None;
    let mut nodes = HashMap::new();
    let mut names = HashSet::new();
    let mut ips = HashSet::new();
    let mut keys = HashSet::new();
    for line in lines {
        let words: Vec<_> = line.split_whitespace().collect();
        match words.as_slice() {
            ["version", number] if version.is_none() => {
                let parsed: u64 = number.parse()?;
                if parsed == 0 {
                    return Err("FlytoMesh roster version must be positive".into());
                }
                version = Some(parsed);
            }
            ["relay", name, address, key] => {
                hub::validate_component(name)?;
                let _: SocketAddrV4 = address.parse()?;
                decode_key(key)?;
            }
            ["node", id, name, ip, key] => {
                let id: u16 = id.parse()?;
                if id == 0 || nodes.len() >= MAX_NODES {
                    return Err("invalid FlytoMesh node count or ID".into());
                }
                hub::validate_component(name)?;
                let ip = valid_ip(ip.parse()?)?;
                let key = decode_key(key)?;
                if !names.insert((*name).to_owned()) || !ips.insert(ip) || !keys.insert(key) {
                    return Err("duplicate FlytoMesh node identity".into());
                }
                let node = Node {
                    id,
                    name: (*name).to_owned(),
                    ip,
                    key,
                };
                if nodes.insert(id, node).is_some() {
                    return Err("duplicate FlytoMesh node ID".into());
                }
            }
            _ => return Err("invalid FlytoMesh roster line".into()),
        }
    }
    Ok(Roster {
        version: version.ok_or("FlytoMesh roster has no version")?,
        nodes,
    })
}

pub fn load_public_roster(
    roster_file: &Path,
    admin_file: &Path,
    pinned_admin_key: &[u8; 32],
) -> Result<Roster> {
    let admin_metadata = fs::metadata(admin_file)?;
    if !admin_metadata.is_file() || admin_metadata.len() > 128 {
        return Err("invalid FlytoMesh administrator public key file".into());
    }
    let public = fs::read_to_string(admin_file)?;
    if decode_key(public.trim())? != *pinned_admin_key {
        return Err("FlytoMesh administrator public key differs from provisioning".into());
    }
    let metadata = fs::metadata(roster_file)?;
    if !metadata.is_file() || metadata.len() > MAX_ROSTER_BYTES {
        return Err("invalid FlytoMesh public roster file".into());
    }
    verify_roster(&fs::read(roster_file)?, pinned_admin_key)
}

fn field<'a>(words: &'a [&str], name: &str) -> Option<&'a str> {
    words
        .windows(2)
        .find(|pair| pair[0] == name)
        .map(|pair| pair[1])
}

fn status_node(words: &[&str]) -> Result<Node> {
    let [_, name, ip, ..] = words else {
        return Err("invalid FlytoMesh status node row".into());
    };
    hub::validate_component(name)?;
    let id: u16 = field(words, "id")
        .ok_or("FlytoMesh status has no node ID")?
        .parse()?;
    if id == 0 {
        return Err("FlytoMesh status has reserved node ID".into());
    }
    let key = decode_key(field(words, "key").ok_or("FlytoMesh status has no node key")?)?;
    Ok(Node {
        id,
        name: (*name).to_owned(),
        ip: valid_ip(ip.parse()?)?,
        key,
    })
}

pub fn parse_status(text: &str) -> Result<Status> {
    if text.len() as u64 > MAX_STATUS_BYTES {
        return Err("FlytoMesh status exceeds 512 KiB".into());
    }
    let mut local = None;
    let mut roster_version = None;
    let mut peers = HashMap::new();
    let mut services = HashMap::new();
    for line in text.lines() {
        let words: Vec<_> = line.split_whitespace().collect();
        match words.first().copied() {
            Some("node") => {
                if local.is_some() {
                    return Err("duplicate FlytoMesh local status row".into());
                }
                let version = field(&words, "roster")
                    .and_then(|value| value.strip_prefix('v'))
                    .ok_or("FlytoMesh status has no roster version")?
                    .parse::<u64>()?;
                roster_version = Some(version);
                local = Some(status_node(&words)?);
            }
            Some("peer") => {
                if peers.len() >= MAX_NODES {
                    return Err("FlytoMesh status has too many peers".into());
                }
                let node = status_node(&words)?;
                let age =
                    field(&words, "handshake").ok_or("FlytoMesh status peer has no handshake")?;
                let handshake_age = if age == "never" {
                    None
                } else {
                    Some(
                        age.strip_suffix('s')
                            .ok_or("invalid FlytoMesh handshake age")?
                            .parse()?,
                    )
                };
                if peers
                    .insert(
                        node.id,
                        StatusPeer {
                            node,
                            handshake_age,
                        },
                    )
                    .is_some()
                {
                    return Err("duplicate FlytoMesh status peer ID".into());
                }
            }
            Some("service") => {
                if services.len() >= MAX_NODES * 8 {
                    return Err("FlytoMesh status has too many services".into());
                }
                let [
                    _,
                    node_name,
                    ip,
                    "id",
                    id,
                    "name",
                    name,
                    "port",
                    port,
                    "ident",
                    ident,
                    "expires",
                    expires,
                    rest @ ..,
                ] = words.as_slice()
                else {
                    return Err("invalid FlytoMesh service status row".into());
                };
                let uid = match rest {
                    [] | ["uid", "-"] => None,
                    ["uid", value] => Some(value.parse::<u32>()?),
                    _ => return Err("invalid FlytoMesh service UID".into()),
                };
                if name.is_empty()
                    || name.len() > 32
                    || name.starts_with('-')
                    || !name.bytes().all(|byte| {
                        byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'
                    })
                {
                    return Err("invalid FlytoMesh service name".into());
                }
                let ident = if *ident == "-" {
                    None
                } else {
                    if ident.len() > 64
                        || ident.is_empty()
                        || !ident
                            .bytes()
                            .all(|byte| byte.is_ascii_graphic() && byte != b' ')
                    {
                        return Err("invalid FlytoMesh service identity".into());
                    }
                    Some((*ident).to_owned())
                };
                let expires = expires
                    .strip_suffix('s')
                    .ok_or("invalid FlytoMesh service expiry")?
                    .parse::<u64>()?;
                if expires > 300 {
                    return Err("invalid FlytoMesh service expiry".into());
                }
                let port = port.parse::<u16>()?;
                if port == 0 {
                    return Err("invalid FlytoMesh service port".into());
                }
                hub::validate_component(node_name)?;
                let node = Node {
                    id: id.parse()?,
                    name: (*node_name).to_owned(),
                    ip: valid_ip(ip.parse()?)?,
                    key: [0u8; 32],
                };
                if services
                    .insert(
                        (node.id, (*name).to_owned()),
                        StatusService {
                            node,
                            name: (*name).to_owned(),
                            port,
                            ident,
                            expires,
                            uid,
                        },
                    )
                    .is_some()
                {
                    return Err("duplicate FlytoMesh service status row".into());
                }
            }
            _ => {}
        }
    }
    let local = local.ok_or("FlytoMesh status has no local identity")?;
    for service in services.values_mut() {
        let peer = if service.node.id == local.id {
            &local
        } else {
            &peers
                .get(&service.node.id)
                .ok_or("service status has no peer row")?
                .node
        };
        if service.node.name != peer.name || service.node.ip != peer.ip {
            return Err("FlytoMesh service does not match node status".into());
        }
        if service.node.id != local.id && service.uid.is_some() {
            return Err("remote FlytoMesh service must not include a UID".into());
        }
        service.node.key = peer.key;
    }
    Ok(Status {
        roster_version: roster_version.ok_or("FlytoMesh status has no node row")?,
        local,
        peers,
        services,
    })
}

pub fn peer_service_address(
    roster: &Roster,
    status: &Status,
    member_id: u16,
    service_name: &str,
) -> Result<Option<SocketAddrV4>> {
    let Some(ip) = peer_address(roster, status, member_id)? else {
        return Ok(None);
    };
    let Some(service) = status.services.get(&(member_id, service_name.to_owned())) else {
        return Ok(None);
    };
    if service.expires == 0 {
        return Ok(None);
    }
    if roster.nodes.get(&member_id) != Some(&service.node) {
        return Err("FlytoMesh service identity differs from signed roster".into());
    }
    Ok(Some(SocketAddrV4::new(ip, service.port)))
}

pub fn load_status(path: &Path) -> Result<Status> {
    let metadata = fs::metadata(path)?;
    if !metadata.is_file()
        || metadata.len() > MAX_STATUS_BYTES
        || metadata.modified()?.elapsed()? > MAX_STATUS_AGE
    {
        return Err("FlytoMesh status file is stale or invalid".into());
    }
    parse_status(&fs::read_to_string(path)?)
}

pub fn peer_address(roster: &Roster, status: &Status, member_id: u16) -> Result<Option<Ipv4Addr>> {
    if roster.version != status.roster_version {
        return Err("FlytoMesh roster and status revisions differ".into());
    }
    if roster.nodes.get(&status.local.id) != Some(&status.local) {
        return Err("FlytoMesh local status identity differs from signed roster".into());
    }
    let member = roster
        .nodes
        .get(&member_id)
        .ok_or("inbox member is absent from signed FlytoMesh roster")?;
    let Some(peer) = status.peers.get(&member_id) else {
        return Ok(None);
    };
    if &peer.node != member {
        return Err("FlytoMesh peer status identity differs from signed roster".into());
    }
    Ok(peer
        .handshake_age
        .filter(|age| *age < MAX_HANDSHAKE_AGE)
        .map(|_| member.ip))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::rand::SystemRandom;
    use ring::signature::{Ed25519KeyPair, KeyPair};

    fn fixture() -> (Vec<u8>, [u8; 32], String) {
        let generated = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).unwrap();
        let signer = Ed25519KeyPair::from_pkcs8(generated.as_ref()).unwrap();
        let admin: [u8; 32] = signer.public_key().as_ref().try_into().unwrap();
        let first = STANDARD.encode([1; 32]);
        let second = STANDARD.encode([2; 32]);
        let body = format!(
            "flytomesh-roster 1\nversion 3\nnode 1 alpha 10.249.0.1 {first}\nnode 2 beta 10.249.0.2 {second}\n"
        );
        let roster = format!(
            "{body}sig {}\n",
            STANDARD.encode(signer.sign(body.as_bytes()).as_ref())
        )
        .into_bytes();
        let status = format!(
            "node alpha 10.249.0.1 id 1 roster v3 downloading 1/2 release v7 key {first}\npeer beta 10.249.0.2 direct 10.0.0.2:41820 handshake 7s tx 1 rx 2 id 2 key {second}\n"
        );
        (roster, admin, status)
    }

    #[test]
    fn verified_roster_binds_peer_id_key_and_address() {
        let (raw, admin, status_text) = fixture();
        let roster = verify_roster(&raw, &admin).unwrap();
        let status = parse_status(&status_text).unwrap();
        assert_eq!(roster.version, 3);
        assert_eq!(
            peer_address(&roster, &status, 2).unwrap(),
            Some(Ipv4Addr::new(10, 249, 0, 2))
        );
        assert!(peer_address(&roster, &status, 3).is_err());
        assert!(
            peer_address(
                &roster,
                &parse_status(&status_text.replace("v3", "v4")).unwrap(),
                2
            )
            .is_err()
        );
        assert!(
            peer_address(
                &roster,
                &parse_status(&status_text.replace("handshake 7s", "handshake 181s")).unwrap(),
                2
            )
            .unwrap()
            .is_none()
        );
        assert!(
            peer_address(
                &roster,
                &parse_status(&status_text.replace("10.249.0.2", "10.249.0.9")).unwrap(),
                2
            )
            .is_err()
        );
    }

    #[test]
    fn invalid_signature_and_old_status_interface_fail_closed() {
        let (raw, admin, status) = fixture();
        let tampered = String::from_utf8(raw)
            .unwrap()
            .replace("10.249.0.2", "10.249.0.9");
        assert!(verify_roster(tampered.as_bytes(), &admin).is_err());
        let old = status.replace(" id 2 key ", " old-format-key ");
        assert!(parse_status(&old).is_err());
    }

    #[test]
    fn service_status_uses_the_verified_node_identity_and_advertised_port() {
        let (raw, admin, status_text) = fixture();
        let roster = verify_roster(&raw, &admin).unwrap();
        let services = format!(
            "{status_text}service alpha 10.249.0.1 id 1 name agent-inbox port 8832 ident ownfingerprint expires 90s uid 501\nservice beta 10.249.0.2 id 2 name agent-inbox port 19001 ident peerfingerprint expires 120s\n"
        );
        let status = parse_status(&services).unwrap();
        assert_eq!(
            status.services.get(&(1, "agent-inbox".into())).unwrap().uid,
            Some(501)
        );
        assert_eq!(
            peer_service_address(&roster, &status, 2, "agent-inbox").unwrap(),
            Some(SocketAddrV4::new(Ipv4Addr::new(10, 249, 0, 2), 19001))
        );
        assert!(
            peer_service_address(&roster, &status, 2, "other")
                .unwrap()
                .is_none()
        );
        let expiring = parse_status(&services.replace("expires 120s", "expires 0s")).unwrap();
        assert!(
            peer_service_address(&roster, &expiring, 2, "agent-inbox")
                .unwrap()
                .is_none()
        );
        assert!(
            parse_status(&services.replace("service beta 10.249.0.2", "service beta 10.249.0.9"))
                .is_err()
        );
        assert!(
            parse_status(&services.replace("expires 120s\n", "expires 120s uid 501\n")).is_err()
        );
    }
}
