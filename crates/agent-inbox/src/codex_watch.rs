//! One per-user watcher. Identity comes from daemon thread.id, never MCP guesses.
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::{Result, codex::Client, command, state};
use serde::Deserialize;
use serde_json::{Value, json};

static STOP: AtomicBool = AtomicBool::new(false);
extern "C" fn stop(_: libc::c_int) {
    STOP.store(true, Ordering::Release);
}

pub fn default_root() -> Result<PathBuf> {
    Ok(crate::journal::default_path()?.with_file_name("codex-watch"))
}
pub fn key(thread: &str) -> Result<String> {
    if thread.is_empty() || thread.len() > 96 || !thread.bytes().all(|b| b.is_ascii_graphic()) {
        return Err("invalid or oversized daemon thread ID".into());
    }
    Ok(thread.bytes().map(|b| format!("{b:02x}")).collect())
}
pub fn name_from_cwd(cwd: &str) -> String {
    crate::claude_join::typed_name(Path::new(cwd), "codex")
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    relay: String,
    state_dir: Option<PathBuf>,
    registry_dir: Option<PathBuf>,
    control_socket: Option<PathBuf>,
    codex_bin: Option<PathBuf>,
    socket: Option<PathBuf>,
    poll_ms: Option<u64>,
    rpc_timeout_secs: Option<u64>,
    snapshot_timeout_secs: Option<u64>,
    delivery_timeout_secs: Option<u64>,
    max_threads: Option<usize>,
    notify_command: Option<PathBuf>,
    #[serde(default)]
    notify_args: Vec<String>,
}

struct Watcher {
    relay: String,
    root: PathBuf,
    registry: PathBuf,
    control: Option<PathBuf>,
    codex: PathBuf,
    socket: Option<PathBuf>,
    poll: Duration,
    rpc: Duration,
    snapshot_timeout: Duration,
    delivery_timeout: u64,
    max_threads: usize,
    notify: Option<PathBuf>,
    notify_args: Vec<String>,
    warned_server: AtomicBool,
}
#[derive(Clone, PartialEq)]
struct Spec {
    thread: String,
    cwd: String,
    socket: PathBuf,
}

impl Watcher {
    fn load(path: &Path) -> Result<Self> {
        let path = std::fs::canonicalize(path)?;
        let config: Config = toml::from_str(&std::fs::read_to_string(&path)?)
            .map_err(|_| "invalid Codex watcher TOML; check fields and syntax")?;
        let absolute = |p: PathBuf| -> Result<PathBuf> {
            Ok(std::path::absolute(crate::hub::relative_to(&path, p))?)
        };
        let root = config
            .state_dir
            .map(absolute)
            .unwrap_or_else(default_root)?;
        let registry = config
            .registry_dir
            .map(absolute)
            .unwrap_or_else(crate::codex_registry::default_dir)?;
        let control = config.control_socket.map(absolute).transpose()?;
        let socket = config.socket.map(absolute).transpose()?;
        let executable = |p: PathBuf| -> Result<PathBuf> {
            if p.components().count() > 1 {
                absolute(p)
            } else {
                Ok(p)
            }
        };
        let codex = config
            .codex_bin
            .map(executable)
            .transpose()?
            .unwrap_or_else(|| "codex".into());
        let notify = config.notify_command.map(executable).transpose()?;
        let poll = config.poll_ms.unwrap_or(2000);
        let rpc = config.rpc_timeout_secs.unwrap_or(3);
        let snapshot_timeout = config.snapshot_timeout_secs.unwrap_or(30);
        let delivery_timeout = config.delivery_timeout_secs.unwrap_or(10);
        let max_threads = config.max_threads.unwrap_or(256);
        if !(100..=60000).contains(&poll)
            || !(1..=60).contains(&rpc)
            || !(1..=300).contains(&snapshot_timeout)
            || !(1..=60).contains(&delivery_timeout)
            || !(1..=256).contains(&max_threads)
        {
            return Err("Codex watcher limits out of range".into());
        }
        Ok(Self {
            relay: crate::hub::loopback_endpoint(&config.relay)?,
            root,
            registry,
            control,
            codex,
            socket,
            poll: Duration::from_millis(poll),
            rpc: Duration::from_secs(rpc),
            snapshot_timeout: Duration::from_secs(snapshot_timeout),
            delivery_timeout,
            max_threads,
            notify,
            notify_args: config.notify_args,
            warned_server: AtomicBool::new(false),
        })
    }

