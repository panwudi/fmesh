//! The agent directory: who is registered on which host, as agent-inbox reports it. Read
//! only: the query never registers, sends or touches sender tokens.

use serde::Serialize;
use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const MAX_REPLY: usize = 1024 * 1024;
const MAX_ROWS: usize = 4096;
const TIMEOUT: Duration = Duration::from_secs(8);

/// One directory row. `name` is `host.agent`; a `host.*` row with `host_marker` says that
/// host's roster could not be fetched.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AgentRow {
    pub name: String,
    pub description: String,
    pub receiving: bool,
    pub host_marker: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Directory {
    Listed { rows: Vec<AgentRow> },
    /// The query failed: unknown, not zero agents.
    Unavailable { reason: String },
}

/// The default command: the local relay's same-user directory. Desktops without one (e.g.
/// Windows today) need `directory_command` in devices.json.
pub fn default_command() -> Option<Vec<String>> {
    #[cfg(windows)]
    return None;
    #[cfg(not(windows))]
    {
        let home = std::path::PathBuf::from(std::env::var_os("HOME")?);
        let exe = home.join(".local/bin/agent-inbox");
        Some(vec![exe.to_string_lossy().into_owned(), "who".into(), "--local".into(), "--json".into()])
    }
}

pub fn query(command: Option<&[String]>) -> Directory {
    let default = default_command();
    let Some(args) = command.or(default.as_deref()).filter(|a| !a.is_empty()) else {
        return Directory::Unavailable { reason: "本机没有 Agent 目录来源 (可在 devices.json 配置 directory_command)".into() };
    };
    match run(args) {
        Some(bytes) => parse(&bytes),
        None => Directory::Unavailable { reason: "名单暂不可用: 本机目录接口未运行或查询失败".into() },
    }
}

pub fn parse(bytes: &[u8]) -> Directory {
    let bad = || Directory::Unavailable { reason: "会话目录返回格式无效".into() };
    let Ok(rows) = serde_json::from_slice::<Vec<serde_json::Value>>(bytes) else { return bad() };
    if rows.len() > MAX_ROWS {
        return bad();
    }
    let rows = rows
        .iter()
        .filter_map(|r| {
            let name = r["name"].as_str()?.to_owned();
            name.split_once('.')?;
            Some(AgentRow {
                description: r["description"].as_str().unwrap_or("").to_owned(),
                receiving: r["status"] == "online" && r["inbox_reachable"] != false && r["host_online"] != false,
                host_marker: r["host_marker"] == true,
                name,
            })
        })
        .collect();
    Directory::Listed { rows }
}

/// Runs `args` (argv, no shell) with a deadline and a bounded reply.
fn run(args: &[String]) -> Option<Vec<u8>> {
    let mut child = Command::new(&args[0])
        .args(&args[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let out = child.stdout.take()?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let ok = out.take(MAX_REPLY as u64 + 1).read_to_end(&mut bytes).is_ok();
        let _ = tx.send(ok.then_some(bytes));
    });
    let deadline = Instant::now() + TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let bytes = rx.recv_timeout(Duration::from_secs(1)).ok().flatten()?;
                return (status.success() && bytes.len() <= MAX_REPLY).then_some(bytes);
            }
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(25)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_keep_receiving_state_and_host_markers() {
        let d = parse(
            br#"[{"name":"laptop1.main","description":"review","status":"online"},
                 {"name":"laptop1.idle","status":"online","inbox_reachable":false},
                 {"name":"server1.*","host_marker":true,"status":"offline"},
                 {"name":"no-host-prefix"}]"#,
        );
        let Directory::Listed { rows } = d else { panic!("{d:?}") };
        let got: Vec<_> = rows.iter().map(|r| (r.name.as_str(), r.receiving, r.host_marker)).collect();
        assert_eq!(got, [("laptop1.main", true, false), ("laptop1.idle", false, false), ("server1.*", false, true)]);
    }

    #[test]
    fn a_failed_query_is_unavailable_not_empty() {
        assert!(matches!(parse(b"oops"), Directory::Unavailable { .. }));
        assert!(matches!(query(Some(&["/nonexistent/agent-inbox".to_string()])), Directory::Unavailable { .. }));
        assert_eq!(parse(b"[]"), Directory::Listed { rows: vec![] });
    }
}
