use crate::{Result, hub, state};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Report {
    pub name: String,
    pub description: String,
    pub online: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Batch {
    agents: Vec<Report>,
}
#[derive(Clone, Serialize, Deserialize)]
struct Entry {
    description: String,
    last_seen: Option<String>,
}
#[derive(Clone, Serialize, Deserialize)]
struct Snapshot {
    version: u32,
    entries: BTreeMap<String, Entry>,
}
struct Seen {
    at: Instant,
    online: HashSet<String>,
}
pub struct Directory {
    path: PathBuf,
    data: Mutex<(Snapshot, HashMap<String, Seen>)>,
    ttl: Duration,
    _lock: File,
}

pub fn description(value: &str) -> Result<()> {
    if value.trim().is_empty()
        || value.len() > 512
        || value
            .chars()
            .any(|c| c.is_control() || c == '\u{2028}' || c == '\u{2029}')
    {
        return Err("description must be one nonempty line of at most 512 UTF-8 bytes".into());
    }
    Ok(())
}
impl Directory {
    pub fn open(path: PathBuf, hosts: &HashSet<String>, ttl: u64) -> Result<Self> {
        if !(1..=3600).contains(&ttl) {
            return Err("directory TTL must be 1..3600 seconds".into());
        }
        state::parent(&path)?;
        let lock = state::open_private(&path.with_extension("directory-lock"), true)?;
        lock.try_lock()
            .map_err(|_| "another hub owns this directory")?;
        let mut snapshot = if path.exists() {
            state::read_json::<Snapshot>(&path)?
        } else {
            Snapshot {
                version: 1,
                entries: BTreeMap::new(),
            }
        };
        if snapshot.version != 1 {
            return Err("unsupported directory state version".into());
        }
        snapshot
            .entries
            .retain(|name, _| hub::qualified(name).is_ok_and(|(host, _)| hosts.contains(host)));
        state::write_json(&path, &snapshot)?;
        // Liveness is never restored from disk. A fresh report is required after hub restart.
        Ok(Self {
            path,
            data: Mutex::new((snapshot, HashMap::new())),
            ttl: Duration::from_secs(ttl),
            _lock: lock,
        })
    }
    pub fn report(&self, host: &str, text: &str) -> Result<()> {
        let batch: Batch = serde_json::from_str(text).map_err(|_| "invalid directory report")?;
        if batch.agents.len() > 256 {
            return Err("too many reported agents".into());
        }
        let mut names = HashSet::new();
        for entry in &batch.agents {
            if hub::qualified(&entry.name)?.0 != host {
                return Err("directory name is outside reporting host prefix".into());
            }
            description(&entry.description)?;
            if !names.insert(entry.name.clone()) {
                return Err("duplicate directory name".into());
            }
        }
        let mut data = self.data.lock().map_err(|_| "directory lock failed")?;
        let mut next = data.0.clone();
        let prefix = format!("{host}.");
        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let mut online = HashSet::new();
        next.entries
            .retain(|name, _| !name.starts_with(&prefix) || names.contains(name));
        for entry in batch.agents {
            let previous = next
                .entries
                .get(&entry.name)
                .and_then(|entry| entry.last_seen.clone());
            let last_seen = if entry.online {
                online.insert(entry.name.clone());
                Some(now.clone())
            } else {
                previous
            };
            next.entries.insert(
                entry.name,
                Entry {
                    description: entry.description,
                    last_seen,
                },
            );
        }
        state::write_json(&self.path, &next)?;
        data.0 = next;
        data.1.insert(
            host.to_owned(),
            Seen {
                at: Instant::now(),
                online,
            },
        );
        Ok(())
    }
    pub fn page(&self, text: &str) -> Result<Value> {
        let query: Value = serde_json::from_str(text).map_err(|_| "invalid directory query")?;
        let cursor = match query.get("cursor") {
            None | Some(Value::Null) => 0,
            Some(value) => value
                .as_u64()
                .filter(|n| *n <= 65536)
                .ok_or("invalid directory cursor")? as usize,
        };
        let data = self.data.lock().map_err(|_| "directory lock failed")?;
        let rows:Vec<_>=data.0.entries.iter().skip(cursor).take(64).map(|(name,entry)|{
   let host=hub::qualified(name).ok().map(|(host,_)|host).unwrap_or("");
   let live=data.1.get(host).filter(|seen|seen.at.elapsed()<=self.ttl);
   let host_online=live.is_some();let inbox_online=live.is_some_and(|seen|seen.online.contains(name));
   json!({"name":name,"description":entry.description,"host_online":host_online,"inbox_reachable":inbox_online,"status":if inbox_online{"online"}else{"offline"},"last_seen":entry.last_seen})
  }).collect();
        let next = if cursor + rows.len() < data.0.entries.len() {
            Some(cursor + rows.len())
        } else {
            None
        };
        Ok(json!({"agents":rows,"nextCursor":next}))
    }
}
