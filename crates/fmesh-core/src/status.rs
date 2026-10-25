//! Parses meshd's status file (contract: docs/network.md section 5). Lines are keyword based;
//! unknown lines and extra fields are ignored, so older and newer meshd versions both work.

use serde::Serialize;
use std::time::{Duration, SystemTime};

/// WireGuard drops a session 180 s after its handshake, and keepalives (every 25 s) renew it
/// while the peer is up: an older or missing handshake means the peer is offline, even when
/// a path to it (e.g. a relay) is known.
const ONLINE_HANDSHAKE_SECS: u64 = 180;

/// meshd rewrites the file every 2 s; older than this means it is not running.
pub const STALE: Duration = Duration::from_secs(10);

/// The service agent-inbox announces through meshd.
pub const INBOX: &str = "agent-inbox";

#[cfg(windows)]
pub const STATUS_FILE: &str = r"C:\ProgramData\flytoMesh\status";
#[cfg(not(windows))]
pub const STATUS_FILE: &str = "/var/run/flytomesh.status";

#[derive(Default, PartialEq, Clone, Debug, Serialize)]
pub struct Status {
    /// meshd wrote the file within `STALE`. False also when there is no file at all.
    pub running: bool,
    pub me: String,
    pub ip: String,
    pub roster: String,
    pub release: String,
    pub relays: Vec<Relay>,
    pub peers: Vec<Peer>,
    pub services: Vec<Service>,
}

#[derive(PartialEq, Clone, Debug, Serialize)]
pub struct Relay {
    pub name: String,
    pub up: bool,
    pub rtt: String,
}

/// How packets reach a peer, as meshd reports it.
#[derive(PartialEq, Clone, Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Path {
    Direct { lan: bool },
    Relay { relay: String },
    None,
}

#[derive(PartialEq, Clone, Debug, Serialize)]
pub struct Peer {
    pub name: String,
    pub ip: String,
    pub path: Path,
    pub online: bool,
    /// Seconds since the last handshake.
    pub handshake: Option<u64>,
}

impl Peer {
    /// "直连", "直连 (局域网)", "经中继 sh", "无路径", or "离线" when it is not online.
    pub fn path_text(&self) -> String {
        if !self.online {
            return "离线".into();
        }
        match &self.path {
            Path::Direct { lan: true } => "直连 (局域网)".into(),
            Path::Direct { lan: false } => "直连".into(),
            Path::Relay { relay } => format!("经中继 {relay}"),
            Path::None => "无路径".into(),
        }
    }
}

/// A fresh service announcement: from this node (`local`) or an online peer.
#[derive(PartialEq, Clone, Debug, Serialize)]
pub struct Service {
    pub node: String,
    pub name: String,
    pub port: u16,
    pub local: bool,
}

fn after<'a>(t: &[&'a str], key: &str) -> Option<&'a str> {
    t.iter().position(|x| *x == key).and_then(|i| t.get(i + 1)).copied()
}

/// Reads and parses `path`; a missing or stale file gives a status that claims nothing.
pub fn read(path: &std::path::Path) -> (Status, String) {
    let fresh = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .is_some_and(|age| age < STALE);
    let Ok(text) = std::fs::read_to_string(path) else { return (Status::default(), String::new()) };
    (parse(&text, fresh), text)
}

pub fn parse(text: &str, fresh: bool) -> Status {
    let mut s = Status { running: fresh, ..Default::default() };
    for line in text.lines() {
        let t: Vec<&str> = line.split_whitespace().collect();
        match t.as_slice() {
            ["node", name, ip, rest @ ..] => {
                s.me = name.to_string();
                s.ip = ip.to_string();
                s.roster = after(rest, "roster").unwrap_or("").to_string();
                s.release = after(rest, "release").unwrap_or("").to_string();
            }
            ["relay", name, _, state, "rtt", rtt, ..] => s.relays.push(Relay { name: name.to_string(), up: *state == "up", rtt: rtt.to_string() }),
            ["peer", name, ip, kind, rest @ ..] => {
                let path = match *kind {
                    "direct" => {
                        let connected = rest.get(1) != Some(&"(unconnected)");
                        let lan = connected
                            && rest.first().is_some_and(|a| a.starts_with("10.") || a.starts_with("192.168.") || a.starts_with("172."));
                        Path::Direct { lan }
                    }
                    "relay" => Path::Relay { relay: rest.first().unwrap_or(&"?").to_string() },
                    _ => Path::None,
                };
                let handshake = after(rest, "handshake").and_then(|h| h.strip_suffix('s')).and_then(|h| h.parse::<u64>().ok());
                let online = handshake.is_some_and(|age| age < ONLINE_HANDSHAKE_SECS);
                s.peers.push(Peer { name: name.to_string(), ip: ip.to_string(), path, online, handshake });
            }
            ["service", node, _ip, rest @ ..] => {
                let (Some(name), Some(port)) = (after(rest, "name"), after(rest, "port").and_then(|p| p.parse().ok())) else { continue };
                let expires = after(rest, "expires").and_then(|e| e.strip_suffix('s')).and_then(|e| e.parse::<u64>().ok());
                if expires.is_some_and(|e| e > 0) {
                    s.services.push(Service { node: node.to_string(), name: name.to_string(), port, local: after(rest, "uid").is_some() });
                }
            }
            _ => {}
        }
    }
    // A stale file is what meshd last saw before it stopped: nothing in it holds any more.
    if !fresh {
        s.peers.iter_mut().for_each(|p| p.online = false);
        s.relays.iter_mut().for_each(|r| r.up = false);
        s.services.clear();
    }
    // A peer's announcement only counts while the peer is online.
    let offline: Vec<String> = s.peers.iter().filter(|p| !p.online).map(|p| p.name.clone()).collect();
    s.services.retain(|x| x.local || !offline.contains(&x.node));
    s
}

