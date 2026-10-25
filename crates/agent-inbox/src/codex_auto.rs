//! One watcher-owned process per exact thread; Claude's join path is unchanged.
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use crate::{
    Result,
    claude_join::{Credentials, Join, Profile},
    codex_inbox,
    codex_presence::{Presence, Routing},
};

static STOP: AtomicBool = AtomicBool::new(false);
extern "C" fn stop(_: libc::c_int) {
    STOP.store(true, Ordering::Release);
}

pub fn run(mut args: impl Iterator<Item = String>) -> Result<()> {
    let mut thread = None;
    let mut relay = None;
    let mut name = None;
    let mut description = None;
    let mut control = None;
    let mut sessions = None;
    let mut queue = None;
    let mut log = None;
    let mut codex = PathBuf::from("codex");
    let mut socket = None;
    let mut registry = None;
    let mut status = None;
    let mut timeout = 10;
    let mut parent = None;
    let mut notify = None;
    let mut notify_args = Vec::new();
    while let Some(flag) = args.next() {
        if flag == "--help" {
            println!(
                "agent-inbox codex-worker --thread <id> --relay <addr> --queue-file <path> --log <path> --status-file <path> --session-dir <path> [--name <name>] [--parent <pid>] [Codex/notify options]"
            );
            return Ok(());
        }
        let value = args.next().ok_or("missing codex-worker option value")?;
        match flag.as_str() {
            "--thread" => thread = Some(value),
            "--relay" => relay = Some(value),
            "--name" => name = Some(value),
            "--description" => description = Some(value),
            "--control-socket" => control = Some(PathBuf::from(value)),
            "--session-dir" => sessions = Some(PathBuf::from(value)),
            "--queue-file" => queue = Some(PathBuf::from(value)),
            "--log" => log = Some(PathBuf::from(value)),
            "--codex-bin" => codex = value.into(),
            "--sock" => socket = Some(PathBuf::from(value)),
            "--registry-dir" => registry = Some(PathBuf::from(value)),
            "--status-file" => status = Some(PathBuf::from(value)),
            "--timeout-secs" => timeout = value.parse::<u64>()?,
            "--parent" => parent = Some(value.parse::<u32>()?),
            "--notify-command" => notify = Some(PathBuf::from(value)),
            "--notify-arg" => notify_args.push(value),
            _ => return Err("unknown codex-worker option".into()),
        }
    }
    let thread = thread.ok_or("codex-worker requires --thread")?;
    crate::codex_watch::key(&thread)?;
    if !(1..=60).contains(&timeout) {
        return Err("worker timeout must be 1..60 seconds".into());
    }
    if parent.is_some_and(|pid| pid <= 1 || pid != unsafe { libc::getppid() } as u32) {
        return Err("watcher parent changed before inbox startup".into());
    }
    let relay = crate::hub::loopback_endpoint(&relay.ok_or("worker requires --relay")?)?;
    let relay_addr: std::net::SocketAddr = relay.parse()?;
    let control = control
        .map(Ok)
        .unwrap_or_else(|| crate::local_control::default_path(relay_addr.port()))?;
    let queue_file = std::path::absolute(queue.ok_or("worker requires --queue-file")?)?;
    let log = std::path::absolute(log.ok_or("worker requires --log")?)?;
    let status = std::path::absolute(status.ok_or("worker requires --status-file")?)?;
    let name = name.unwrap_or_else(|| {
        crate::codex_watch::name_from_cwd(
            &std::env::current_dir()
                .unwrap_or_default()
                .to_string_lossy(),
        )
    });
    let description = description.unwrap_or_else(|| {
        format!("Codex loaded thread {thread}; online does not mean human attendance")
    });
    crate::hub::validate_component(&name)?;
    crate::directory::description(&description)?;
    let credentials = Credentials::create(&sessions.ok_or("worker requires --session-dir")?)?;
    let token = credentials.inbox.clone();
    let sender_file = std::path::absolute(credentials.sender_file())?;
    let token_file = sender_file.with_file_name("inbox.token");
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let inbox = listener.local_addr()?.to_string();
    let presence = Arc::new(Presence::open(
        status,
        Routing {
            thread: thread.clone(),
            inbox: inbox.clone(),
            relay: relay.clone(),
            token_file: token_file.clone(),
            sender_file,
            queue_file: queue_file.clone(),
        },
    )?);
    presence.publish(&Profile {
        name: None,
        description: description.clone(),
        registered: false,
    })?;
    let changes = Arc::clone(&presence);
    let journal = log.clone();
    let destination = thread.clone();
    let join = Join::start(
        control,
        relay,
        inbox,
        credentials,
        name,
        description,
        Arc::new(move |profile| {
            if changes.publish(profile).is_err() {
                eprintln!("agent-inbox: Codex auto-join status write failed");
            }
            let text = format!(
                "Codex auto-join: name {}, registered {}. Online means daemon-loaded/inbox reachable, not human attendance. Idle addressed messages can start autonomous work.",
                profile.name.as_deref().unwrap_or("pending"),
                profile.registered
            );
            let _ = crate::journal::append(&journal, "agent-inbox", &destination, &text);
        }),
    )?;
    let running = Arc::new(AtomicBool::new(true));
    STOP.store(false, Ordering::Release);
    for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        if unsafe { libc::signal(signal, stop as *const () as libc::sighandler_t) } == libc::SIG_ERR
        {
            return Err("cannot install worker shutdown handler".into());
        }
    }
    let monitor = Arc::clone(&running);
    std::thread::spawn(move || {
        while monitor.load(Ordering::Acquire)
            && !STOP.load(Ordering::Acquire)
            && parent.is_none_or(|pid| pid == unsafe { libc::getppid() } as u32)
        {
            std::thread::sleep(Duration::from_millis(25));
        }
        monitor.store(false, Ordering::Release);
    });
    let metadata = Arc::clone(&presence);
    let result = codex_inbox::serve(codex_inbox::Config {
        thread,
        listener,
        token,
        token_file,
        log,
        sender: "teammate".into(),
        codex,
        socket,
        timeout: Duration::from_secs(timeout),
        queue_file: Some(queue_file),
        notify,
        notify_args,
        registry_dir: registry,
        running: Arc::clone(&running),
        metadata: Some(Arc::new(move || metadata.status())),
        routing_context: Some(presence.context()),
    });
    running.store(false, Ordering::Release);
    drop(join);
    presence.remove();
    result
}
