//! Scheduled, cooperative Codex daemon upgrades. No real daemon changes in tests.
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    Result,
    codex::Client,
    codex_registry::{self, Gate, Record},
    command, http, state,
    token::Token,
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    registry_dir: Option<PathBuf>,
    codex_bin: Option<PathBuf>,
    settings_file: Option<PathBuf>,
    latest_command: Option<Vec<String>>,
    max_wait_secs: Option<u64>,
    poll_ms: Option<u64>,
    command_timeout_secs: Option<u64>,
    ready_timeout_secs: Option<u64>,
    notify_command: Option<PathBuf>,
    #[serde(default)]
    notify_args: Vec<String>,
}

struct Manager {
    config: Config,
    dir: PathBuf,
    codex: PathBuf,
    settings: PathBuf,
    wait: Duration,
    poll: Duration,
    command_timeout: Duration,
    ready: Duration,
}

impl Manager {
    fn load(path: &Path) -> Result<Self> {
        let config: Config = toml::from_str(&std::fs::read_to_string(path)?)
            .map_err(|_| "invalid upgrade TOML; check field names and syntax")?;
        let dir = config
            .registry_dir
            .clone()
            .map(Ok)
            .unwrap_or_else(codex_registry::default_dir)?;
        let settings = match &config.settings_file {
            Some(path) => path.clone(),
            None => {
                let root = match std::env::var_os("CODEX_HOME") {
                    Some(path) => PathBuf::from(path),
                    None => PathBuf::from(std::env::var_os("HOME").ok_or("HOME is unset")?)
                        .join(".codex"),
                };
                root.join("app-server-daemon/settings.json")
            }
        };
        let wait = config.max_wait_secs.unwrap_or(7200);
        let poll = config.poll_ms.unwrap_or(1000);
        let timeout = config.command_timeout_secs.unwrap_or(600);
        let ready = config.ready_timeout_secs.unwrap_or(120);
        if !(1..=72000).contains(&wait)
            || !(100..=60000).contains(&poll)
            || !(1..=3600).contains(&timeout)
            || !(1..=3600).contains(&ready)
        {
            return Err("upgrade timing out of range".into());
        }
        let codex = config.codex_bin.clone().unwrap_or_else(|| "codex".into());
        Ok(Self {
            config,
            dir,
            codex,
            settings,
            wait: Duration::from_secs(wait),
            poll: Duration::from_millis(poll),
            command_timeout: Duration::from_secs(timeout),
            ready: Duration::from_secs(ready),
        })
    }

    fn codex_json(&self, args: &[&str]) -> Result<Value> {
        let bytes = command::output(Command::new(&self.codex).args(args), self.command_timeout)?;
        serde_json::from_slice(&bytes).map_err(|_| "Codex command did not return valid JSON".into())
    }

    fn daemon(&self) -> Result<Value> {
        let bytes = command::output(
            Command::new(&self.codex).args(["app-server", "daemon", "version"]),
            Duration::from_secs(5),
        )?;
        let value: Value =
            serde_json::from_slice(&bytes).map_err(|_| "invalid daemon version JSON")?;
        if value["status"] != "running"
            || value["socketPath"].as_str().is_none()
            || value["appServerVersion"].as_str().is_none()
            || value["cliVersion"].as_str().is_none()
        {
            return Err("daemon unavailable or version response incompatible".into());
        }
        Ok(value)
    }

    fn latest(&self) -> Result<String> {
        let args = self.config.latest_command.clone().unwrap_or_else(|| {
            vec![
                "curl".into(),
                "--fail".into(),
                "--silent".into(),
                "--show-error".into(),
                "--connect-timeout".into(),
                "10".into(),
                "--max-time".into(),
                "30".into(),
                "https://api.github.com/repos/openai/codex/releases/latest".into(),
            ]
        });
        let executable = args.first().ok_or("latest_command cannot be empty")?;
        let value: Value = serde_json::from_slice(&command::output(
            Command::new(executable).args(&args[1..]),
            Duration::from_secs(45),
        )?)
        .map_err(|_| "latest release command returned invalid JSON")?;
        let version = value["version"]
            .as_str()
            .or_else(|| value["tag_name"].as_str())
            .ok_or("release version missing")?
            .trim_start_matches("rust-v")
            .to_owned();
        version_tuple(&version)?;
        if value["prerelease"] == true || value["draft"] == true {
            return Err("latest metadata is not a stable release".into());
        }
        Ok(version)
    }

