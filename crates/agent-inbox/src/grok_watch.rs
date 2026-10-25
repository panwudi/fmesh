//! Watch Grok Build's shared leader and auto-register exact resident sessions.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    Result,
    claude_join::{Credentials, Join, Profile},
    grok_acp::{self, Session, Shared},
    grok_queue::Queue,
    http, journal, state,
};

static STOP: AtomicBool = AtomicBool::new(false);
extern "C" fn stop(_: libc::c_int) {
    STOP.store(true, Ordering::Release);
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    relay: String,
    state_dir: Option<PathBuf>,
    control_socket: Option<PathBuf>,
    grok_bin: Option<PathBuf>,
    leader_socket: Option<PathBuf>,
    poll_ms: Option<u64>,
    rpc_timeout_secs: Option<u64>,
    max_sessions: Option<usize>,
}

struct Watcher {
    relay: String,
    root: PathBuf,
    control: PathBuf,
    grok: PathBuf,
    socket: PathBuf,
    poll: Duration,
    rpc: Duration,
    max_sessions: usize,
}

struct Inbox {
    cwd: String,
    running: Arc<AtomicBool>,
    _join: Join,
    status_file: PathBuf,
    server: Option<std::thread::JoinHandle<()>>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Inbox {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Release);
        if let Some(server) = self.server.take() {
            let _ = server.join();
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        let _ = std::fs::remove_file(&self.status_file);
    }
}

impl Watcher {
    fn load(path: &Path) -> Result<Self> {
        let path = std::fs::canonicalize(path)?;
        let config: Config = toml::from_str(&std::fs::read_to_string(&path)?)
            .map_err(|_| "invalid Grok watcher TOML; check fields and syntax")?;
        let absolute = |value: PathBuf| -> Result<PathBuf> {
            Ok(std::path::absolute(crate::hub::relative_to(&path, value))?)
        };
        let root = config
            .state_dir
            .map(absolute)
            .unwrap_or_else(default_root)?;
        let relay = crate::hub::loopback_endpoint(&config.relay)?;
        let relay_addr: std::net::SocketAddr = relay.parse()?;
        let control = config
            .control_socket
            .map(absolute)
            .unwrap_or_else(|| crate::local_control::default_path(relay_addr.port()))?;
        let grok = config
            .grok_bin
            .map(|bin| {
                if bin.components().count() > 1 {
                    absolute(bin)
                } else {
                    Ok(bin)
                }
            })
            .transpose()?
            .unwrap_or_else(|| PathBuf::from("grok"));
        let socket = config
            .leader_socket
            .map(absolute)
            .unwrap_or_else(grok_acp::default_socket)?;
        let poll = config.poll_ms.unwrap_or(2000);
        let rpc = config.rpc_timeout_secs.unwrap_or(5);
        let max_sessions = config.max_sessions.unwrap_or(128);
        if !(100..=60000).contains(&poll)
            || !(1..=60).contains(&rpc)
            || !(1..=256).contains(&max_sessions)
        {
            return Err("Grok watcher limits out of range".into());
        }
        Ok(Self {
            relay,
            root,
            control,
            grok,
            socket,
            poll: Duration::from_millis(poll),
            rpc: Duration::from_secs(rpc),
            max_sessions,
        })
    }

