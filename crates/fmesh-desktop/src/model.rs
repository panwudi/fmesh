//! What the list shows, independent of the toolkit: which devices are visible and expanded,
//! what a copy puts on the clipboard and what the status line says.

use fmesh_core::view::{Filter, View};

pub const FILTERS: [(&str, Filter); 4] =
    [("全部", Filter::All), ("在线", Filter::Online), ("离线", Filter::Offline), ("有 Agent 服务", Filter::AgentService)];
pub const NOTE: &str = "\"未发现\" 不代表没装; \"可接收\" 不代表有人在电脑前, 也不代表模型已处理消息. 归属是本机登记的展示信息, 不是权限.";

/// A visible row: a device, or one of its agents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Row {
    pub device: usize,
    pub agent: Option<usize>,
}

/// Indices of the devices that pass the filter and the search.
pub fn visible(view: &View, query: &str, filter: Filter) -> Vec<usize> {
    (0..view.devices.len()).filter(|&i| view.devices[i].passes(filter) && view.devices[i].matches(query)).collect()
}

/// Devices whose agents the search hit; the window expands them.
pub fn expand(view: &View, query: &str) -> Vec<usize> {
    (0..view.devices.len()).filter(|&i| view.devices[i].matches_agent(query)).collect()
}

/// What a copy of this row puts on the clipboard: `<name>.mesh` or the agent address.
pub fn copy_text(view: &View, row: Row) -> String {
    let d = &view.devices[row.device];
    match row.agent {
        None => d.mesh_name.clone(),
        Some(a) => d.agent_list()[a].name.clone(),
    }
}

/// Feedback after a copy; a shared host's agent reminds how it may be contacted.
pub fn copied(view: &View, row: Row) -> String {
    let text = copy_text(view, row);
    if view.devices[row.device].shared && row.agent.is_some() {
        format!("已复制 {text}. 请交给自己的 Agent 转交, 不要直接联系共享 Agent.")
    } else {
        format!("已复制 {text}")
    }
}

/// The line above the list.
pub fn summary(view: &View) -> String {
    let s = &view.summary;
    let mut line = s.lines().join("  ·  ");
    if s.network {
        line.push_str(&format!("  ·  名单 {} · 版本 {}", s.roster, s.release));
    }
    if view.labels_error {
        line.push_str("  ·  设备归属配置读取失败");
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use fmesh_core::{directory, labels, status, view};

    fn sample() -> View {
        let s = status::parse(
            "node laptop1 10.249.0.1 id 1 roster v3 release v9 (macos-aarch64) key K=\n\
             relay r1 203.0.113.10:41900 up rtt 9ms sees-us -\n\
             peer server1 10.249.0.3 relay r1 handshake 20s tx 0 rx 0 id 3 key S=\n\
             peer desk 10.249.0.5 relay r1 handshake never tx 0 rx 0 id 5 key D=\n\
             service laptop1 10.249.0.1 id 1 name agent-inbox port 8832 ident - expires 60s uid 501\n\
             service server1 10.249.0.3 id 3 name agent-inbox port 8832 ident - expires 60s\n",
            true,
        );
        let l = labels::parse("{\"owners\":{\"laptop1\":\"成员 A\"},\"shared_hosts\":[\"server1\"]}".as_bytes());
        let d = directory::parse(br#"[{"name":"laptop1.main","description":"review","status":"online"},{"name":"server1.build","description":"","status":"online"}]"#);
        view::build(&s, &l, &d)
    }

    #[test]
    fn copies_and_feedback() {
        let v = sample();
        assert_eq!(copy_text(&v, Row { device: 1, agent: None }), "server1.mesh");
        assert_eq!(copied(&v, Row { device: 0, agent: Some(0) }), "已复制 laptop1.main");
        assert!(copied(&v, Row { device: 1, agent: Some(0) }).contains("交给自己的 Agent"));
    }

    #[test]
    fn search_filters_and_expands() {
        let v = sample();
        assert_eq!(visible(&v, "review", Filter::All), [0]);
        assert_eq!(expand(&v, "review"), [0]);
        assert_eq!(visible(&v, "", Filter::Offline), [2]);
        assert!(expand(&v, "laptop1.mesh").is_empty());
        assert!(summary(&v).contains("名单 v3"));
    }
}
