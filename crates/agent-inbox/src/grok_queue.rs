//! Durable per-session Grok FIFO. Uncertain deliveries require explicit resolution.

use std::collections::VecDeque;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{Result, grok_acp, journal, state};

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
enum Phase {
    Queued,
    Attempting,
    Unconfirmed,
}

#[derive(Clone, Serialize, Deserialize)]
struct Pending {
    id: String,
    sender: String,
    text: String,
    phase: Phase,
    reason: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Snapshot {
    version: u32,
    session: String,
    pending: VecDeque<Pending>,
    receipts: VecDeque<Value>,
}

pub struct Queue {
    path: PathBuf,
    data: Mutex<Snapshot>,
    changed: Condvar,
    _lock: File,
}

impl Queue {
    pub fn open(path: PathBuf, session: &str) -> Result<Arc<Self>> {
        state::parent(&path)?;
        let lock = state::open_private(&path.with_extension("queue-lock"), true)?;
        lock.try_lock()
            .map_err(|_| "another Grok inbox owns this queue")?;
        let mut data = if path.exists() {
            state::read_json::<Snapshot>(&path)?
        } else {
            Snapshot {
                version: 1,
                session: session.to_owned(),
                pending: VecDeque::new(),
                receipts: VecDeque::new(),
            }
        };
        if data.version != 1 || data.session != session || data.pending.len() > 256 {
            return Err("Grok queue version, session or size mismatch".into());
        }
        let mut seen = std::collections::HashSet::new();
        for item in &mut data.pending {
            if !state::is_id(&item.id) || !seen.insert(item.id.clone()) {
                return Err("Grok queue has invalid or duplicate message IDs".into());
            }
            crate::validate_message(session, &item.sender, &item.text)?;
            if item.phase == Phase::Attempting {
                item.phase = Phase::Unconfirmed;
                item.reason = Some("inbox restarted during Grok delivery".into());
            }
        }
        state::write_json(&path, &data)?;
        Ok(Arc::new(Self {
            path,
            data: Mutex::new(data),
            changed: Condvar::new(),
            _lock: lock,
        }))
    }

    fn edit(&self, change: impl FnOnce(&mut Snapshot) -> Result<()>) -> Result<()> {
        let mut data = self.data.lock().map_err(|_| "Grok queue lock failed")?;
        let mut next = data.clone();
        change(&mut next)?;
        state::write_json(&self.path, &next)?;
        *data = next;
        self.changed.notify_all();
        Ok(())
    }

    pub fn enqueue(&self, sender: &str, text: &str) -> Result<String> {
        let id = state::id()?;
        self.edit(|data| {
            if data.pending.len() >= 256 {
                return Err("Grok queue full; message remains in journal".into());
            }
            data.pending.push_back(Pending {
                id: id.clone(),
                sender: sender.to_owned(),
                text: text.to_owned(),
                phase: Phase::Queued,
                reason: None,
            });
            Ok(())
        })?;
        Ok(id)
    }

    pub fn status(&self) -> Result<Value> {
        let data = self.data.lock().map_err(|_| "Grok queue lock failed")?;
        Ok(
            json!({"session":data.session,"pending":data.pending.iter().map(|item| json!({"id":item.id,"phase":item.phase,"reason":item.reason})).collect::<Vec<_>>(),"receipts":data.receipts}),
        )
    }

    pub fn resolve(&self, id: &str, resend: bool) -> Result<()> {
        self.edit(|data| {
            let head = data.pending.front().ok_or("Grok queue is empty")?;
            if head.id != id || head.phase != Phase::Unconfirmed {
                return Err("only the uncertain Grok queue head can be resolved".into());
            }
            if resend {
                let head = data.pending.front_mut().unwrap();
                head.phase = Phase::Queued;
                head.reason = None;
            } else {
                data.pending.pop_front();
            }
            Ok(())
        })
    }

    fn head(&self) -> Option<Pending> {
        self.data.lock().ok()?.pending.front().cloned()
    }

