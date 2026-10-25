use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::{
    Result,
    codex_queue::{Backend, Queue},
    http, journal,
    token::Token,
    validate_message,
};

pub struct Config {
    pub thread: String,
    pub listener: TcpListener,
    pub token: Token,
    pub token_file: PathBuf,
    pub log: PathBuf,
    pub sender: String,
    pub codex: PathBuf,
    pub socket: Option<PathBuf>,
    pub timeout: Duration,
    pub queue_file: Option<PathBuf>,
    pub notify: Option<PathBuf>,
    pub notify_args: Vec<String>,
    pub registry_dir: Option<PathBuf>,
    pub running: Arc<AtomicBool>,
    pub metadata: Option<Arc<dyn Fn() -> serde_json::Value + Send + Sync>>,
    pub routing_context: Option<String>,
}

pub fn run(mut args: impl Iterator<Item = String>) -> Result<()> {
    let mut thread = None;
    let mut listen = None;
    let mut token_file = std::env::var_os("AGENT_INBOX_TOKEN_FILE").map(PathBuf::from);
    let mut log = None;
    let mut sender = "claude".to_owned();
    let mut codex = PathBuf::from("codex");
    let mut socket = None;
    let mut timeout = 10_u64;
    let mut queue_file = None;
    let mut notify = None;
    let mut notify_args = Vec::new();
    let mut registry_dir = None;
    while let Some(flag) = args.next() {
        if flag == "--help" || flag == "-h" {
            println!(
                "agent-inbox codex --thread <id> --listen <addr> --token-file <path> [--log <path>] [--queue-file <path>] [--registry-dir <path>] [--notify-command <path>] [--notify-arg <arg>] [--from <default-sender>] [--codex-bin <path>] [--sock <path>] [--timeout-secs <n>]"
            );
            return Ok(());
        }
        if !matches!(
            flag.as_str(),
            "--thread"
                | "--listen"
                | "--token-file"
                | "--log"
                | "--from"
                | "--codex-bin"
                | "--sock"
                | "--timeout-secs"
                | "--queue-file"
                | "--notify-command"
                | "--notify-arg"
                | "--registry-dir"
        ) {
            return Err(format!("unknown codex option {flag}").into());
        }
        let value = args
            .next()
            .ok_or_else(|| format!("missing value for {flag}"))?;
        match flag.as_str() {
            "--thread" => thread = Some(value),
            "--listen" => listen = Some(http::listen_address(&value)?),
            "--token-file" => token_file = Some(PathBuf::from(value)),
            "--log" => log = Some(PathBuf::from(value)),
            "--from" => sender = value,
            "--codex-bin" => codex = PathBuf::from(value),
            "--sock" => socket = Some(PathBuf::from(value)),
            "--timeout-secs" => timeout = value.parse()?,
            "--queue-file" => queue_file = Some(PathBuf::from(value)),
            "--notify-command" => notify = Some(PathBuf::from(value)),
            "--notify-arg" => notify_args.push(value),
            "--registry-dir" => registry_dir = Some(PathBuf::from(value)),
            _ => unreachable!(),
        }
    }
    let thread = thread.ok_or("--thread is required")?;
    let listen = listen.ok_or("--listen is required; choose an address owned by this session")?;
    validate_message(&thread, &sender, "configuration")?;
    if !(1..=60).contains(&timeout) {
        return Err("--timeout-secs must be between 1 and 60".into());
    }
    let token_file = token_file.ok_or("--token-file or AGENT_INBOX_TOKEN_FILE is required")?;
    let token = Token::read(&token_file)?;
    let log = match log {
        Some(path) => path,
        None => journal::default_path()?,
    };
    let listener = TcpListener::bind(listen)?;
    serve(Config {
        thread,
        listener,
        token,
        token_file,
        log,
        sender,
        codex,
        socket,
        timeout: Duration::from_secs(timeout),
        queue_file,
        notify,
        notify_args,
        registry_dir,
        running: Arc::new(AtomicBool::new(true)),
        metadata: None,
        routing_context: None,
    })
}

