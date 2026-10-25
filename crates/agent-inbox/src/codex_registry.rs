//! Per-user inbox discovery and a startup barrier for maintenance.
use std::fs::File;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{Result, codex_queue::Queue, state};

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Record {
    pub instance: String,
    pub address: String,
    pub token_file: PathBuf,
    pub thread: String,
    pub socket: Option<PathBuf>,
}

#[derive(Serialize, Deserialize)]
pub struct Gate {
    pub lease: String,
    pub expires: u64,
}

pub fn default_dir() -> Result<PathBuf> {
    Ok(crate::journal::default_path()?.with_file_name("codex-inboxes"))
}

pub fn barrier(dir: &Path) -> Result<File> {
    let path = dir.join("registry.lock");
    state::parent(&path)?;
    let lock = state::open_private(&path, true)?;
    lock.lock()?;
    Ok(lock)
}

pub struct Registration {
    path: PathBuf,
    _alive: File,
}
impl Registration {
    pub fn new(dir: &Path, record: &Record, queue: &Queue) -> Result<Self> {
        let _barrier = barrier(dir)?;
        let gate_path = dir.join("hold.json");
        let mut active_gate = false;
        if gate_path.exists() {
            let gate: Gate = state::read_json(&gate_path)?;
            let now = chrono::Utc::now().timestamp().max(0) as u64;
            if gate.expires > now {
                queue.hold(&gate.lease, gate.expires)?;
                active_gate = true;
            }
        }
        // An inbox can die while held and return after the manager released its
        // live siblings. Clear only the known maintenance lease in that case.
        let maintenance = dir.join("maintenance-lease.json");
        if !active_gate && maintenance.exists() {
            let lease: String = state::read_json(&maintenance)?;
            queue.release(&lease)?;
        }
        let path = dir.join(format!("{}.json", record.instance));
        let alive = state::open_private(&path.with_extension("alive"), true)?;
        alive.try_lock()?;
        state::write_json(&path, record)?;
        Ok(Self {
            path,
            _alive: alive,
        })
    }
}
impl Drop for Registration {
    fn drop(&mut self) {
        let _barrier = self.path.parent().and_then(|dir| barrier(dir).ok());
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_file(self.path.with_extension("alive"));
    }
}

/// Caller owns the startup barrier while inspecting records.
pub fn discover(dir: &Path) -> Result<Vec<Record>> {
    let mut records = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        let Some(name) = path.file_stem().and_then(|name| name.to_str()) else {
            continue;
        };
        if path.extension().is_none_or(|ext| ext != "json")
            || name.len() != 36
            || !name.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
        {
            continue;
        }
        let alive = state::open_private(&path.with_extension("alive"), true)?;
        if alive.try_lock().is_ok() {
            std::fs::remove_file(&path)?;
            std::fs::remove_file(path.with_extension("alive"))?;
            continue;
        }
        let record: Record = state::read_json(&path)?;
        if record.instance != name {
            return Err("inbox registry identity mismatch".into());
        }
        let address: std::net::SocketAddr = record.address.parse()?;
        if !address.ip().is_loopback() {
            return Err("maintenance inbox must be loopback".into());
        }
        records.push(record);
    }
    records.sort_by(|a, b| a.instance.cmp(&b.instance));
    Ok(records)
}
