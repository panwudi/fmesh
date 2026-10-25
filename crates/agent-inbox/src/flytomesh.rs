//! Optional FlytoMesh transport for host relays. Mesh status is routing data, not identity.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

use crate::{Result, http, hub, token::Token};

const STATUS_MAX_AGE: Duration = Duration::from_secs(10);
const HANDSHAKE_MAX_AGE: u64 = 180;
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const STATUS_MAX_BYTES: u64 = 64 * 1024;

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub status_file: Option<PathBuf>,
    pub node_name: Option<String>,
    pub listen_port: Option<u16>,
    #[serde(default)]
    pub peers: Vec<PeerConfig>,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PeerConfig {
    pub host: String,
    pub mesh_name: Option<String>,
    pub port: Option<u16>,
    pub token_file: PathBuf,
}

#[derive(Clone)]
struct Peer {
    mesh_name: String,
    port: u16,
    token: Token,
}

#[derive(Clone)]
pub struct Plugin {
    status_file: PathBuf,
    node_name: String,
    port: u16,
    peers: HashMap<String, Peer>,
}

#[derive(Clone)]
struct Status {
    node_name: String,
    node_ip: Ipv4Addr,
    peers: HashMap<String, (Ipv4Addr, Option<u64>)>,
}

fn default_status_file() -> PathBuf {
    #[cfg(windows)]
    {
        PathBuf::from(r"C:\ProgramData\flytoMesh\status")
    }
    #[cfg(not(windows))]
    {
        PathBuf::from("/var/run/flytomesh.status")
    }
}

impl Plugin {
    pub fn load(
        config: &Config,
        path: &Path,
        host: &str,
        own_token: Option<&Token>,
    ) -> Result<Self> {
        let node_name = config.node_name.as_deref().unwrap_or(host);
        hub::validate_component(node_name)?;
        let port = config.listen_port.unwrap_or(8832);
        if port == 0 {
            return Err("FlytoMesh listener needs a nonzero port".into());
        }
        if config.peers.len() > 32 {
            return Err("FlytoMesh supports at most 32 peer hosts".into());
        }
        let mut peers: HashMap<String, Peer> = HashMap::new();
        let mut mesh_names = HashSet::new();
        for peer in &config.peers {
            hub::validate_component(&peer.host)?;
            let mesh_name = peer.mesh_name.as_deref().unwrap_or(&peer.host);
            hub::validate_component(mesh_name)?;
            let peer_port = peer.port.unwrap_or(port);
            if peer_port == 0 {
                return Err("FlytoMesh peer port must be nonzero".into());
            }
            if peer.host == host
                || mesh_name == node_name
                || peers.contains_key(&peer.host)
                || !mesh_names.insert(mesh_name.to_owned())
            {
                return Err("duplicate or local FlytoMesh peer".into());
            }
            let token = Token::read(&hub::relative_to(path, peer.token_file.clone()))?;
            if own_token.is_some_and(|own| token.bytes() == own.bytes())
                || peers
                    .values()
                    .any(|existing| existing.token.bytes() == token.bytes())
            {
                return Err("FlytoMesh peer tokens must be independent".into());
            }
            peers.insert(
                peer.host.clone(),
                Peer {
                    mesh_name: mesh_name.to_owned(),
                    port: peer_port,
                    token,
                },
            );
        }
        Ok(Self {
            status_file: config
                .status_file
                .clone()
                .map(|file| hub::relative_to(path, file))
                .unwrap_or_else(default_status_file),
            node_name: node_name.to_owned(),
            port,
            peers,
        })
    }

