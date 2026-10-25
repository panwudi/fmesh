//! One list grouped by computer, built from the three sources. Every UI (CLI, tray, native
//! window) renders this; the wording rules live here so they cannot drift apart:
//!
//! - a failed directory query is "unavailable", never zero agents;
//! - no announcement is "not discovered", never "not installed";
//! - owner labels are explicit display data, "未填写" when absent, never guessed from names;
//! - shared hosts are contacted through a member's own agent; the mark grants nothing;
//! - "receiving" says nothing about a person being present or a model having acted.

use crate::directory::{AgentRow, Directory};
use crate::labels::Labels;
use crate::status::{Status, INBOX};
use serde::Serialize;

pub const SHARED_NOTE: &str = "共享 Agent: 请通过成员自己的 Agent 联系, 此标记不代表任何授权";

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct View {
    pub summary: Summary,
    pub devices: Vec<Device>,
    /// devices.json exists but could not be read.
    pub labels_error: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Summary {
    /// meshd is running (fresh status file).
    pub network: bool,
    pub me: String,
    pub ip: String,
    pub relays_up: usize,
    pub relays: usize,
    pub others_online: usize,
    pub others: usize,
    /// Devices, this one included, that announce the agent service.
    pub agent_devices: usize,
    pub roster: String,
    pub release: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Link {
    Online,
    Offline,
    /// This device, with meshd not running.
    NetworkStopped,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Device {
    pub name: String,
    pub mesh_name: String,
    pub ip: String,
    pub me: bool,
    pub link: Link,
    pub path: String,
    pub handshake_secs: Option<u64>,
    pub owner: Option<String>,
    pub shared: bool,
    /// Port of a fresh agent service announcement.
    pub agent_port: Option<u16>,
    pub agents: Agents,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Agents {
    Listed { agents: Vec<Agent> },
    DeviceOffline,
    NotDiscovered,
    Unavailable { reason: String },
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Agent {
    /// Full address, `host.agent`.
    pub name: String,
    /// Self-reported role; may be empty.
    pub role: String,
    pub receiving: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Filter {
    All,
    Online,
    Offline,
    AgentService,
}

impl Device {
    pub fn online(&self) -> bool {
        self.link == Link::Online
    }

    pub fn state_text(&self) -> &'static str {
        match self.link {
            Link::Online => "在线",
            Link::Offline => "离线",
            Link::NetworkStopped => "组网未运行",
        }
    }

    pub fn owner_text(&self) -> &str {
        self.owner.as_deref().unwrap_or("未填写")
    }

    pub fn handshake_text(&self) -> String {
        if self.me {
            return "-".into();
        }
        match self.handshake_secs {
            None => "从未".into(),
            Some(s) if s < 120 => format!("{s} 秒前"),
            Some(s) if s < 7200 => format!("{} 分钟前", s / 60),
            Some(s) => format!("{} 小时前", s / 3600),
        }
    }

    pub fn service_text(&self) -> String {
        match self.agent_port {
            Some(p) => format!("Agent 服务可用 · 端口 {p}"),
            None => "未发现 Agent 服务".into(),
        }
    }

    /// One line about the agent list, e.g. "2 个可接收 / 3 个登记会话".
    pub fn agents_text(&self) -> String {
        match &self.agents {
            Agents::Listed { agents } => {
                let receiving = agents.iter().filter(|a| a.receiving).count();
                format!("{receiving} 个可接收 / {} 个登记会话", agents.len())
            }
            Agents::DeviceOffline => "设备离线, Agent 状态未知".into(),
            Agents::NotDiscovered => "未发现 Agent 服务公告".into(),
            Agents::Unavailable { reason } => reason.clone(),
        }
    }

    pub fn agent_list(&self) -> &[Agent] {
        match &self.agents {
            Agents::Listed { agents } => agents,
            _ => &[],
        }
    }

    /// Case-insensitive match on device, mesh name, address, owner, path, state, agent names
    /// and roles. An empty query matches everything.
    pub fn matches(&self, query: &str) -> bool {
        let q = query.trim().to_lowercase();
        if q.is_empty() {
            return true;
        }
        let mut hay = format!("{} {} {} {} {} {}", self.name, self.mesh_name, self.ip, self.owner_text(), self.path, self.state_text());
        if self.shared {
            hay.push_str(" 共享 shared");
        }
        for a in self.agent_list() {
            hay.push(' ');
            hay.push_str(&a.name);
            hay.push(' ');
            hay.push_str(&a.role);
        }
        hay.to_lowercase().contains(&q)
    }

    /// Whether the query matched an agent rather than only the device (UIs expand those).
    pub fn matches_agent(&self, query: &str) -> bool {
        let q = query.trim().to_lowercase();
        !q.is_empty() && self.agent_list().iter().any(|a| format!("{} {}", a.name, a.role).to_lowercase().contains(&q))
    }

    pub fn passes(&self, filter: Filter) -> bool {
        match filter {
            Filter::All => true,
            Filter::Online => self.online(),
            Filter::Offline => !self.online(),
            Filter::AgentService => self.agent_port.is_some(),
        }
    }
}

impl Summary {
    /// The tray's fixed lines.
    pub fn lines(&self) -> [String; 3] {
        if !self.network {
            return ["组网未运行".into(), "其他设备在线: -".into(), "Agent: -".into()];
        }
        let relays =
            if self.relays_up > 0 { format!("已连接 (中继 {}/{})", self.relays_up, self.relays) } else { "中继全部断开".into() };
        let agents = match self.agent_devices {
            0 => "Agent: 未发现可用服务".to_string(),
            n => format!("Agent: {n} 台可用"),
        };
        [format!("{} {} · {relays}", self.me, self.ip), format!("其他设备在线: {}/{} 台", self.others_online, self.others), agents]
    }
}

impl View {
    pub fn visible(&self, query: &str, filter: Filter) -> Vec<&Device> {
        self.devices.iter().filter(|d| d.passes(filter) && d.matches(query)).collect()
    }
}

/// `hosts`: the relay host prefixes that belong to this device. This device also takes rows
/// from hosts no mesh device is named after (its relay may use another name than meshd).
fn agents_for(hosts: &[&str], online: bool, port: Option<u16>, dir: &Directory) -> Agents {
    if !online {
        return Agents::DeviceOffline;
    }
    if port.is_none() {
        return Agents::NotDiscovered;
    }
    let rows: &[AgentRow] = match dir {
        Directory::Unavailable { reason } => return Agents::Unavailable { reason: reason.clone() },
        Directory::Listed { rows } => rows,
    };
    let mine = |r: &&AgentRow| r.name.split_once('.').is_some_and(|(h, _)| hosts.contains(&h));
    if rows.iter().filter(mine).any(|r| r.host_marker) {
        return Agents::Unavailable { reason: "名单暂不可用: 对端目录未响应".into() };
    }
    let agents = rows
        .iter()
        .filter(mine)
        .map(|r| Agent { name: r.name.clone(), role: r.description.clone(), receiving: r.receiving })
        .collect();
    Agents::Listed { agents }
}

fn local_host(dir: &Directory) -> Option<String> {
    let Directory::Listed { rows } = dir else { return None };
    let mut hosts = rows.iter().filter(|r| !r.host_marker).filter_map(|r| r.name.split_once('.').map(|(h, _)| h));
    let first = hosts.next()?;
    hosts.all(|h| h == first).then(|| first.to_owned())
}

pub fn build(s: &Status, labels: &Labels, dir: &Directory) -> View {
    let port = |node: &str| s.service(node, INBOX).map(|x| x.port);
    let mut devices = Vec::with_capacity(s.peers.len() + 1);
    let me_link = if s.running { Link::Online } else { Link::NetworkStopped };
    // Agent-only machines have no status file: take this host's name from the local
    // directory when it names exactly one host.
    let me_name = if s.me.is_empty() { local_host(dir).unwrap_or_else(|| "本机".into()) } else { s.me.clone() };
    let mut known: Vec<&str> = s.peers.iter().map(|p| p.name.as_str()).collect();
    known.push(&me_name);
    let mut my_hosts = vec![me_name.as_str()];
    if let Directory::Listed { rows } = dir {
        for h in rows.iter().filter_map(|r| r.name.split_once('.').map(|(h, _)| h)) {
            if !known.contains(&h) && !my_hosts.contains(&h) {
                my_hosts.push(h);
            }
        }
    }
    // The local directory answers for this device whether or not it announces the service.
    let me_agents = agents_for(&my_hosts, true, Some(0), dir);
    devices.push(Device {
        mesh_name: format!("{me_name}.mesh"),
        ip: s.ip.clone(),
        me: true,
        link: me_link,
        path: if s.running { "本机".into() } else { "组网未运行".into() },
        handshake_secs: None,
        owner: labels.owners.get(&s.me).cloned(),
        shared: labels.shared_hosts.contains(&s.me),
        agent_port: port(&s.me),
        agents: me_agents,
        name: me_name,
    });
    let mut peers: Vec<_> = s.peers.iter().collect();
    peers.sort_by(|a, b| b.online.cmp(&a.online).then(a.name.cmp(&b.name)));
    for p in peers {
        let link = if p.online { Link::Online } else { Link::Offline };
        devices.push(Device {
            name: p.name.clone(),
            mesh_name: format!("{}.mesh", p.name),
            ip: p.ip.clone(),
            me: false,
            link,
            path: p.path_text(),
            handshake_secs: p.handshake,
            owner: labels.owners.get(&p.name).cloned(),
            shared: labels.shared_hosts.contains(&p.name),
            agent_port: port(&p.name),
            agents: agents_for(&[p.name.as_str()], p.online, port(&p.name), dir),
        });
    }
    let summary = Summary {
        network: s.running,
        me: s.me.clone(),
        ip: s.ip.clone(),
        relays_up: s.relays_up(),
        relays: s.relays.len(),
        others_online: s.online(),
        others: s.peers.len(),
        agent_devices: s.inbox_nodes(),
        roster: s.roster.clone(),
        release: s.release.clone(),
    };
    View { summary, devices, labels_error: labels.error }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{directory, labels, status};

    const STATUS: &str = "node laptop1 10.249.0.1 id 1 roster v14 release v9 (macos-aarch64) key K=\n\
        relay r1 203.0.113.10:41900 up rtt 20ms sees-us -\n\
        peer laptop2 10.249.0.2 direct 10.0.0.1:41820 handshake 5s tx 0 rx 0 id 2 key J=\n\
        peer server1 10.249.0.3 relay r1 handshake 20s tx 0 rx 0 id 3 key S=\n\
        peer nas 10.249.0.4 relay r1 handshake 30s tx 0 rx 0 id 4 key N=\n\
        peer desk 10.249.0.5 relay r1 handshake never tx 0 rx 0 id 5 key D=\n\
        service laptop1 10.249.0.1 id 1 name agent-inbox port 8832 ident - expires 60s uid 501\n\
        service laptop2 10.249.0.2 id 2 name agent-inbox port 8832 ident - expires 60s\n\
        service server1 10.249.0.3 id 3 name agent-inbox port 8840 ident - expires 60s\n";

    const DIR: &[u8] = br#"[
        {"name":"laptop1.main","description":"integration review","status":"online"},
        {"name":"laptop2.ops","description":"","status":"offline","inbox_reachable":false},
        {"name":"server1.*","host_marker":true,"status":"offline"}]"#;

    fn view(dir: &Directory) -> View {
        let labels = labels::parse("{\"owners\":{\"laptop1\":\"成员 A\"},\"shared_hosts\":[\"server1\"]}".as_bytes());
        build(&status::parse(STATUS, true), &labels, dir)
    }

    fn by<'a>(v: &'a View, name: &str) -> &'a Device {
        v.devices.iter().find(|d| d.name == name).unwrap()
    }

    #[test]
    fn groups_agents_by_computer_with_explicit_states() {
        let v = view(&directory::parse(DIR));
        let names: Vec<_> = v.devices.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["laptop1", "laptop2", "nas", "server1", "desk"], "this device first, then online, then offline");
        assert_eq!(by(&v, "laptop1").agents_text(), "1 个可接收 / 1 个登记会话");
        assert_eq!(by(&v, "laptop2").agents_text(), "0 个可接收 / 1 个登记会话");
        assert_eq!(by(&v, "nas").agents, Agents::NotDiscovered);
        assert_eq!(by(&v, "nas").service_text(), "未发现 Agent 服务", "not 'not installed'");
        assert!(matches!(by(&v, "server1").agents, Agents::Unavailable { .. }), "host marker: unknown, not zero");
        assert_eq!(by(&v, "desk").agents, Agents::DeviceOffline);
        assert_eq!(v.summary.lines()[2], "Agent: 3 台可用");
    }

    #[test]
    fn a_failed_directory_never_reads_as_zero_agents() {
        let v = view(&Directory::Unavailable { reason: "名单暂不可用".into() });
        for d in v.devices.iter().filter(|d| d.online() && d.agent_port.is_some()) {
            assert!(matches!(d.agents, Agents::Unavailable { .. }), "{}", d.name);
            assert!(!d.agents_text().starts_with('0'));
        }
    }

    #[test]
    fn owners_are_explicit_and_shared_marks_grant_nothing() {
        let v = view(&directory::parse(DIR));
        assert_eq!(by(&v, "laptop1").owner_text(), "成员 A");
        assert_eq!(by(&v, "laptop2").owner_text(), "未填写", "never guessed from the host name");
        assert!(by(&v, "server1").shared);
        assert!(SHARED_NOTE.contains("成员自己的 Agent") && SHARED_NOTE.contains("不代表任何授权"));
    }

    #[test]
    fn search_covers_device_owner_agent_and_role() {
        let v = view(&directory::parse(DIR));
        let hits = |q| v.visible(q, Filter::All).iter().map(|d| d.name.clone()).collect::<Vec<_>>();
        assert_eq!(hits("REVIEW"), ["laptop1"]);
        assert_eq!(hits("成员 a"), ["laptop1"]);
        assert_eq!(hits("laptop2.ops"), ["laptop2"]);
        assert_eq!(hits("10.249.0.4"), ["nas"]);
        assert_eq!(hits("共享"), ["server1"]);
        assert!(by(&v, "laptop1").matches_agent("review") && !by(&v, "laptop1").matches_agent("laptop1.mesh"));
        assert_eq!(v.visible("", Filter::Offline).len(), 1);
        assert_eq!(v.visible("", Filter::AgentService).len(), 3);
    }

    #[test]
    fn stopped_network_still_lists_local_agents_and_claims_no_peers() {
        let v = build(&status::parse(STATUS, false), &Labels::default(), &directory::parse(DIR));
        assert_eq!(by(&v, "laptop1").link, Link::NetworkStopped);
        assert_eq!(by(&v, "laptop1").agent_list().len(), 1);
        assert!(v.devices.iter().filter(|d| !d.me).all(|d| d.agents == Agents::DeviceOffline));
        assert_eq!(v.summary.lines()[0], "组网未运行");
    }

    #[test]
    fn a_relay_named_unlike_the_node_still_lists_under_this_device() {
        let dir = directory::parse(br#"[{"name":"box.a","status":"online"},{"name":"laptop2.ops","status":"online"}]"#);
        let v = build(&status::parse(STATUS, true), &Labels::default(), &dir);
        assert_eq!(by(&v, "laptop1").agent_list().iter().map(|a| a.name.as_str()).collect::<Vec<_>>(), ["box.a"]);
        assert_eq!(by(&v, "laptop2").agent_list().len(), 1);
    }

    #[test]
    fn no_status_file_at_all_is_agent_only() {
        let local = br#"[{"name":"box.a","status":"online"},{"name":"box.b","status":"online"}]"#;
        let v = build(&Status::default(), &Labels::default(), &directory::parse(local));
        assert_eq!(v.devices.len(), 1);
        assert_eq!((v.devices[0].name.as_str(), v.devices[0].agent_list().len()), ("box", 2));
        assert_eq!(v.devices[0].link, Link::NetworkStopped);
        let v = build(&Status::default(), &Labels::default(), &Directory::Unavailable { reason: "x".into() });
        assert_eq!(v.devices[0].name, "本机");
    }
}
