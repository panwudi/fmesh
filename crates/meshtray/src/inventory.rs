//! Read-only agent directory and user-maintained device ownership labels.
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

#[derive(Default)]
pub struct Inventory {
    pub agents: Vec<serde_json::Value>,
    pub error: Option<String>,
    pub owners: BTreeMap<String, String>,
    pub shared_hosts: BTreeSet<String>,
    pub labels_error: bool,
}

pub fn config_path() -> PathBuf {
    #[cfg(windows)]
    let base = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_default();
    #[cfg(not(windows))]
    let base = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config"));
    base.join("flytomesh/devices.json")
}

pub fn owner(host: &str) -> Option<String> {
    let bytes = std::fs::read(config_path()).ok()?;
    if bytes.len() > 65536 { return None; }
    let config: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    config["owners"][host].as_str().filter(|s| s.len() <= 512).map(str::to_owned)
}

fn query(args: &[String]) -> Option<Vec<u8>> {
    use std::io::Read;
    use std::process::Stdio;
    use std::time::{Duration, Instant};
    let mut child = std::process::Command::new(args.first()?).args(&args[1..])
        .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().ok()?;
    let output = child.stdout.take()?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = output.take(1024 * 1024 + 1).read_to_end(&mut bytes);
        let _ = tx.send(result.ok().map(|_| bytes));
    });
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success().then(|| rx.recv_timeout(Duration::from_secs(1)).ok().flatten()).flatten(),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(25)),
            _ => { let _ = child.kill(); let _ = child.wait(); return None; }
        }
    }
}

pub fn load() -> Inventory {
    let mut inv = Inventory::default();
    let config = match std::fs::read(config_path()) {
        Ok(bytes) if bytes.len() <= 65536 => match serde_json::from_slice::<serde_json::Value>(&bytes) {
            Ok(value) if value.is_object() => value,
            _ => { inv.labels_error = true; serde_json::Value::Null }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => serde_json::Value::Null,
        _ => { inv.labels_error = true; serde_json::Value::Null }
    };
    if let Some(owners) = config["owners"].as_object() {
        for (name, owner) in owners {
            if let Some(owner) = owner.as_str().filter(|s| s.len() <= 512) {
                inv.owners.insert(name.clone(), owner.into());
            }
        }
    }
    if let Some(hosts) = config["shared_hosts"].as_array() {
        inv.shared_hosts.extend(hosts.iter().filter_map(|host| host.as_str().map(str::to_owned)));
    }
    // An explicit command supports desktops without their own local relay, using
    // an existing SSH identity. No shell evaluation or embedded sender tokens.
    let configured = config["directory_command"].as_array().and_then(|args| {
        args.iter().map(|s| s.as_str().map(str::to_owned)).collect::<Option<Vec<_>>>()
    });
    let default = || {
        let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
        vec![home.join(".local/bin/agent-inbox").to_string_lossy().into_owned(), "who".into(), "--local".into(), "--json".into()]
    };
    let args = configured.unwrap_or_else(default);
    if args.is_empty() {
        inv.error = Some("未配置会话目录来源".into());
        return inv;
    }
    match query(&args) {
        Some(bytes) if bytes.len() <= 1024 * 1024 => {
            match serde_json::from_slice::<Vec<serde_json::Value>>(&bytes) {
                Ok(rows) if rows.len() <= 4096 => inv.agents = rows,
                _ => inv.error = Some("会话目录返回格式无效".into()),
            }
        }
        _ => inv.error = Some("名单暂不可用: 本机目录接口未运行或查询失败".into()),
    }
    inv
}

impl Inventory {
    pub fn rows<'a>(&'a self, host: &str) -> Vec<&'a serde_json::Value> {
        self.agents.iter().filter(|a| a["name"].as_str().and_then(|s| s.split_once('.')).is_some_and(|(h, _)| h == host) && a["host_marker"] != true).collect()
    }

    pub fn available(&self, host: &str) -> bool {
        self.error.is_none() && !self.agents.iter().any(|a| a["host_marker"] == true && a["name"].as_str() == Some(&format!("{host}.*")))
    }
}