    fn auto_disabled(&self) -> Result<()> {
        let settings: Value = serde_json::from_slice(&std::fs::read(&self.settings)?)
            .map_err(|_| "invalid daemon settings JSON")?;
        if settings["updater"]["autoUpdateEnabled"] != false {
            return Err("disable the daemon updater first; see upgrade-codex --disable-auto-update and docs/CODEX-UPGRADES.md".into());
        }
        // Settings are loaded at lifecycle boundaries. An already running loop must stop too.
        for name in ["daemon-updater.pid", "app-server-updater.pid"] {
            let path = self.settings.with_file_name(name);
            if path.exists() {
                let value: Value = serde_json::from_slice(&std::fs::read(path)?)
                    .map_err(|_| "unreadable updater PID record; inspect before scheduling")?;
                let pid = value["pid"]
                    .as_u64()
                    .filter(|pid| *pid > 1 && *pid <= i32::MAX as u64)
                    .ok_or("invalid updater PID record")?;
                #[cfg(unix)]
                if unsafe { libc::kill(pid as libc::pid_t, 0) } == 0
                    || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
                {
                    return Err("an updater PID is still live; apply the disabled setting before scheduling".into());
                }
            }
        }
        Ok(())
    }

    fn disable(&self) -> Result<()> {
        let mut value: Value = match std::fs::read(&self.settings) {
            Ok(bytes) => {
                serde_json::from_slice(&bytes).map_err(|_| "invalid daemon settings JSON")?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => json!({}),
            Err(error) => return Err(error.into()),
        };
        if !value.is_object() || value.get("updater").is_some_and(|v| !v.is_object()) {
            return Err("daemon settings/updater must be JSON objects".into());
        }
        if value["updater"]["autoUpdateEnabled"] != false {
            if value.get("updater").is_none() {
                value["updater"] = json!({});
            }
            value["updater"]["autoUpdateEnabled"] = json!(false);
            state::write_json(&self.settings, &value)?;
        }
        println!(
            "Saved updater.autoUpdateEnabled=false. Apply with an owner-coordinated `codex app-server daemon start`; verify the updater process stopped. This command does not restart the daemon."
        );
        Ok(())
    }

    fn control(&self, record: &Record, action: &str, gate: &Gate) -> Result<Value> {
        let token = Token::read(&record.token_file)?;
        let (status, body) = http::post_response_from(
            &record.address,
            &format!("/control/{action}"),
            &token,
            None,
            &json!({"instance":record.instance,"lease":gate.lease,"expires":gate.expires})
                .to_string(),
            Duration::from_secs(2),
        )?;
        http::accepted(status)?;
        if action == "status" {
            let value: Value = serde_json::from_str(&body)?;
            if value["instance"] != record.instance || value["thread"] != record.thread {
                return Err("registered inbox changed identity".into());
            }
            Ok(value)
        } else {
            Ok(Value::Null)
        }
    }

    fn hold_all(
        &self,
        gate: &Gate,
        records: &mut BTreeMap<String, Record>,
        socket: &Path,
        deadline: Instant,
    ) -> Result<bool> {
        let _barrier = codex_registry::barrier(&self.dir)?;
        let current = codex_registry::discover(&self.dir)?;
        let mut held = true;
        for record in current {
            if Instant::now() >= deadline {
                return Ok(false);
            }
            // One manager covers one daemon. Refuse mixed custom sockets rather than omit an inbox.
            if record.socket.as_deref().is_some_and(|s| s != socket) {
                return Err(
                    "registry contains an inbox for another daemon; use a separate registry/config"
                        .into(),
                );
            }
            records.insert(record.instance.clone(), record.clone());
            match self.control(&record, "status", gate) {
                Ok(status)
                    if status["holds"][&gate.lease] == gate.expires && status["held"] == true => {}
                Ok(_) => {
                    if Instant::now() >= deadline {
                        return Ok(false);
                    }
                    self.control(&record, "hold", gate)?;
                    held = false;
                }
                Err(_) => {
                    held = false;
                }
            }
        }
        Ok(held)
    }

    fn snapshot(&self, socket: &Path, deadline: Instant) -> Result<(bool, Vec<Value>)> {
        if Instant::now() >= deadline {
            return Err("gap wait expired".into());
        }
        let mut client = Client::connect(&self.codex, socket, Duration::from_secs(3))?;
        let mut cursor = None;
        let mut cursors = HashSet::new();
        let mut threads = HashSet::new();
        let mut tui = Vec::new();
        let mut idle = true;
        for _ in 0..1024 {
            if Instant::now() >= deadline {
                return Err("gap wait expired".into());
            }
            let page =
                client.request("thread/loaded/list", json!({"cursor":cursor,"limit":100}))?;
            let ids = page["data"]
                .as_array()
                .ok_or("loaded thread list missing")?;
            for id in ids {
                if Instant::now() >= deadline {
                    return Err("gap wait expired".into());
                }
                let id = id.as_str().ok_or("invalid loaded thread id")?;
                if !threads.insert(id.to_owned()) {
                    return Err("loaded thread pagination repeated a thread".into());
                }
                let thread = client.read_thread(id)?;
                match thread["status"]["type"].as_str() {
                    Some("active") => idle = false,
                    Some("idle" | "notLoaded") => {}
                    _ => return Err("unknown thread runtime status; upgrade skipped".into()),
                }
                if thread["source"] == "cli" {
                    tui.push(json!({"thread":id,"name":thread["name"],"cwd":thread["cwd"],"notice":"may need codex resume"}));
                }
            }
            match page.get("nextCursor") {
                None | Some(Value::Null) => return Ok((idle, tui)),
                Some(Value::String(next)) if cursors.insert(next.clone()) => {
                    cursor = Some(next.clone())
                }
                _ => return Err("incomplete loaded thread list".into()),
            }
        }
        Err("loaded thread pagination limit reached".into())
    }

    fn release(&self, gate: &Gate, records: &mut BTreeMap<String, Record>) -> Vec<String> {
        let mut failed = Vec::new();
        // Remove the startup barrier and collect late arrivals in one critical section.
        let cleanup = (|| -> Result<()> {
            let _barrier = codex_registry::barrier(&self.dir)?;
            let live = codex_registry::discover(&self.dir)?;
            records.retain(|id, _| live.iter().any(|record| &record.instance == id));
            for record in live {
                records.insert(record.instance.clone(), record);
            }
            let path = self.dir.join("hold.json");
            if path.exists() {
                let current: Gate = state::read_json(&path)?;
                if current.lease == gate.lease {
                    std::fs::remove_file(path)?;
                    std::fs::File::open(&self.dir)?.sync_all()?;
                }
            }
            Ok(())
        })();
        if cleanup.is_err() {
            failed.push("startup barrier (lease will expire)".into());
        }
        // Include every live attempted request, even if its hold ack was lost.
        // A dead inbox clears this maintenance lease against the gate on startup.
        for record in records.values() {
            if self.control(record, "release", gate).is_err() {
                failed.push(record.instance.clone());
            }
        }
        failed
    }

    fn run(&self) -> Result<()> {
        state::parent(&self.dir.join("upgrade.lock"))?;
        let lock = state::open_private(&self.dir.join("upgrade.lock"), true)?;
        if lock.try_lock().is_err() {
            return Err("another Codex upgrade manager is running".into());
        }
        self.auto_disabled()?;
        let before = self.daemon()?;
        let latest = self.latest()?;
        if version_tuple(&latest)? <= version_tuple(before["appServerVersion"].as_str().unwrap())? {
            return Ok(());
        }
        let socket = PathBuf::from(before["socketPath"].as_str().unwrap());
        // Reuse the maintenance lease under the kernel lock, so a later run can
        // release holds left by a crashed manager without touching manual leases.
        let lease_path = self.dir.join("maintenance-lease.json");
        let lease: String = if lease_path.exists() {
            state::read_json(&lease_path)?
        } else {
            let lease = state::id()?;
            state::write_json(&lease_path, &lease)?;
            lease
        };
        let gate = Gate {
            lease,
            expires: (chrono::Utc::now().timestamp().max(0) as u64)
                + self.wait.as_secs()
                + self.command_timeout.as_secs()
                + self.ready.as_secs()
                + 300,
        };
        let start = Instant::now();
        let mut records = BTreeMap::new();
        let mut tui = Vec::new();
        let mut tui_procs = Vec::new();
        let mut last_error = None;
        let result = (|| -> Result<&str> {
            {
                let _barrier = codex_registry::barrier(&self.dir)?;
                state::write_json(&self.dir.join("hold.json"), &gate)?;
            }
            let mut idle_checks = 0;
            let deadline = start + self.wait;
            loop {
                if start.elapsed() >= self.wait {
                    return Ok("skippedBusy");
                }
                let held = self.hold_all(&gate, &mut records, &socket, deadline)?;
                match self.snapshot(&socket, deadline) {
                    Ok((idle, attached)) => {
                        tui = attached;
                        if held && idle {
                            idle_checks += 1;
                        } else {
                            idle_checks = 0;
                        }
                        last_error = None;
                    }
                    Err(_) => {
                        last_error = Some("daemon status unavailable/incomplete".to_owned());
                        idle_checks = 0;
                    }
                }
                if idle_checks >= 2 {
                    self.auto_disabled()?;
                    // A final fresh snapshot narrows, but cannot atomically close, the TUI race.
                    let (idle, attached) = self.snapshot(&socket, deadline)?;
                    if idle && Instant::now() < deadline {
                        tui = attached;
                        tui_procs = tui_processes();
                        break;
                    }
                    idle_checks = 0;
                }
                std::thread::sleep(self.poll.min(self.wait.saturating_sub(start.elapsed())));
            }
            let _update = self.codex_json(&["app-server", "daemon", "update", "--yes"])?;
            let ready_deadline = Instant::now() + self.ready;
            loop {
                if let Ok(after) = self.daemon()
                    && version_tuple(after["appServerVersion"].as_str().unwrap())?
                        >= version_tuple(&latest)?
                    && after["appServerVersion"] != before["appServerVersion"]
                    && Client::connect(
                        &self.codex,
                        Path::new(after["socketPath"].as_str().unwrap()),
                        Duration::from_secs(3),
                    )
                    .is_ok()
                {
                    self.auto_disabled()?;
                    return Ok("updated");
                }
                if Instant::now() >= ready_deadline {
                    return Err("new daemon did not become ready; inspect daemon update result before retrying".into());
                }
                std::thread::sleep(self.poll);
            }
        })();
        let release_failed = self.release(&gate, &mut records);
        let after = self.daemon().ok();
        let target = after.as_ref().and_then(|v| v["appServerVersion"].as_str());
        let cli = after
            .as_ref()
            .and_then(|v| v["cliVersion"].as_str())
            .or_else(|| before["cliVersion"].as_str())
            .unwrap_or("unknown");
        let cli_command =
            target.and_then(|target| match (version_tuple(cli), version_tuple(target)) {
                (Ok(cli), Ok(daemon)) if cli < daemon => {
                    Some(format!("npm install -g @openai/codex@{target}"))
                }
                _ => None,
            });
        let report = json!({"event":"codexUpgrade", "status":result.as_ref().copied().unwrap_or("failed"),
            "from":before["appServerVersion"],"to":after.as_ref().map(|v| &v["appServerVersion"]),"latest":latest,
            "cliVersion":cli,"cliUpgradeCommand":cli_command,"holdSeconds":start.elapsed().as_secs_f64(),
            "tuiSessionsMayNeedResume":tui,"tuiProcesses":tui_procs,
            "releaseFailed":release_failed,"lease":gate.lease,"leaseExpires":gate.expires,
            "error":result.as_ref().err().map(|e| e.to_string()).or(last_error),
            "limit":"Cooperative idle snapshots; a TUI can start a turn in the gap. Daemon drain is bounded; inbox FIFO survives. CLI upgrade is manual."});
        state::write_json(&self.dir.join("last-upgrade.json"), &report)?;
        crate::codex_queue::notify(
            self.config.notify_command.as_deref(),
            &self.config.notify_args,
            &report,
        )?;
        let status = result?;
        if !release_failed.is_empty() {
            return Err("some hold releases failed; see last-upgrade.json and lease expiry".into());
        }
        println!(
            "Codex upgrade {status}; report: {}",
            self.dir.join("last-upgrade.json").display()
        );
        Ok(())
    }
}

fn version_tuple(version: &str) -> Result<(u64, u64, u64)> {
    let parts: Vec<_> = version.split('.').collect();
    if parts.len() != 3
        || parts
            .iter()
            .any(|part| part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()))
    {
        return Err("expected a stable three-part Codex version; no upgrade performed".into());
    }
    Ok((parts[0].parse()?, parts[1].parse()?, parts[2].parse()?))
}