    fn socket(&self) -> Result<PathBuf> {
        if let Some(path) = &self.socket {
            return Ok(path.clone());
        }
        let bytes = command::output(
            Command::new(&self.codex).args(["app-server", "daemon", "version"]),
            self.rpc,
        )?;
        let value: Value =
            serde_json::from_slice(&bytes).map_err(|_| "invalid daemon version response")?;
        if value["status"] != "running" {
            return Err("Codex daemon unavailable".into());
        }
        if value["appServerVersion"]
            .as_str()
            .is_some_and(|v| v != crate::proxy::CODEX_VERSION)
            && !self.warned_server.swap(true, Ordering::Relaxed)
        {
            eprintln!(
                "agent-inbox: warning: daemon differs from tested Codex {}; validating protocol responses",
                crate::proxy::CODEX_VERSION
            );
        }
        let path = value["socketPath"]
            .as_str()
            .ok_or("daemon socket path missing")?;
        if !Path::new(path).is_absolute() {
            return Err("daemon socket must be absolute".into());
        }
        Ok(path.into())
    }

    fn snapshot(&self) -> Result<BTreeMap<String, Spec>> {
        let deadline = Instant::now() + self.snapshot_timeout;
        let socket = self.socket()?;
        let mut client = Client::connect(&self.codex, &socket, self.rpc)?;
        let mut seen = HashSet::new();
        let mut cursors = HashSet::new();
        let mut cursor = None;
        let mut result = BTreeMap::new();
        for _ in 0..1024 {
            if STOP.load(Ordering::Acquire) || Instant::now() >= deadline {
                return Err("watcher snapshot interrupted or timed out".into());
            }
            let page =
                client.request("thread/loaded/list", json!({"cursor":cursor,"limit":100}))?;
            let ids = page["data"]
                .as_array()
                .ok_or("invalid loaded thread page")?;
            for id in ids {
                if STOP.load(Ordering::Acquire) || Instant::now() >= deadline {
                    return Err("watcher snapshot interrupted or timed out".into());
                }
                let id = id.as_str().ok_or("invalid loaded thread ID")?;
                key(id)?;
                if !seen.insert(id.to_owned()) || seen.len() > 4096 {
                    return Err("repeated/oversized loaded thread list".into());
                }
                let thread =
                    client.request("thread/read", json!({"threadId":id,"includeTurns":false}))?;
                let thread = &thread["thread"];
                if thread["id"] != id {
                    return Err("thread/read identity mismatch".into());
                }
                match thread["status"]["type"].as_str() {
                    Some("idle" | "active" | "systemError") => {}
                    Some("notLoaded") => {
                        return Err(
                            "loaded thread changed during snapshot; retaining membership".into(),
                        );
                    }
                    _ => return Err("unknown thread status; retaining membership".into()),
                }
                let eligible = match &thread["source"] {
                    Value::String(source)
                        if ["cli", "vscode", "appServer"].contains(&source.as_str()) =>
                    {
                        true
                    }
                    Value::String(source) if ["exec", "unknown"].contains(&source.as_str()) => {
                        false
                    }
                    Value::Object(source)
                        if source.len() == 1
                            && (source.get("subAgent").is_some_and(|value| {
                                value.is_string()
                                    || value.as_object().is_some_and(|o| !o.is_empty())
                            }) || source.get("custom").is_some_and(Value::is_string)) =>
                    {
                        false
                    }
                    _ => return Err("unknown thread source; retaining membership".into()),
                };
                let cwd = thread["cwd"]
                    .as_str()
                    .filter(|cwd| Path::new(cwd).is_absolute() && !cwd.contains(['\r', '\n', '\0']))
                    .ok_or("invalid thread cwd")?;
                if eligible {
                    result.insert(
                        id.into(),
                        Spec {
                            thread: id.into(),
                            cwd: cwd.into(),
                            socket: socket.clone(),
                        },
                    );
                    if result.len() > self.max_threads {
                        return Err("watcher thread capacity exceeded; retaining membership".into());
                    }
                }
            }
            match page.get("nextCursor") {
                Some(Value::Null) | None => return Ok(result),
                Some(Value::String(next)) if !next.is_empty() && cursors.insert(next.clone()) => {
                    cursor = Some(next.clone())
                }
                _ => {
                    return Err(
                        "incomplete/repeated loaded thread cursor; retaining membership".into(),
                    );
                }
            }
        }
        Err("loaded thread pagination limit exceeded".into())
    }