    pub fn runtime_key(&self) -> (&Path, &str, u16) {
        (&self.status_file, &self.node_name, self.port)
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn has_peer(&self, host: &str) -> bool {
        self.peers.contains_key(host)
    }

    pub fn configured_hosts(&self) -> Vec<String> {
        let mut hosts: Vec<_> = self.peers.keys().cloned().collect();
        hosts.sort();
        hosts
    }

    pub fn authenticate(&self, header: &[u8]) -> Option<http::SenderIdentity> {
        let mut identity = None;
        for (host, peer) in &self.peers {
            if peer.token.authorizes(header) {
                identity = Some(http::SenderIdentity::HostPrefix(host.clone()));
            }
        }
        identity
    }

    pub fn token_conflicts(&self, token: &Token) -> bool {
        self.peers
            .values()
            .any(|peer| peer.token.bytes() == token.bytes())
    }

    pub fn local_ip(&self) -> Option<Ipv4Addr> {
        self.status()
            .filter(|status| status.node_name == self.node_name)
            .map(|status| status.node_ip)
    }

    pub fn target(&self, host: &str) -> Option<(String, Token)> {
        let peer = self.peers.get(host)?;
        let status = self.status()?;
        if status.node_name != self.node_name {
            return None;
        }
        let (ip, age) = status.peers.get(&peer.mesh_name)?;
        if age.is_none_or(|age| age > HANDSHAKE_MAX_AGE) {
            return None;
        }
        Some((
            SocketAddrV4::new(*ip, peer.port).to_string(),
            peer.token.clone(),
        ))
    }

    pub fn targets(&self) -> Vec<(String, String, Token)> {
        self.peers
            .keys()
            .filter_map(|host| {
                self.target(host)
                    .map(|(address, token)| (host.clone(), address, token))
            })
            .collect()
    }

    fn status(&self) -> Option<Status> {
        let metadata = fs::metadata(&self.status_file).ok()?;
        if !metadata.is_file()
            || metadata.len() > STATUS_MAX_BYTES
            || metadata.modified().ok()?.elapsed().ok()? > STATUS_MAX_AGE
        {
            return None;
        }
        parse_status(&fs::read_to_string(&self.status_file).ok()?)
    }
}

fn parse_status(text: &str) -> Option<Status> {
    let mut node = None;
    let mut peers = HashMap::new();
    for line in text.lines() {
        let fields: Vec<_> = line.split_whitespace().collect();
        match fields.as_slice() {
            ["node", name, ip, ..] => {
                let ip: Ipv4Addr = ip.parse().ok()?;
                if ip.is_unspecified() || ip.is_multicast() || node.is_some() {
                    return None;
                }
                node = Some(((*name).to_owned(), ip));
            }
            ["peer", name, ip, ..] => {
                let ip: Ipv4Addr = ip.parse().ok()?;
                if ip.is_unspecified() || ip.is_multicast() {
                    return None;
                }
                let age = fields
                    .windows(2)
                    .find(|pair| pair[0] == "handshake")
                    .and_then(|pair| pair[1].strip_suffix('s'))
                    .and_then(|age| age.parse::<u64>().ok());
                if peers.insert((*name).to_owned(), (ip, age)).is_some() {
                    return None;
                }
            }
            _ => {}
        }
    }
    let (node_name, node_ip) = node?;
    Some(Status {
        node_name,
        node_ip,
        peers,
    })
}

/// A failed preflight has not sent a message, so the caller may use its fallback route.
pub fn reachable(address: &str) -> bool {
    let Ok(addr) = address.parse::<SocketAddr>() else {
        return false;
    };
    let Ok(mut stream) = TcpStream::connect_timeout(&addr, PROBE_TIMEOUT) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(PROBE_TIMEOUT));
    let _ = stream.set_write_timeout(Some(PROBE_TIMEOUT));
    if stream
        .write_all(b"GET / HTTP/1.1\r\nHost: mesh\r\nConnection: close\r\n\r\n")
        .is_err()
    {
        return false;
    }
    let mut response = String::new();
    stream.take(512).read_to_string(&mut response).is_ok()
        && response.starts_with("HTTP/1.1 200 ")
        && response.contains("agent-inbox ok")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_status_and_ignores_extensions() {
        let status = parse_status("node alpha 10.249.0.1 id 1 extra\nrelay sh ignored\npeer beta 10.249.0.2 direct x handshake 7s tx 1 rx 2 extra\npeer gamma 10.249.0.3 none handshake never\n").unwrap();
        assert_eq!(status.node_name, "alpha");
        assert_eq!(status.node_ip, Ipv4Addr::new(10, 249, 0, 1));
        assert_eq!(status.peers["beta"].1, Some(7));
        assert_eq!(status.peers["gamma"].1, None);
        assert!(parse_status("peer beta 10.249.0.2 handshake 7s").is_none());
    }
}