fn tui_processes() -> Vec<Value> {
    let Ok(bytes) = command::output(
        Command::new("ps").args(["-axo", "pid=,tty=,comm="]),
        Duration::from_secs(2),
    ) else {
        return vec![];
    };
    String::from_utf8_lossy(&bytes)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next()?.parse::<u32>().ok()?;
            let tty = fields.next()?;
            let executable = fields.next()?;
            if tty == "??" || tty == "?" || Path::new(executable).file_name()?.to_str()? != "codex"
            {
                return None;
            }
            Some(json!({"pid":pid,"tty":tty,"notice":"may need codex resume"}))
        })
        .collect()
}

pub fn run(mut args: impl Iterator<Item = String>) -> Result<()> {
    let mut path = None;
    let mut disable = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => path = Some(PathBuf::from(args.next().ok_or("--config needs a path")?)),
            "--disable-auto-update" => disable = true,
            "--help" | "-h" => {
                println!("agent-inbox upgrade-codex --config <toml> [--disable-auto-update]");
                return Ok(());
            }
            _ => return Err("unknown upgrade-codex option".into()),
        }
    }
    let manager = Manager::load(&path.ok_or("upgrade-codex requires --config")?)?;
    if disable {
        manager.disable()
    } else {
        manager.run()
    }
}
