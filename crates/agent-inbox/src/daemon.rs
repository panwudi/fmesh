//! One desktop service owns the host relay and optional Codex/Grok watchers.
use std::fs::File;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::{Result, http, hub, local_control, state};

static STOP: AtomicBool = AtomicBool::new(false);
extern "C" fn stop(_: libc::c_int) {
    STOP.store(true, Ordering::Release);
}

pub(crate) fn watch_parent(parent: Option<u32>, stopped: &'static AtomicBool) -> Result<()> {
    if let Some(parent) = parent {
        if parent <= 1 || unsafe { libc::getppid() } as u32 != parent {
            return Err("desktop service parent is no longer current".into());
        }
        std::thread::spawn(move || {
            while !stopped.load(Ordering::Acquire) && unsafe { libc::getppid() } as u32 == parent {
                std::thread::sleep(Duration::from_millis(50));
            }
            stopped.store(true, Ordering::Release);
        });
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    state_dir: Option<PathBuf>,
    relay: Relay,
    codex_watch: Option<Watcher>,
    grok_watch: Option<Watcher>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Relay {
    listen: String,
    config: PathBuf,
    #[serde(default)]
    no_tunnel: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Watcher {
    config: PathBuf,
}

struct Service {
    name: &'static str,
    args: Vec<String>,
    child: Option<Child>,
    started: Instant,
    retry: Instant,
    backoff: u64,
}
impl Service {
    fn new(name: &'static str, args: Vec<String>) -> Self {
        Self {
            name,
            args,
            child: None,
            started: Instant::now(),
            retry: Instant::now(),
            backoff: 1,
        }
    }
    fn tick(&mut self, binary: &Path) -> Result<()> {
        if let Some(child) = &mut self.child
            && let Some(status) = child.try_wait()?
        {
            self.child.take();
            if self.started.elapsed() >= Duration::from_secs(60) {
                self.backoff = 1;
            }
            eprintln!(
                "agent-inbox: desktop {} exited ({status}); retry in {}s",
                self.name, self.backoff
            );
            self.retry = Instant::now() + Duration::from_secs(self.backoff);
            self.backoff = (self.backoff * 2).min(30);
        }
        if self.child.is_none() && Instant::now() >= self.retry {
            let child = Command::new(binary)
                .args(&self.args)
                .args(["--parent", &std::process::id().to_string()])
                .process_group(0)
                .stdin(Stdio::null())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit())
                .spawn();
            match child {
                Ok(child) => {
                    eprintln!(
                        "agent-inbox: desktop {} started (pid {})",
                        self.name,
                        child.id()
                    );
                    self.started = Instant::now();
                    self.child = Some(child);
                }
                Err(_) => {
                    eprintln!(
                        "agent-inbox: desktop {} could not start; retry in {}s",
                        self.name, self.backoff
                    );
                    self.retry = Instant::now() + Duration::from_secs(self.backoff);
                    self.backoff = (self.backoff * 2).min(30);
                }
            }
        }
        Ok(())
    }
    fn signal(&mut self) {
        if let Some(child) = &mut self.child
            && matches!(child.try_wait(), Ok(None))
        {
            unsafe {
                libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
            }
        }
    }
    fn finish(&mut self, deadline: Instant) {
        if let Some(mut child) = self.child.take() {
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => break,
                    _ if Instant::now() >= deadline => {
                        unsafe {
                            libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
                        }
                        let _ = child.kill();
                        let _ = child.wait();
                        break;
                    }
                    _ => std::thread::sleep(Duration::from_millis(20)),
                }
            }
        }
    }
}
struct Services(Vec<Service>);
impl Drop for Services {
    fn drop(&mut self) {
        // Stop discovery before its relay, but signal everything before waiting.
        for service in self.0.iter_mut().rev() {
            service.signal();
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        for service in self.0.iter_mut().rev() {
            service.finish(deadline);
        }
    }
}

fn load(path: &Path) -> Result<(PathBuf, Services)> {
    let path = std::fs::canonicalize(path)?;
    let config: Config = toml::from_str(&std::fs::read_to_string(&path)?)
        .map_err(|_| "invalid desktop daemon TOML; check fields and syntax")?;
    let listen = http::listen_address(&config.relay.listen)?;
    if !listen.ip().is_loopback() || listen.port() == 0 {
        return Err("desktop relay needs a fixed nonzero loopback address".into());
    }
    let relay_config = std::fs::canonicalize(hub::relative_to(&path, config.relay.config))?;
    let relay_control =
        match crate::relay::validate_for_daemon(&relay_config, config.relay.no_tunnel)? {
            Some(path) => path,
            None => local_control::default_path(listen.port())?,
        };
    let address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    if relay_control.as_os_str().as_bytes().len() >= address.sun_path.len() {
        return Err("control socket path is too long; choose a shorter control_socket in both component configs".into());
    }
    let mut args = vec![
        "relay".into(),
        "--listen".into(),
        listen.to_string(),
        "--config".into(),
        relay_config.display().to_string(),
    ];
    if config.relay.no_tunnel {
        args.push("--no-tunnel".into());
    }
    let mut services = Services(vec![Service::new("relay", args)]);
    if let Some(watcher) = config.codex_watch {
        let file = std::fs::canonicalize(hub::relative_to(&path, watcher.config))?;
        let (endpoint, control) = crate::codex_watch::validate_for_daemon(&file)?;
        let control = match control {
            Some(path) => path,
            None => local_control::default_path(listen.port())?,
        };
        if endpoint != listen.to_string() || control != relay_control {
            return Err("Codex watcher must use this desktop relay and its control socket".into());
        }
        services.0.push(Service::new(
            "codex-watch",
            vec![
                "codex-watch".into(),
                "--config".into(),
                file.display().to_string(),
            ],
        ));
    }
    if let Some(watcher) = config.grok_watch {
        let file = std::fs::canonicalize(hub::relative_to(&path, watcher.config))?;
        let (endpoint, control) = crate::grok_watch::validate_for_daemon(&file)?;
        if endpoint != listen.to_string() || control != relay_control {
            return Err("Grok watcher must use this desktop relay and its control socket".into());
        }
        services.0.push(Service::new(
            "grok-watch",
            vec![
                "grok-watch".into(),
                "--config".into(),
                file.display().to_string(),
            ],
        ));
    }
    let root = match config.state_dir {
        Some(p) => hub::relative_to(&path, p),
        None => crate::journal::default_path()?.with_file_name("desktop-daemon"),
    };
    Ok((root, services))
}

pub fn run(mut args: impl Iterator<Item = String>) -> Result<()> {
    let mut path = None;
    let mut check = false;
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--config" => {
                path = Some(PathBuf::from(
                    args.next().ok_or("--config requires a file")?,
                ))
            }
            "--check" => check = true,
            "--help" | "-h" => {
                println!("agent-inbox daemon --config <toml> [--check]");
                return Ok(());
            }
            _ => return Err("unknown desktop daemon option".into()),
        }
    }
    let (root, mut services) = load(&path.ok_or("daemon requires --config")?)?;
    if check {
        let codex = services
            .0
            .iter()
            .any(|service| service.name == "codex-watch");
        let grok = services
            .0
            .iter()
            .any(|service| service.name == "grok-watch");
        let suffix = match (codex, grok) {
            (false, false) => "",
            (true, false) => " and Codex auto-join",
            (false, true) => " and Grok auto-join",
            (true, true) => " and Codex auto-join and Grok auto-join",
        };
        println!("Desktop configuration valid: relay{suffix}");
        return Ok(());
    }
    local_control::private_dir(&root)?;
    let _lock: File = state::open_private(&root.join("daemon.lock"), true)?;
    _lock
        .try_lock()
        .map_err(|_| "another desktop daemon owns this state directory")?;
    STOP.store(false, Ordering::Release);
    for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        if unsafe { libc::signal(signal, stop as *const () as libc::sighandler_t) } == libc::SIG_ERR
        {
            return Err("cannot install desktop shutdown handler".into());
        }
    }
    let binary = std::env::current_exe()?;
    eprintln!(
        "agent-inbox: desktop service supervising {} component(s)",
        services.0.len()
    );
    while !STOP.load(Ordering::Acquire) {
        for service in &mut services.0 {
            if STOP.load(Ordering::Acquire) {
                break;
            }
            service.tick(&binary)?;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    drop(services);
    eprintln!("agent-inbox: desktop service stopped");
    Ok(())
}
