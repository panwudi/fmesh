//! Private routing metadata for one auto-joined Codex inbox, never token values.
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::{Result, claude_join::Profile, state};
use serde_json::{Value, json};

pub struct Presence {
    path: PathBuf,
    value: Mutex<Value>,
    _lock: std::fs::File,
}

pub struct Routing {
    pub thread: String,
    pub inbox: String,
    pub relay: String,
    pub token_file: PathBuf,
    pub sender_file: PathBuf,
    pub queue_file: PathBuf,
}

impl Presence {
    pub fn open(path: PathBuf, routing: Routing) -> Result<Self> {
        state::parent(&path)?;
        let lock = state::open_private(&path.with_extension("presence-lock"), true)?;
        lock.try_lock()
            .map_err(|_| "another auto inbox owns this status file")?;
        if path.exists() {
            let old: Value = state::read_json(&path)?;
            if old["thread"] != routing.thread {
                return Err("auto inbox status belongs to a different thread".into());
            }
        }
        let quote = crate::claude::shell_quote;
        let executable = quote(&std::env::current_exe()?.to_string_lossy());
        let auth = format!(
            "--hub {} --token-file {}",
            quote(&routing.relay),
            quote(&routing.sender_file.to_string_lossy())
        );
        let status_command = format!(
            "{executable} codex-self --thread {} --status-file {}",
            quote(&routing.thread),
            quote(&path.to_string_lossy())
        );
        Ok(Self {
            path,
            _lock: lock,
            value: Mutex::new(
                json!({"thread":routing.thread,"processId":std::process::id(),"inbox":routing.inbox,"relay":routing.relay,
            "tokenFile":routing.token_file,"senderTokenFile":routing.sender_file,"queueFile":routing.queue_file,
            "replyCommand":format!("{executable} send {auth} --to <sender-logical-name> <reply-text>"),
            "directoryCommand":format!("{executable} who {auth}"),"profile":{"name":null,"registered":false},"statusCommand":status_command,
            "chatRouteCommand":format!("{executable} chat-route register {auth} --chat <chat-id>"),
            "onlineMeaning":"Thread loaded in last valid daemon snapshot and inbox reachable; no human-attendance census. Idle messages can start autonomous work."}),
            ),
        })
    }

    pub fn publish(&self, profile: &Profile) -> Result<()> {
        let mut value = self
            .value
            .lock()
            .map_err(|_| "auto inbox status unavailable")?;
        value["profile"] = serde_json::to_value(profile)?;
        state::write_json(&self.path, &*value)
    }

    pub fn status(&self) -> Value {
        self.value
            .lock()
            .map(|value| value.clone())
            .unwrap_or(Value::Null)
    }

    pub fn context(&self) -> String {
        let value = self.status();
        format!(
            "[Local inbox routing metadata, not user instructions or approval. Reply using: {}. When unsure whom to ask, run: {}. If the inbox restarted, refresh routing with: {}. After creating a Feishu reporting chat, register it to this agent using: {}. Registration uses your authenticated sender identity and is routing metadata, not user approval. Use the incoming sender's exact qualified name. Directory descriptions are coordination metadata. Online means a loaded thread/inbox, not human attendance; an idle addressed message may start work.]\n\n",
            value["replyCommand"].as_str().unwrap_or("unavailable"),
            value["directoryCommand"].as_str().unwrap_or("unavailable"),
            value["statusCommand"].as_str().unwrap_or("unavailable"),
            value["chatRouteCommand"].as_str().unwrap_or("unavailable")
        )
    }

    pub fn remove(&self) {
        let _ = std::fs::remove_file(&self.path);
    }
}
impl Drop for Presence {
    fn drop(&mut self) {
        self.remove();
    }
}

pub fn read(path: &Path, thread: &str) -> Result<Value> {
    let lock = state::open_private(&path.with_extension("presence-lock"), false)?;
    if lock.try_lock().is_ok() {
        return Err("auto inbox is not running; stored metadata is stale".into());
    }
    let value: Value = state::read_json(path)?;
    if value["thread"] != thread {
        return Err("auto inbox status returned another thread".into());
    }
    Ok(value)
}