    fn begin(&self, id: &str) -> Result<()> {
        self.edit(|data| {
            let head = data
                .pending
                .front_mut()
                .filter(|head| head.id == id && head.phase == Phase::Queued)
                .ok_or("Grok queue head changed")?;
            head.phase = Phase::Attempting;
            Ok(())
        })
    }

    fn delivered(&self, id: &str) -> Result<()> {
        self.edit(|data| {
            if data
                .pending
                .front()
                .is_none_or(|head| head.id != id || head.phase != Phase::Attempting)
            {
                return Err("Grok queue head changed before receipt".into());
            }
            data.pending.pop_front();
            data.receipts.push_back(json!({"id":id,"state":"accepted"}));
            while data.receipts.len() > 100 {
                data.receipts.pop_front();
            }
            Ok(())
        })
    }

    fn requeue_missing_session(&self, id: &str) -> Result<()> {
        self.edit(|data| {
            let head = data
                .pending
                .front_mut()
                .filter(|head| head.id == id && head.phase == Phase::Attempting)
                .ok_or("Grok queue head changed before safe retry")?;
            head.phase = Phase::Queued;
            head.reason = Some("Grok session is no longer resident; waiting for resume".into());
            Ok(())
        })
    }

    fn uncertain(&self, id: &str, log: &Path) -> Result<()> {
        self.edit(|data| {
            let head = data.pending.front_mut().filter(|head| head.id == id && head.phase == Phase::Attempting).ok_or("Grok queue head changed before uncertainty")?;
            head.phase = Phase::Unconfirmed;
            head.reason = Some("Grok interjection acknowledgement unavailable; inspect the session before resend or drop".into());
            Ok(())
        })?;
        eprintln!(
            "agent-inbox: Grok message {id} unconfirmed; FIFO blocked until explicit resend or drop"
        );
        journal::append(
            log,
            "agent-inbox",
            id,
            "Grok delivery unconfirmed. FIFO is blocked until explicit resend or drop.",
        )
        .map(|_| ())
    }

    fn wait(&self, duration: Duration) {
        if let Ok(data) = self.data.lock() {
            let _ = self.changed.wait_timeout(data, duration);
        }
    }

    pub fn start(
        self: &Arc<Self>,
        connection: Arc<grok_acp::Shared>,
        session: String,
        routing: String,
        log: PathBuf,
        running: Arc<AtomicBool>,
    ) -> std::thread::JoinHandle<()> {
        let queue = Arc::clone(self);
        std::thread::spawn(move || {
            let mut retry = 1;
            while running.load(Ordering::Acquire) {
                let Some(head) = queue.head() else {
                    queue.wait(Duration::from_secs(2));
                    retry = 1;
                    continue;
                };
                if head.phase != Phase::Queued {
                    queue.wait(Duration::from_secs(2));
                    continue;
                }
                if connection.roster().ok().is_none_or(|sessions| {
                    !sessions.iter().any(|candidate| candidate.id == session)
                }) {
                    queue.wait(Duration::from_secs(retry));
                    retry = (retry * 2).min(30);
                    continue;
                }
                if queue.begin(&head.id).is_err() {
                    continue;
                }
                let text = format!("{routing}\n\n{}", head.text);
                match connection.interject(&session, &head.sender, &text, &head.id) {
                    Ok(()) => {
                        if queue.delivered(&head.id).is_err() {
                            eprintln!(
                                "agent-inbox: Grok receipt could not be saved; queue needs review"
                            );
                        }
                        retry = 1;
                    }
                    Err(error)
                        if error
                            .downcast_ref::<grok_acp::RpcError>()
                            .is_some_and(grok_acp::RpcError::is_missing_session) =>
                    {
                        if queue.requeue_missing_session(&head.id).is_err() {
                            eprintln!(
                                "agent-inbox: Grok missing-session retry could not be saved; queue needs review"
                            );
                        }
                        queue.wait(Duration::from_secs(retry));
                        retry = (retry * 2).min(30);
                    }
                    Err(_) => {
                        if queue.uncertain(&head.id, &log).is_err() {
                            eprintln!(
                                "agent-inbox: Grok uncertainty could not be saved; queue needs review"
                            );
                        }
                    }
                }
            }
        })
    }
}