pub fn serve(config: Config) -> Result<()> {
    let Config {
        thread,
        listener,
        token,
        token_file,
        log,
        sender,
        codex,
        socket,
        timeout,
        queue_file,
        notify,
        notify_args,
        registry_dir,
        running,
        metadata,
        routing_context,
    } = config;
    let address = listener.local_addr()?;
    let queue_file = queue_file.unwrap_or_else(|| {
        log.with_file_name(format!(
            "codex-{}.queue.json",
            thread
                .as_bytes()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        ))
    });
    let queue = Queue::open(queue_file, &thread)?;
    let socket = socket.map(std::path::absolute).transpose()?;
    let instance = crate::state::id()?;
    let registry_dir = registry_dir
        .map(Ok)
        .unwrap_or_else(crate::codex_registry::default_dir)?;
    let _registration = crate::codex_registry::Registration::new(
        &registry_dir,
        &crate::codex_registry::Record {
            instance: instance.clone(),
            address: address.to_string(),
            token_file: std::path::absolute(&token_file)?,
            thread: thread.clone(),
            socket: socket.clone(),
        },
        &queue,
    )?;
    let push_blocked_notice = metadata.is_some();
    queue.start(Backend {
        codex,
        socket,
        timeout,
        thread: thread.clone(),
        log: log.clone(),
        address: address.to_string(),
        token_file,
        notify,
        notify_args,
        routing_context,
        push_blocked_notice,
        running: Arc::clone(&running),
    });
    let shutdown = Arc::clone(&queue);
    let admission = Mutex::new(());
    let handler = Arc::new(
        move |path: &str, text: &str, from: Option<&str>, _: http::Origin| {
            if path == "/control/status" {
                return match queue.status() {
                    Ok(mut status) => {
                        status["instance"] = serde_json::json!(instance);
                        if let Some(metadata) = &metadata {
                            status["autoJoin"] = metadata();
                        }
                        http::Response::json(200, status)
                    }
                    Err(_) => http::Response::new(500, "queue status unavailable"),
                };
            }
            if path == "/control/hold" || path == "/control/release" {
                let value: serde_json::Value = match serde_json::from_str(text) {
                    Ok(value) => value,
                    Err(_) => return http::Response::new(400, "invalid hold body"),
                };
                if value["instance"] != instance {
                    return http::Response::new(409, "inbox instance changed");
                }
                let Some(lease) = value["lease"].as_str() else {
                    return http::Response::new(400, "hold needs a lease");
                };
                let result = if path.ends_with("hold") {
                    queue.hold(lease, value["expires"].as_u64().unwrap_or(0))
                } else {
                    queue.release(lease)
                };
                return match result {
                    Ok(()) => http::Response::new(200, "hold state persisted"),
                    Err(_) => http::Response::new(409, "hold state could not be saved"),
                };
            }
            if path == "/control/resend" || path == "/control/drop" {
                let id = serde_json::from_str::<serde_json::Value>(text)
                    .ok()
                    .and_then(|value| value["id"].as_str().map(str::to_owned));
                let Some(id) = id else {
                    return http::Response::new(400, "control body needs a message id");
                };
                return match queue.resolve(&id, path == "/control/resend") {
                    Ok(()) => {
                        let _ = journal::append(
                            &log,
                            "agent-inbox",
                            &thread,
                            &format!(
                                "Explicit {} for message {id}",
                                if path.ends_with("resend") {
                                    "resend"
                                } else {
                                    "drop"
                                }
                            ),
                        );
                        http::Response::new(200, "queue resolution recorded")
                    }
                    Err(_) => http::Response::new(
                        409,
                        "message is not blocked/unconfirmed or resolution could not be saved",
                    ),
                };
            }
            if path != "/" {
                return http::Response::new(404, "unknown endpoint");
            }
            let Ok(_admission) = admission.lock() else {
                return http::Response::new(500, "inbox admission failed");
            };
            let from = from.unwrap_or(&sender);
            if journal::append(&log, from, &thread, text).is_err() {
                eprintln!("agent-inbox: message journal write failed; delivery skipped");
                return http::Response::new(500, "journal write failed; message was not delivered");
            }
            match queue.enqueue(from, text) {
                Ok(_) => http::Response::new(202, "message durably queued for the Codex session"),
                Err(_) => http::Response::new(
                    503,
                    "queue acceptance unconfirmed; message saved to journal; inspect queue before retrying",
                ),
            }
        },
    );
    eprintln!("agent-inbox: listening on http://{address}/");
    let authenticate = Arc::new(move |header: &[u8]| {
        token
            .authorizes(header)
            .then_some(http::SenderIdentity::Header)
    });
    let result = http::serve_authenticated(listener, Arc::clone(&running), authenticate, handler);
    running.store(false, std::sync::atomic::Ordering::Release);
    shutdown.finish_notifier();
    result
}
