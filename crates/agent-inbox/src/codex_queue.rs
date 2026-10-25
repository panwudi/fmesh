use std::collections::VecDeque;
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    Result,
    codex::{Client, RpcError},
    journal, proxy, state,
};

#[derive(Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum Phase {
    Queued,
    Attempting,
    Unconfirmed,
    Blocked,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Pending {
    pub id: String,
    pub sender: String,
    pub text: String,
    pub phase: Phase,
    pub reason: Option<String>,
    pub notified: bool,
}

#[derive(Clone, Serialize, Deserialize)]
struct Snapshot {
    version: u32,
    thread: String,
    pending: VecDeque<Pending>,
    receipts: VecDeque<Value>,
    #[serde(default)]
    holds: std::collections::BTreeMap<String, u64>,
}

pub struct Queue {
    path: PathBuf,
    data: Mutex<Snapshot>,
    changed: Condvar,
    _lock: File,
    notifying: AtomicBool,
}

#[derive(Debug)]
struct Held;
impl std::fmt::Display for Held {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("delivery held for maintenance")
    }
}
impl std::error::Error for Held {}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn held(data: &Snapshot) -> bool {
    data.holds.values().any(|expires| *expires > now())
}

#[derive(Clone)]
pub struct Backend {
    pub codex: PathBuf,
    pub socket: Option<PathBuf>,
    pub timeout: Duration,
    pub thread: String,
    pub log: PathBuf,
    pub address: String,
    pub token_file: PathBuf,
    pub notify: Option<PathBuf>,
    pub notify_args: Vec<String>,
    pub routing_context: Option<String>,
    pub push_blocked_notice: bool,
    pub running: Arc<AtomicBool>,
}

impl Queue {
    pub fn open(path: PathBuf, thread: &str) -> Result<Arc<Self>> {
        state::parent(&path)?;
        let lock = state::open_private(&path.with_extension("queue-lock"), true)?;
        lock.try_lock()
            .map_err(|_| "another inbox owns this queue")?;
        let mut data = if path.exists() {
            state::read_json::<Snapshot>(&path)?
        } else {
            Snapshot {
                version: 1,
                thread: thread.to_owned(),
                pending: VecDeque::new(),
                receipts: VecDeque::new(),
                holds: Default::default(),
            }
        };
        if data.version != 1 || data.thread != thread || data.pending.len() > 256 {
            return Err("queue version, thread or size mismatch".into());
        }
        let mut ids = std::collections::HashSet::new();
        for pending in &mut data.pending {
            if pending.id.len() != 36
                || !pending.id.bytes().enumerate().all(|(index, byte)| {
                    if [8, 13, 18, 23].contains(&index) {
                        byte == b'-'
                    } else {
                        byte.is_ascii_hexdigit()
                    }
                })
                || !ids.insert(pending.id.clone())
            {
                return Err("invalid or duplicate queued client message id".into());
            }
            crate::validate_message(thread, &pending.sender, &pending.text)?;
            if pending.phase == Phase::Attempting {
                pending.phase = Phase::Unconfirmed;
                pending.reason = Some("inbox restarted during delivery".into());
                pending.notified = false;
            }
            if matches!(pending.phase, Phase::Unconfirmed | Phase::Blocked) {
                pending.notified = false;
            }
        }
        state::write_json(&path, &data)?;
        Ok(Arc::new(Self {
            path,
            data: Mutex::new(data),
            changed: Condvar::new(),
            _lock: lock,
            notifying: AtomicBool::new(false),
        }))
    }

