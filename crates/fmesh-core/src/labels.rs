//! Device labels the user maintains by hand in `devices.json`: who uses a device (a person,
//! a team or a role), which hosts are shared servers, and optionally how to query the agent
//! directory from a desktop without its own relay. Display data only, never an ACL.

use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

/// Bigger files are ignored rather than parsed.
const MAX_FILE: usize = 64 * 1024;
const MAX_LABEL: usize = 512;

#[derive(Default, Debug, Clone, PartialEq, Serialize)]
pub struct Labels {
    pub owners: BTreeMap<String, String>,
    pub shared_hosts: BTreeSet<String>,
    /// An explicit directory command (argv, no shell), e.g. over an existing SSH identity.
    pub directory_command: Option<Vec<String>>,
    /// The file exists but could not be read or parsed; the UI says so.
    pub error: bool,
}

/// `~/.config/flytomesh/devices.json` (honouring `XDG_CONFIG_HOME`), or
/// `%APPDATA%\flytomesh\devices.json` on Windows. The directory name stays `flytomesh` so
/// existing installations keep their labels.
pub fn path() -> PathBuf {
    #[cfg(windows)]
    let base = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_default();
    #[cfg(not(windows))]
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config"));
    base.join("flytomesh").join("devices.json")
}

pub fn load() -> Labels {
    match std::fs::read(path()) {
        Ok(bytes) => parse(&bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Labels::default(),
        Err(_) => Labels { error: true, ..Default::default() },
    }
}

pub fn parse(bytes: &[u8]) -> Labels {
    let mut l = Labels::default();
    // Windows editors may add a BOM.
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    if bytes.len() > MAX_FILE {
        return Labels { error: true, ..Default::default() };
    }
    let config = match serde_json::from_slice::<serde_json::Value>(bytes) {
        Ok(v) if v.is_object() => v,
        _ => return Labels { error: true, ..Default::default() },
    };
    if let Some(owners) = config["owners"].as_object() {
        for (name, owner) in owners {
            if let Some(owner) = owner.as_str().filter(|s| !s.is_empty() && s.len() <= MAX_LABEL) {
                l.owners.insert(name.clone(), owner.into());
            }
        }
    }
    if let Some(hosts) = config["shared_hosts"].as_array() {
        l.shared_hosts.extend(hosts.iter().filter_map(|h| h.as_str().map(str::to_owned)));
    }
    l.directory_command = config["directory_command"]
        .as_array()
        .and_then(|args| args.iter().map(|s| s.as_str().map(str::to_owned)).collect::<Option<Vec<_>>>());
    l
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_owners_shared_hosts_and_command() {
        let l = parse(
            "\u{feff}{\"owners\":{\"laptop1\":\"成员 A\",\"bad\":7,\"empty\":\"\"},\"shared_hosts\":[\"server1\"],\"directory_command\":[\"ssh\",\"h\",\"agent-inbox who --local --json\"]}"
                .as_bytes(),
        );
        assert!(!l.error);
        assert_eq!(l.owners.len(), 1);
        assert_eq!(l.owners["laptop1"], "成员 A");
        assert!(l.shared_hosts.contains("server1"));
        assert_eq!(l.directory_command.as_deref().map(<[String]>::len), Some(3));
    }

    #[test]
    fn a_broken_file_is_an_error_not_empty_labels() {
        assert!(parse(b"{not json").error);
        assert!(parse(b"[1,2]").error);
        let big = format!("{{\"owners\":{{\"a\":\"{}\"}}}}", "x".repeat(MAX_FILE));
        assert!(parse(big.as_bytes()).error);
    }
}