    fn start_inbox(&self, session: &Session, connection: Arc<Shared>) -> Result<Inbox> {
        let key: String = session
            .id
            .bytes()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let dir = self.root.join("threads").join(key);
        let log = dir.join("messages.md");
        let queue = Queue::open(dir.join("queue.json"), &session.id)?;
        let credentials = Credentials::create(&self.root.join("credentials"))?;
        let token = credentials.inbox.clone();
        let sender_file = credentials.sender_file();
        let inbox_token_file = sender_file.with_file_name("inbox.token");
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?.to_string();
        let status_file = dir.join("session.json");
        let executable = crate::claude::shell_quote(&std::env::current_exe()?.to_string_lossy());
        let auth = format!(
            "--hub {} --token-file {}",
            crate::claude::shell_quote(&self.relay),
            crate::claude::shell_quote(&sender_file.to_string_lossy())
        );
        let status_command = format!(
            "{executable} grok-self --session {} --status-file {}",
            crate::claude::shell_quote(&session.id),
            crate::claude::shell_quote(&status_file.to_string_lossy())
        );
        let routing = format!(
            "[Reply to the incoming sender with: {executable} send {auth} --to <sender-logical-name> <reply-text>. Find agents with: {executable} who {auth}. If this inbox restarts, refresh the commands with: {status_command}. Use the exact sender name in the received message. Do not read or print token contents.] "
        );
        let base_status = json!({"session":session.id,"cwd":session.cwd,"inbox":address,"relay":self.relay,"senderTokenFile":sender_file,"inboxTokenFile":inbox_token_file,"queueFile":dir.join("queue.json"),"replyCommand":format!("{executable} send {auth} --to <sender-logical-name> <reply-text>"),"directoryCommand":format!("{executable} who {auth}"),"statusCommand":status_command});
        state::write_json(&status_file, &base_status)?;
        let name = crate::claude_join::typed_name(Path::new(&session.cwd), "grok");
        let description = format!(
            "Grok Build resident session {}; inbox may queue while the leader is unavailable",
            session.id
        );
        let status_update = status_file.clone();
        let base_update = base_status.clone();
        let join = Join::start(
            self.control.clone(),
            self.relay.clone(),
            address.clone(),
            credentials,
            name,
            description,
            Arc::new(move |profile: &Profile| {
                let mut status = base_update.clone();
                status["profile"] = serde_json::to_value(profile).unwrap_or(Value::Null);
                if state::write_json(&status_update, &status).is_err() {
                    eprintln!("agent-inbox: Grok routing status could not be saved");
                }
            }),
        )?;
        let running = Arc::new(AtomicBool::new(true));
        let worker = queue.start(
            connection,
            session.id.clone(),
            routing,
            log.clone(),
            Arc::clone(&running),
        );
        let handler_queue = Arc::clone(&queue);
        let handler_log = log;
        let handler_session = session.id.clone();
        let admission = Mutex::new(());
        let handler = Arc::new(
            move |path: &str, text: &str, from: Option<&str>, _: http::Origin| {
                if path == "/control/status" {
                    return match handler_queue.status() {
                        Ok(value) => http::Response::json(200, value),
                        Err(_) => http::Response::new(500, "Grok queue status unavailable"),
                    };
                }
                if path == "/control/resend" || path == "/control/drop" {
                    let id = serde_json::from_str::<Value>(text)
                        .ok()
                        .and_then(|value| value["id"].as_str().map(str::to_owned));
                    let Some(id) = id else {
                        return http::Response::new(400, "control body needs a message id");
                    };
                    return match handler_queue.resolve(&id, path.ends_with("resend")) {
                        Ok(()) => {
                            let _ = journal::append(
                                &handler_log,
                                "agent-inbox",
                                &handler_session,
                                &format!(
                                    "Explicit {} for Grok message {id}",
                                    if path.ends_with("resend") {
                                        "resend"
                                    } else {
                                        "drop"
                                    }
                                ),
                            );
                            http::Response::new(200, "Grok queue resolution recorded")
                        }
                        Err(_) => http::Response::new(409, "Grok queue resolution rejected"),
                    };
                }
                if path != "/" {
                    return http::Response::new(404, "unknown endpoint");
                }
                let Ok(_guard) = admission.lock() else {
                    return http::Response::new(500, "Grok inbox admission failed");
                };
                let sender = from.unwrap_or("teammate");
                if journal::append(&handler_log, sender, &handler_session, text).is_err() {
                    return http::Response::new(500, "Grok journal failed; delivery skipped");
                }
                match handler_queue.enqueue(sender, text) {
                    Ok(_) => http::Response::new(202, "message durably queued for Grok"),
                    Err(_) => {
                        http::Response::new(503, "Grok queue unconfirmed; message saved to journal")
                    }
                }
            },
        );
        let server_running = Arc::clone(&running);
        let authenticate = Arc::new(move |header: &[u8]| {
            token
                .authorizes(header)
                .then_some(http::SenderIdentity::Header)
        });
        let server = std::thread::spawn(move || {
            if let Err(error) =
                http::serve_authenticated(listener, server_running, authenticate, handler)
            {
                eprintln!("agent-inbox: Grok inbox stopped: {error}");
            }
        });
        Ok(Inbox {
            cwd: session.cwd.clone(),
            running,
            _join: join,
            status_file,
            server: Some(server),
            worker: Some(worker),
        })
    }
}

pub(crate) fn validate_for_daemon(path: &Path) -> Result<(String, PathBuf)> {
    let watcher = Watcher::load(path)?;
    Ok((watcher.relay, watcher.control))
}

fn default_root() -> Result<PathBuf> {
    Ok(journal::default_path()?.with_file_name("grok-watch"))
}