impl Status {
    pub fn online(&self) -> usize {
        self.peers.iter().filter(|p| p.online).count()
    }

    pub fn relays_up(&self) -> usize {
        self.relays.iter().filter(|r| r.up).count()
    }

    /// The fresh announcement of `service` by `node`, if any.
    pub fn service(&self, node: &str, service: &str) -> Option<&Service> {
        self.services.iter().find(|x| x.node == node && x.name == service)
    }

    /// Nodes (this one included) with a fresh agent-inbox announcement.
    pub fn inbox_nodes(&self) -> usize {
        self.services.iter().filter(|x| x.name == INBOX).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "node laptop1 10.249.0.1 id 1 roster v14 release v8 (macos-aarch64) key T2ny=\n\
        relay sh 203.0.113.10:41900 up rtt 42ms sees-us 1.2.3.4:1027\n\
        relay hk 203.0.113.20:41900 down rtt - sees-us -\n\
        peer laptop2 10.249.0.2 direct 10.0.0.1:41820 handshake 51s tx 0 rx 0 id 2 key J/Mq=\n\
        peer nas 10.249.0.9 relay sh handshake 153s tx 0 rx 0 id 9 key s0g4=\n\
        peer desk 10.249.0.10 relay hk handshake never tx 0 rx 0 id 10 key Jxq=\n\
        peer old 10.249.0.13 relay hk handshake 400s tx 0 rx 0\n\
        service laptop1 10.249.0.1 id 1 name agent-inbox port 8832 ident abc expires 72s uid 501\n\
        service laptop2 10.249.0.2 id 2 name agent-inbox port 8832 ident - expires 105s\n\
        service old 10.249.0.13 id 13 name agent-inbox port 8832 ident - expires 20s\n\
        service nas 10.249.0.9 id 9 name other port 1 ident - expires 0s\n\
        future line type we do not know\n";

    #[test]
    fn offline_peers_are_not_counted_as_reachable() {
        let s = parse(SAMPLE, true);
        let got: Vec<(&str, bool, String)> = s.peers.iter().map(|p| (p.name.as_str(), p.online, p.path_text())).collect();
        assert_eq!(
            got,
            [("laptop2", true, "直连 (局域网)".into()), ("nas", true, "经中继 sh".into()), ("desk", false, "离线".into()), ("old", false, "离线".into())]
        );
        assert_eq!((s.online(), s.relays_up(), s.relays.len()), (2, 1, 2));
        assert_eq!((s.roster.as_str(), s.release.as_str()), ("v14", "v8"));
    }

    #[test]
    fn counts_fresh_inbox_announcements_of_online_nodes_only() {
        let s = parse(SAMPLE, true);
        // laptop1 (local) and laptop2 count; `old` is offline, nas's entry expired.
        assert_eq!(s.inbox_nodes(), 2);
        assert!(s.service("laptop1", INBOX).is_some_and(|x| x.local && x.port == 8832));
        assert!(s.service("laptop2", INBOX).is_some_and(|x| !x.local));
        assert!(s.service("old", INBOX).is_none());
        assert!(s.service("nas", "other").is_none());
    }

    #[test]
    fn a_stale_file_claims_nothing() {
        let s = parse(SAMPLE, false);
        assert!(!s.running);
        assert_eq!((s.online(), s.relays_up(), s.inbox_nodes()), (0, 0, 0));
        assert!(s.peers.iter().all(|p| p.path_text() == "离线"));
    }

    #[test]
    fn old_status_format_still_parses() {
        let s = parse("node laptop1 10.249.0.1 id 1 roster v12 release v6 (macos-aarch64)\npeer laptop2 10.249.0.2 direct 10.0.0.1:41820 handshake 5s tx 0 rx 0\n", true);
        assert_eq!((s.me.as_str(), s.online(), s.inbox_nodes()), ("laptop1", 1, 0));
    }
}