    fn spawn(&self, spec: &Spec) -> Result<OwnedChild> {
        let directory = self.root.join("threads").join(key(&spec.thread)?);
        crate::local_control::private_dir(&directory)?;
        let mut command = Command::new(std::env::current_exe()?);
        command
            .args([
                "codex-worker",
                "--thread",
                &spec.thread,
                "--relay",
                &self.relay,
                "--name",
                &name_from_cwd(&spec.cwd),
            ])
            .arg("--description")
            .arg(format!(
                "Codex loaded thread {}; online does not imply human attendance",
                spec.thread
            ))
            .arg("--queue-file")
            .arg(directory.join("queue.json"))
            .arg("--log")
            .arg(directory.join("messages.md"))
            .arg("--status-file")
            .arg(directory.join("session.json"))
            .arg("--session-dir")
            .arg(self.root.join("credentials"))
            .arg("--registry-dir")
            .arg(&self.registry)
            .arg("--codex-bin")
            .arg(&self.codex)
            .arg("--sock")
            .arg(&spec.socket)
            .arg("--timeout-secs")
            .arg(self.delivery_timeout.to_string())
            .arg("--parent")
            .arg(std::process::id().to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());
        if let Some(control) = &self.control {
            command.arg("--control-socket").arg(control);
        }
        if let Some(notify) = &self.notify {
            command.arg("--notify-command").arg(notify);
        }
        for arg in &self.notify_args {
            command.arg("--notify-arg").arg(arg);
        }
        use std::os::unix::process::CommandExt;
        command.process_group(0);
        Ok(OwnedChild(Some(command.spawn()?)))
    }

    fn run(&self, parent: Option<u32>) -> Result<()> {
        crate::local_control::private_dir(&self.root)?;
        crate::local_control::private_dir(&self.registry)?;
        let lock = state::open_private(&self.registry.join("codex-watch.lock"), true)?;
        lock.try_lock()
            .map_err(|_| "a Codex watcher already owns this user/daemon registry")?;
        let version = command::output(Command::new(&self.codex).arg("--version"), self.rpc)?;
        if String::from_utf8_lossy(&version).trim()
            != format!("codex-cli {}", crate::proxy::CODEX_VERSION)
        {
            eprintln!(
                "agent-inbox: warning: CLI differs from tested Codex {}; validating protocol responses",
                crate::proxy::CODEX_VERSION
            );
        }
        STOP.store(false, Ordering::Release);
        for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
            if unsafe { libc::signal(signal, stop as *const () as libc::sighandler_t) }
                == libc::SIG_ERR
            {
                return Err("cannot install watcher shutdown handler".into());
            }
        }
        crate::daemon::watch_parent(parent, &STOP)?;
        eprintln!(
            "agent-inbox: Codex watcher ready; online means daemon-loaded/inbox reachable, not human attendance"
        );
        let mut slots = Slots(BTreeMap::new());
        let mut failed = false;
        while !STOP.load(Ordering::Acquire) {
            // Reap/restart crashed workers even if the daemon is temporarily unavailable.
            for (id, slot) in &mut slots.0 {
                if slot.child.as_mut().is_some_and(|child| child.exited()) {
                    slot.child.take();
                    slot.next = Instant::now() + Duration::from_secs(slot.backoff);
                    eprintln!(
                        "agent-inbox: Codex inbox {id} exited; retrying in {}s",
                        slot.backoff
                    );
                    slot.backoff = (slot.backoff * 2).min(30);
                }
            }
            match self.snapshot() {
                Ok(snapshot) => {
                    if failed {
                        eprintln!(
                            "agent-inbox: Codex watcher recovered; complete snapshot restored"
                        );
                        failed = false;
                    }
                    slots
                        .0
                        .retain(|id, slot| snapshot.get(id).is_some_and(|spec| spec == &slot.spec));
                    for (id, spec) in snapshot {
                        slots.0.entry(id).or_insert_with(|| Slot {
                            spec,
                            child: None,
                            next: Instant::now(),
                            backoff: 1,
                        });
                    }
                }
                Err(_) => {
                    if !failed {
                        eprintln!(
                            "agent-inbox: Codex snapshot unavailable/incomplete; retaining inboxes and queues"
                        );
                        failed = true;
                    }
                }
            }
            for (id, slot) in &mut slots.0 {
                if STOP.load(Ordering::Acquire) {
                    break;
                }
                if slot.child.is_none() && Instant::now() >= slot.next {
                    match self.spawn(&slot.spec) {
                        Ok(child) => slot.child = Some(child),
                        Err(_) => {
                            eprintln!(
                                "agent-inbox: Codex inbox {id} could not start; retrying with backoff"
                            );
                            slot.next = Instant::now() + Duration::from_secs(slot.backoff);
                            slot.backoff = (slot.backoff * 2).min(30);
                        }
                    }
                }
            }
            let deadline = Instant::now() + self.poll;
            while !STOP.load(Ordering::Acquire) && Instant::now() < deadline {
                std::thread::sleep(
                    Duration::from_millis(25)
                        .min(deadline.saturating_duration_since(Instant::now())),
                );
            }
        }
        Ok(())
    }
}