pub fn own_status(mut args: impl Iterator<Item = String>) -> Result<()> {
    let mut session = std::env::var("GROK_SESSION_ID").ok();
    let mut root = None;
    let mut status_file = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--session" => session = Some(args.next().ok_or("missing Grok session ID")?),
            "--state-dir" => {
                root = Some(PathBuf::from(args.next().ok_or("missing Grok state dir")?))
            }
            "--status-file" => {
                status_file = Some(PathBuf::from(
                    args.next().ok_or("missing Grok status file")?,
                ))
            }
            "--help" | "-h" => {
                println!(
                    "agent-inbox grok-self --session <id> [--state-dir <watcher-state> | --status-file <path>]"
                );
                return Ok(());
            }
            _ => return Err("unknown grok-self option".into()),
        }
    }
    let session = session.ok_or("grok-self requires --session or GROK_SESSION_ID")?;
    if !state::is_id(&session) {
        return Err("invalid Grok session ID".into());
    }
    let key: String = session.bytes().map(|byte| format!("{byte:02x}")).collect();
    let status_file = match status_file {
        Some(path) => path,
        None => root
            .map(Ok)
            .unwrap_or_else(default_root)?
            .join("threads")
            .join(key)
            .join("session.json"),
    };
    let value: Value = state::read_json(&status_file)?;
    if value["session"] != session {
        return Err("Grok status belongs to another session".into());
    }
    let sender_file = Path::new(
        value["senderTokenFile"]
            .as_str()
            .ok_or("Grok status has no sender file")?,
    );
    let alive = state::open_private(&sender_file.with_file_name("alive"), false)?;
    if alive.try_lock().is_ok() {
        return Err("Grok inbox is not running; stored status is stale".into());
    }
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

pub fn run(mut args: impl Iterator<Item = String>) -> Result<()> {
    let mut config = None;
    let mut parent = None;
    let mut check = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => {
                config = Some(PathBuf::from(
                    args.next().ok_or("missing Grok watcher config")?,
                ))
            }
            "--help" | "-h" => {
                println!("agent-inbox grok-watch --config <toml> [--check]");
                return Ok(());
            }
            "--check" => check = true,
            "--parent" => {
                parent = Some(
                    args.next()
                        .ok_or("missing Grok watcher parent")?
                        .parse::<u32>()?,
                )
            }
            _ => return Err("unknown Grok watcher option".into()),
        }
    }
    let watcher = Watcher::load(&config.ok_or("grok-watch requires --config")?)?;
    if check {
        let connection = Shared::new(watcher.grok, watcher.socket, watcher.rpc);
        let sessions = connection.roster()?;
        println!("Grok leader ready: {} resident session(s)", sessions.len());
        return Ok(());
    }
    state::parent(&watcher.root.join("watch.lock"))?;
    let lock = state::open_private(&watcher.root.join("watch.lock"), true)?;
    lock.try_lock()
        .map_err(|_| "another Grok watcher owns this state directory")?;
    STOP.store(false, Ordering::Release);
    for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        if unsafe { libc::signal(signal, stop as *const () as libc::sighandler_t) } == libc::SIG_ERR
        {
            return Err("cannot install Grok watcher shutdown handler".into());
        }
    }
    crate::daemon::watch_parent(parent, &STOP)?;
    let connection = Arc::new(Shared::new(
        watcher.grok.clone(),
        watcher.socket.clone(),
        watcher.rpc,
    ));
    let mut inboxes: HashMap<String, Inbox> = HashMap::new();
    let mut unavailable_reported = false;
    while !STOP.load(Ordering::Acquire) {
        match connection.roster() {
            Ok(sessions) => {
                unavailable_reported = false;
                let empty = sessions.is_empty();
                if sessions.len() > watcher.max_sessions {
                    eprintln!(
                        "agent-inbox: Grok roster exceeds configured limit; retaining inboxes"
                    );
                } else {
                    let ids: HashSet<_> =
                        sessions.iter().map(|session| session.id.as_str()).collect();
                    inboxes.retain(|id, _| ids.contains(id.as_str()));
                    for session in sessions {
                        if inboxes
                            .get(&session.id)
                            .is_some_and(|inbox| inbox.cwd == session.cwd)
                        {
                            continue;
                        }
                        inboxes.remove(&session.id);
                        match watcher.start_inbox(&session, Arc::clone(&connection)) {
                            Ok(inbox) => {
                                inboxes.insert(session.id, inbox);
                            }
                            Err(error) => {
                                eprintln!("agent-inbox: Grok inbox start failed: {error}")
                            }
                        }
                    }
                }
                if empty {
                    connection.disconnect();
                }
            }
            Err(error) if !unavailable_reported => {
                eprintln!(
                    "agent-inbox: Grok leader unavailable; retaining durable inboxes: {error}"
                );
                unavailable_reported = true;
            }
            Err(_) => {}
        }
        let mut elapsed = Duration::ZERO;
        while elapsed < watcher.poll && !STOP.load(Ordering::Acquire) {
            let pause = Duration::from_millis(100).min(watcher.poll - elapsed);
            std::thread::sleep(pause);
            elapsed += pause;
        }
    }
    drop(inboxes);
    Ok(())
}
