//! Parses meshd's status file (contract: docs/design.md 7b). Lines are keyword based; unknown
//! lines and extra fields are ignored, so older and newer meshd versions both work.

/// WireGuard drops a session 180 s after its handshake, and keepalives (every 25 s) renew it
/// while the peer is up: an older or missing handshake means the peer is offline, even when
/// a path to it (e.g. a relay) is known.
const ONLINE_HANDSHAKE_SECS: u64 = 180;

/// The service the tray counts (agent-inbox announces itself through meshd).
pub const INBOX: &str = "agent-inbox";

#[derive(Default, PartialEq, Clone, Debug)]
pub struct Status {
    pub running: bool,
    pub me: String,
    pub ip: String,
    pub roster: String,
    pub release: String,
    pub relays: Vec<Relay>,
    pub peers: Vec<Peer>,
    pub services: Vec<Service>,
}

#[derive(PartialEq, Clone, Debug)]
pub struct Relay {
    pub name: String,
    pub up: bool,
    pub rtt: String,
}

#[derive(PartialEq, Clone, Debug)]
pub struct Peer {
    pub name: String,
    pub ip: String,
    /// Human-readable path ("直连", "经中继 sh", "离线").
    pub path: String,
    pub online: bool,
    /// Seconds since the last handshake.
    pub handshake: Option<u64>,
}

/// A fresh service announcement: from this node (`local`) or an online peer.
#[derive(PartialEq, Clone, Debug)]
pub struct Service {
    pub node: String,
    pub name: String,
    pub port: u16,
    pub local: bool,
}

fn after<'a>(t: &[&'a str], key: &str) -> Option<&'a str> {
    t.iter().position(|x| *x == key).and_then(|i| t.get(i + 1)).copied()
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
                    "direct" if rest.get(1) == Some(&"(unconnected)") => "直连".to_string(),
                    "direct" => {
                        let lan = rest.first().is_some_and(|a| a.starts_with("10.") || a.starts_with("192.168.") || a.starts_with("172."));
                        if lan { "直连 (局域网)".into() } else { "直连".into() }
                    }
                    "relay" => format!("经中继 {}", rest.first().unwrap_or(&"?")),
                    _ => "无路径".into(),
                };
                let handshake = after(rest, "handshake").and_then(|h| h.strip_suffix('s')).and_then(|h| h.parse::<u64>().ok());
                let online = handshake.is_some_and(|age| age < ONLINE_HANDSHAKE_SECS);
                let path = if online { path } else { "离线".into() };
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
        for p in &mut s.peers {
            p.online = false;
            p.path = "离线".into();
        }
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
        let got: Vec<(&str, bool, &str)> = s.peers.iter().map(|p| (p.name.as_str(), p.online, p.path.as_str())).collect();
        assert_eq!(
            got,
            [("laptop2", true, "直连 (局域网)"), ("nas", true, "经中继 sh"), ("desk", false, "离线"), ("old", false, "离线")]
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
        assert!(s.peers.iter().all(|p| p.path == "离线"));
    }

    #[test]
    fn old_status_format_still_parses() {
        let s = parse("node laptop1 10.249.0.1 id 1 roster v12 release v6 (macos-aarch64)\npeer laptop2 10.249.0.2 direct 10.0.0.1:41820 handshake 5s tx 0 rx 0\n", true);
        assert_eq!((s.me.as_str(), s.online(), s.inbox_nodes()), ("laptop1", 1, 0));
    }
}