    fn edit(&self, change: impl FnOnce(&mut Snapshot) -> Result<()>) -> Result<()> {
        let mut data = self.data.lock().map_err(|_| "queue lock failed")?;
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
                return Err("Codex queue full; message remains in the journal".into());
            }
            data.pending.push_back(Pending {
                id: id.clone(),
                sender: sender.to_owned(),
                text: text.to_owned(),
                phase: Phase::Queued,
                reason: None,
                notified: false,
            });
            Ok(())
        })?;
        Ok(id)
    }

    pub fn status(&self) -> Result<Value> {
        let data = self.data.lock().map_err(|_| "queue lock failed")?;
        Ok(
            json!({"thread":data.thread, "holdRequested":held(&data), "held":held(&data) && !data.pending.iter().any(|item| item.phase == Phase::Attempting), "holds":data.holds, "pending":data.pending.iter().map(|item| json!({"id":item.id,"phase":item.phase,"reason":item.reason})).collect::<Vec<_>>(), "receipts":data.receipts}),
        )
    }

    pub fn hold(&self, lease: &str, expires: u64) -> Result<()> {
        if lease.len() != 36
            || !lease.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
            || expires <= now()
            || expires > now() + 86400
        {
            return Err("invalid hold lease or expiry (maximum 24 hours)".into());
        }
        self.edit(|data| {
            data.holds.retain(|_, expiry| *expiry > now());
            if data.holds.len() >= 16 && !data.holds.contains_key(lease) {
                return Err("too many hold leases".into());
            }
            data.holds.insert(lease.into(), expires);
            Ok(())
        })
    }

    pub fn release(&self, lease: &str) -> Result<()> {
        self.edit(|data| {
            data.holds.remove(lease);
            Ok(())
        })
    }

    fn is_held(&self) -> bool {
        self.data.lock().map(|data| held(&data)).unwrap_or(true)
    }

    fn begin(&self, id: &str) -> Result<()> {
        self.edit(|data| {
            if held(data) {
                return Err(Held.into());
            }
            let head = data
                .pending
                .front_mut()
                .filter(|item| item.id == id)
                .ok_or("queue head changed")?;
            head.phase = Phase::Attempting;
            head.reason = None;
            head.notified = false;
            Ok(())
        })
    }

    pub fn resolve(&self, id: &str, resend: bool) -> Result<()> {
        self.edit(|data| {
            let position = data
                .pending
                .iter()
                .position(|item| item.id == id)
                .ok_or("unknown queued message id")?;
            let item = &mut data.pending[position];
            if !matches!(item.phase, Phase::Unconfirmed | Phase::Blocked) {
                return Err("only blocked or unconfirmed messages can be resolved".into());
            }
            if resend {
                item.phase = Phase::Queued;
                item.reason = None;
                item.notified = false;
            } else {
                data.pending.remove(position);
                data.receipts.push_back(json!({"id":id,"status":"dropped"}));
                while data.receipts.len() > 128 {
                    data.receipts.pop_front();
                }
            }
            Ok(())
        })
    }

    fn head(&self) -> Option<Pending> {
        self.data.lock().ok()?.pending.front().cloned()
    }

    fn phase(&self, id: &str, phase: Phase, reason: Option<&str>) -> Result<()> {
        self.edit(|data| {
            let item = data
                .pending
                .front_mut()
                .filter(|item| item.id == id)
                .ok_or("queue head changed")?;
            if item.phase != phase || item.reason.as_deref() != reason {
                item.notified = false;
            }
            item.phase = phase;
            item.reason = reason.map(str::to_owned);
            Ok(())
        })
    }

    fn delivered(&self, id: &str, turn: &str) -> Result<()> {
        self.edit(|data| {
            if data.pending.front().is_some_and(|item| item.id == id) {
                data.pending.pop_front();
                data.receipts
                    .push_back(json!({"id":id,"status":"delivered","turnId":turn}));
                while data.receipts.len() > 128 {
                    data.receipts.pop_front();
                }
            }
            Ok(())
        })
    }

    fn wait(&self, id: Option<&str>, delay: Duration) {
        let deadline = Instant::now() + delay;
        let Ok(mut data) = self.data.lock() else {
            return;
        };
        let phase = data.pending.front().map(|item| item.phase.clone());
        let holds = data.holds.clone();
        while Instant::now() < deadline
            && data.pending.front().map(|item| item.id.as_str()) == id
            && data.pending.front().map(|item| item.phase.clone()) == phase
            && data.holds == holds
        {
            let Ok((next, _)) = self
                .changed
                .wait_timeout(data, deadline.saturating_duration_since(Instant::now()))
            else {
                return;
            };
            data = next;
        }
    }

    pub fn start(self: &Arc<Self>, backend: Backend) {
        let queue = Arc::clone(self);
        std::thread::spawn(move || {
            let mut delay = 1;
            loop {
                if !backend.running.load(Ordering::Acquire) {
                    break;
                }
                if queue.is_held() {
                    let id = queue.head().map(|item| item.id);
                    queue.wait(id.as_deref(), Duration::from_millis(500));
                    delay = 1;
                    continue;
                }
                let Some(head) = queue.head() else {
                    queue.wait(None, Duration::from_secs(30));
                    delay = 1;
                    continue;
                };
                let result = queue.attempt(&backend, &head);
                if result.is_ok() && queue.head().is_none_or(|item| item.id != head.id) {
                    delay = 1;
                    continue;
                }
                if let Some(item) = queue
                    .head()
                    .filter(|item| matches!(item.phase, Phase::Blocked | Phase::Unconfirmed))
                    && queue.notice(&backend, &item).is_err()
                {
                    eprintln!(
                        "agent-inbox: could not persist/notify blocked delivery; FIFO remains blocked"
                    );
                }
                eprintln!(
                    "agent-inbox: Codex message {} pending; retry/check in {delay}s; FIFO retained",
                    head.id
                );
                queue.wait(Some(&head.id), Duration::from_secs(delay));
                delay = (delay * 2).min(30);
            }
        });
    }

    fn attempt(&self, backend: &Backend, item: &Pending) -> Result<()> {
        if self.is_held() || !backend.running.load(Ordering::Acquire) {
            return Err(Held.into());
        }
        if item.phase == Phase::Blocked {
            return Err("delivery requires explicit resolution".into());
        }
        let socket = match proxy::checked_socket(&backend.codex, backend.socket.as_deref()) {
            Ok(socket) => socket,
            Err(_) => return Err("daemon unavailable before delivery".into()),
        };
        let mut client = match Client::connect(&backend.codex, &socket, backend.timeout) {
            Ok(client) => client,
            Err(error) => {
                if item.phase == Phase::Queued
                    && (error.to_string().contains("protocol mismatch")
                        || error
                            .downcast_ref::<RpcError>()
                            .is_some_and(|error| !error.is_draining()))
                {
                    let reason = error.downcast_ref::<RpcError>().map_or_else(
                        || "app-server initialization protocol mismatch".to_owned(),
                        |rpc| {
                            format!(
                                "app-server initialization rejected {} (code {})",
                                rpc.method(),
                                rpc.code()
                            )
                        },
                    );
                    self.phase(&item.id, Phase::Blocked, Some(&reason))?;
                }
                return Err(error);
            }
        };
        if matches!(item.phase, Phase::Unconfirmed | Phase::Attempting) {
            match client.find_client_message(&backend.thread, &item.id) {
                Ok(Some(turn)) => return self.delivered(&item.id, &turn),
                Ok(None) => self.phase(&item.id, Phase::Queued, None)?,
                Err(_) => {
                    self.phase(
                        &item.id,
                        Phase::Unconfirmed,
                        Some("history unavailable or incomplete; later messages are blocked"),
                    )?;
                    return Err("history reconciliation incomplete".into());
                }
            }
        }
        // Local routing hints do not enlarge the durable message or its 64 KiB limit.
        let text = backend
            .routing_context
            .as_ref()
            .map(|context| format!("{context}{}", item.text));
        let result = client.deliver_tracked(
            &backend.thread,
            &item.sender,
            text.as_deref().unwrap_or(&item.text),
            Some(&item.id),
            || {
                if !backend.running.load(Ordering::Acquire) {
                    return Err(Held.into());
                }
                self.begin(&item.id)
            },
        );
        match result {
            Ok(delivery) => self.delivered(&item.id, &delivery.turn),
            Err(error) => {
                if error.downcast_ref::<Held>().is_some() {
                    self.phase(&item.id, Phase::Queued, None)?;
                    return Err(error);
                }
                if error
                    .downcast_ref::<RpcError>()
                    .is_some_and(RpcError::is_draining)
                {
                    self.phase(
                        &item.id,
                        Phase::Queued,
                        Some("server is draining; retrying without losing the message"),
                    )?;
                } else if error.downcast_ref::<RpcError>().is_some()
                    || self.head().is_some_and(|item| item.phase == Phase::Queued)
                        && error.downcast_ref::<tungstenite::Error>().is_none()
                        && error.downcast_ref::<std::io::Error>().is_none()
                {
                    let reason = error.downcast_ref::<RpcError>().map_or_else(
                        || "app-server protocol mismatch; explicit resolution required".to_owned(),
                        |rpc| {
                            format!(
                                "app-server rejected {} (code {}); explicit resolution required",
                                rpc.method(),
                                rpc.code()
                            )
                        },
                    );
                    self.phase(&item.id, Phase::Blocked, Some(&reason))?;
                } else if self
                    .head()
                    .is_some_and(|item| item.phase == Phase::Attempting)
                {
                    self.phase(
                        &item.id,
                        Phase::Unconfirmed,
                        Some("delivery acknowledgement lost; reconciling history before replay"),
                    )?;
                }
                Err(error)
            }
        }
    }

    fn notice(&self, backend: &Backend, item: &Pending) -> Result<()> {
        if item.notified {
            return Ok(());
        }
        let base = format!(
            "agent-inbox codex-queue '{}' --token-file '{}'",
            backend.address.replace('\'', "'\\''"),
            backend
                .token_file
                .display()
                .to_string()
                .replace('\'', "'\\''")
        );
        let phase = serde_json::to_value(&item.phase)?;
        let text = format!(
            "Message {} is {}. {}. FIFO blocks later messages to this thread. Resolve explicitly with: {base} --resend {} or {base} --drop {}",
            item.id,
            phase.as_str().unwrap_or("unknown"),
            item.reason.as_deref().unwrap_or("delivery requires review"),
            item.id,
            item.id
        );
        journal::append(&backend.log, "agent-inbox", &backend.thread, &text)?;
        let push_result = if backend.push_blocked_notice {
            (|| -> Result<()> {
                let socket = proxy::checked_socket(&backend.codex, backend.socket.as_deref())?;
                let mut client = Client::connect(&backend.codex, &socket, backend.timeout)?;
                client.deliver(&backend.thread, "agent-inbox", &text)?;
                Ok(())
            })()
        } else {
            Ok(())
        };
        let report = json!({"event":"codexDeliveryBlocked","thread":backend.thread,"messageId":item.id,"reason":item.reason,"fifoBlocked":true,"resend":format!("{base} --resend {}",item.id),"drop":format!("{base} --drop {}",item.id)});
        self.notifying.store(true, Ordering::Release);
        struct Active<'a>(&'a AtomicBool);
        impl Drop for Active<'_> {
            fn drop(&mut self) {
                self.0.store(false, Ordering::Release);
            }
        }
        let _active = Active(&self.notifying);
        let hook_result = notify_running(
            backend.notify.as_deref(),
            &backend.notify_args,
            &report,
            Some(&backend.running),
        );
        if backend.push_blocked_notice {
            push_result.map_err(|_| "blocked delivery alert did not reach Codex; retrying")?;
            if hook_result.is_err() {
                eprintln!(
                    "agent-inbox: blocked delivery alert reached Codex but notify hook failed"
                );
            }
        } else {
            hook_result?;
        }
        self.edit(|data| {
            if let Some(head) = data.pending.front_mut().filter(|head| head.id == item.id) {
                head.notified = true;
            }
            Ok(())
        })
    }
    pub fn finish_notifier(&self) {
        self.changed.notify_all();
        let deadline = Instant::now() + Duration::from_millis(250);
        while self.notifying.load(Ordering::Acquire) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

struct NotifyChild(std::process::Child);
fn stop_notify_group(id: u32) {
    #[cfg(unix)]
    unsafe {
        libc::kill(-(id as libc::pid_t), libc::SIGKILL);
    }
}
impl Drop for NotifyChild {
    fn drop(&mut self) {
        stop_notify_group(self.0.id());
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

pub fn notify(command: Option<&std::path::Path>, args: &[String], report: &Value) -> Result<()> {
    notify_running(command, args, report, None)
}

fn notify_running(
    command: Option<&std::path::Path>,
    args: &[String],
    report: &Value,
    running: Option<&AtomicBool>,
) -> Result<()> {
    if running.is_some_and(|r| !r.load(Ordering::Acquire)) {
        return Err("inbox stopped before notification".into());
    }
    let Some(command) = command else {
        eprintln!("agent-inbox notice: {report}");
        return Ok(());
    };
    let mut command = Command::new(command);
    command
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let child = command.spawn()?;
    let mut owner = NotifyChild(child);
    let child = &mut owner.0;
    let mut stdin = child.stdin.take().ok_or("notify stdin unavailable")?;
    let payload = format!("{report}\n");
    let (sent, result) = std::sync::mpsc::channel();
    let writer = std::thread::spawn(move || {
        let _ = sent.send(stdin.write_all(payload.as_bytes()));
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if running.is_some_and(|r| !r.load(Ordering::Acquire)) {
            return Err("inbox stopped during notification".into());
        }
        if let Some(status) = child.try_wait()? {
            stop_notify_group(child.id());
            writer.join().map_err(|_| "notify writer failed")?;
            result.recv().map_err(|_| "notify pipe closed")??;
            return if status.success() {
                Ok(())
            } else {
                Err("notify hook failed".into())
            };
        }
        if Instant::now() >= deadline {
            stop_notify_group(child.id());
            let _ = child.kill();
            let _ = child.wait();
            let _ = writer.join();
            return Err("notify hook timed out".into());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}