struct OwnedChild(Option<Child>);
impl OwnedChild {
    fn exited(&mut self) -> bool {
        self.0
            .as_mut()
            .is_some_and(|child| child.try_wait().ok().flatten().is_some())
    }
    fn signal(&mut self) {
        if !self.exited()
            && let Some(child) = &self.0
        {
            unsafe {
                libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
            }
        }
    }
    fn finish(&mut self, deadline: Instant) {
        self.signal();
        if let Some(mut child) = self.0.take() {
            while child.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            unsafe {
                libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
            }
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}
impl Drop for OwnedChild {
    fn drop(&mut self) {
        self.finish(Instant::now() + Duration::from_secs(2));
    }
}
struct Slot {
    spec: Spec,
    child: Option<OwnedChild>,
    next: Instant,
    backoff: u64,
}
struct Slots(BTreeMap<String, Slot>);
impl Drop for Slots {
    fn drop(&mut self) {
        for slot in self.0.values_mut() {
            if let Some(child) = &mut slot.child {
                child.signal();
            }
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        for slot in self.0.values_mut() {
            if let Some(child) = &mut slot.child {
                child.finish(deadline);
            }
        }
    }
}

pub(crate) fn validate_for_daemon(path: &Path) -> Result<(String, Option<PathBuf>)> {
    let watcher = Watcher::load(path)?;
    Ok((watcher.relay, watcher.control))
}

pub fn run(mut args: impl Iterator<Item = String>) -> Result<()> {
    let mut config = None;
    let mut parent = None;
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--config" => config = Some(PathBuf::from(args.next().ok_or("--config needs a path")?)),
            "--parent" => {
                parent = Some(
                    args.next()
                        .ok_or("missing watcher parent")?
                        .parse::<u32>()?,
                )
            }
            "--help" | "-h" => {
                println!("agent-inbox codex-watch --config <toml>");
                return Ok(());
            }
            _ => return Err("unknown codex-watch option".into()),
        }
    }
    Watcher::load(&config.ok_or("codex-watch requires --config")?)?.run(parent)
}

pub fn own_status(mut args: impl Iterator<Item = String>) -> Result<()> {
    let mut root = None;
    let mut status_file = None;
    let mut thread = std::env::var("CODEX_THREAD_ID").ok();
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--thread" => thread = Some(args.next().ok_or("--thread needs an ID")?),
            "--state-dir" => {
                root = Some(PathBuf::from(
                    args.next().ok_or("--state-dir needs a path")?,
                ))
            }
            "--status-file" => {
                status_file = Some(PathBuf::from(
                    args.next().ok_or("--status-file needs a path")?,
                ))
            }
            "--help" | "-h" => {
                println!("agent-inbox codex-self [--thread <id>] [--state-dir <watcher-state>]");
                return Ok(());
            }
            _ => return Err("unknown codex-self option".into()),
        }
    }
    let thread = thread.ok_or("--thread or CODEX_THREAD_ID is required")?;
    let path = match status_file {
        Some(path) => path,
        None => root
            .map(Ok)
            .unwrap_or_else(default_root)?
            .join("threads")
            .join(key(&thread)?)
            .join("session.json"),
    };
    let value = crate::codex_presence::read(&path, &thread)?;
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}
