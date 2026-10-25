#![cfg(unix)]
use std::io::{BufRead, BufReader};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::{Duration, Instant};

use agent_inbox::{codex_registry, http, state, token::Token};
use serde_json::{Value, json};
use tungstenite::Message;
mod support;
static NEXT: AtomicU64 = AtomicU64::new(0);

struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let path = PathBuf::from(format!(
            "/tmp/ai-upgrade-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("token"), "upgrade-fake-private-token").unwrap();
        std::fs::set_permissions(path.join("token"), std::fs::Permissions::from_mode(0o600))
            .unwrap();
        for (file, version) in [("before.json", "0.162.1"), ("after.json", "0.163.0")] {
            std::fs::write(
                path.join(file),
                json!({"status":"running","appServerVersion":version,
                "cliVersion":"0.162.1","socketPath":path.join("server.sock")})
                .to_string(),
            )
            .unwrap();
        }
        state::write_json(&path.join("settings.json"), &json!({"remoteControlEnabled":true,"other":{"keep":1},"updater":{"autoUpdateEnabled":false,"updateIntervalMinutes":137}})).unwrap();
        std::fs::write(
            path.join("latest.json"),
            r#"{"tag_name":"rust-v0.163.0","prerelease":false}"#,
        )
        .unwrap();
        let fake = support::fake_codex();
        let config = format!(
            "registry_dir = {:?}\ncodex_bin = {:?}\nsettings_file = {:?}\nlatest_command = [{:?}, 'test-latest']\nmax_wait_secs = 2\npoll_ms = 100\ncommand_timeout_secs = 2\nready_timeout_secs = 1\nnotify_command = {:?}\nnotify_args = ['test-notify']\n",
            path.join("registry"),
            fake,
            path.join("settings.json"),
            fake,
            fake
        );
        std::fs::write(path.join("upgrade.toml"), config).unwrap();
        Self(path)
    }
    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agent-inbox"));
        command
            .args(["upgrade-codex", "--config"])
            .arg(self.0.join("upgrade.toml"))
            .env("AGENT_INBOX_TEST_DAEMON", &self.0)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }
    fn record(&self) -> codex_registry::Record {
        let _barrier = codex_registry::barrier(&self.0.join("registry")).unwrap();
        codex_registry::discover(&self.0.join("registry"))
            .unwrap()
            .pop()
            .unwrap()
    }
    fn report(&self) -> Value {
        state::read_json(&self.0.join("registry/last-upgrade.json")).unwrap()
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Inbox(Child);
impl Inbox {
    fn start(temp: &Temp) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_agent-inbox"))
            .args([
                "codex",
                "--thread",
                "upgrade-thread",
                "--listen",
                "0",
                "--token-file",
            ])
            .arg(temp.0.join("token"))
            .arg("--log")
            .arg(temp.0.join("journal.md"))
            .arg("--queue-file")
            .arg(temp.0.join("queue.json"))
            .arg("--registry-dir")
            .arg(temp.0.join("registry"))
            .arg("--codex-bin")
            .arg(support::fake_codex())
            .arg("--sock")
            .arg(temp.0.join("server.sock"))
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stderr = BufReader::new(child.stderr.take().unwrap());
        loop {
            let mut line = String::new();
            assert_ne!(stderr.read_line(&mut line).unwrap(), 0, "inbox failed");
            if line.contains("listening on") {
                break;
            }
        }
        std::thread::spawn(move || for _ in stderr.lines() {});
        Self(child)
    }
}
impl Drop for Inbox {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
struct OwnedChild(Option<Child>);
impl Drop for OwnedChild {
    fn drop(&mut self) {
        if let Some(child) = &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}
impl OwnedChild {
    fn wait(mut self) -> std::process::Output {
        self.0.take().unwrap().wait_with_output().unwrap()
    }
}

fn wait(check: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(8);
    while !check() {
        assert!(Instant::now() < deadline, "condition timed out");
        std::thread::sleep(Duration::from_millis(25));
    }
}
fn control(record: &codex_registry::Record, action: &str, body: Value) -> (u16, Value) {
    let (status, body) = http::post_response(
        &record.address,
        &format!("/control/{action}"),
        &Token::read(&record.token_file).unwrap(),
        &body.to_string(),
    )
    .unwrap();
    (status, serde_json::from_str(&body).unwrap_or(Value::Null))
}
fn status(record: &codex_registry::Record) -> Value {
    control(record, "status", json!({})).1
}
fn send(record: &codex_registry::Record, text: &str) {
    assert_eq!(
        http::send(
            &record.address,
            &Token::read(&record.token_file).unwrap(),
            "fake",
            text
        )
        .unwrap(),
        202
    );
}

struct Server {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    path: PathBuf,
}
impl Server {
    fn start(temp: &Temp) -> Self {
        let path = temp.0.join("server.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let root = temp.0.clone();
        let thread = std::thread::spawn(move || {
            let mut workers = Vec::new();
            for stream in listener.incoming() {
                if stopping.load(Ordering::Relaxed) {
                    break;
                }
                let root = root.clone();
                workers.push(std::thread::spawn(move || serve(stream.unwrap(), &root)));
            }
            for worker in workers {
                worker.join().unwrap();
            }
        });
        Self {
            stop,
            thread: Some(thread),
            path,
        }
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = UnixStream::connect(&self.path);
        self.thread.take().unwrap().join().unwrap();
    }
}
fn serve(stream: UnixStream, root: &Path) {
    stream
        .set_read_timeout(Some(Duration::from_secs(4)))
        .unwrap();
    let Ok(mut socket) = tungstenite::accept(stream) else {
        return;
    };
    while let Ok(Message::Text(text)) = socket.read() {
        let request: Value = serde_json::from_str(&text).unwrap();
        let method = request["method"].as_str().unwrap();
        let result = match method {
            "initialize" => {
                json!({"userAgent":"fake","platformFamily":"unix","platformOs":"macos","codexHome":"/fake"})
            }
            "initialized" => continue,
            "thread/loaded/list" => {
                if root.join("invalid-history").exists() {
                    json!({"data":[],"nextCursor":42})
                } else if request["params"]["cursor"].is_null() {
                    json!({"data":["upgrade-thread"],"nextCursor":"page2"})
                } else {
                    json!({"data":["other-thread"]})
                }
            }
            "thread/read" => {
                json!({"thread":{"id":request["params"]["threadId"],"status":{"type":if root.join("busy").exists() { "active" } else { "idle" }},"source":"cli","name":"test TUI","cwd":"/fake/project"}})
            }
            "thread/turns/list" => json!({"data":[{"id":"busy-turn","status":"inProgress"}]}),
            "turn/start" | "turn/steer" => {
                let mut log = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(root.join("delivered.jsonl"))
                    .unwrap();
                use std::io::Write;
                writeln!(log, "{}", request["params"]).unwrap();
                if method == "turn/start" {
                    json!({"turn":{"id":"new-turn"}})
                } else {
                    json!({"turnId":"busy-turn"})
                }
            }
            other => panic!("unexpected fake request {other}"),
        };
        if socket
            .send(Message::Text(
                json!({"id":request["id"],"result":result})
                    .to_string()
                    .into(),
            ))
            .is_err()
        {
            return;
        }
    }
}

#[test]
fn no_new_version_is_quiet_and_does_not_hold_or_update() {
    let temp = Temp::new();
    std::fs::write(temp.0.join("latest.json"), r#"{"version":"0.162.1"}"#).unwrap();
    let output = temp.command().output().unwrap();
    assert!(output.status.success(), "{:?}", output);
    assert!(output.stdout.is_empty() && output.stderr.is_empty());
    assert!(!temp.0.join("update-requested").exists());
    assert!(!temp.0.join("registry/hold.json").exists());
    assert!(!temp.0.join("notices.jsonl").exists());
}

#[test]
fn disable_preserves_other_settings_without_a_daemon_restart() {
    let temp = Temp::new();
    let mut settings: Value = state::read_json(&temp.0.join("settings.json")).unwrap();
    settings["updater"]["autoUpdateEnabled"] = json!(true);
    state::write_json(&temp.0.join("settings.json"), &settings).unwrap();
    let refused = temp.command().output().unwrap();
    assert!(!refused.status.success());
    assert!(!temp.0.join("update-requested").exists());
    let output = temp
        .command()
        .arg("--disable-auto-update")
        .output()
        .unwrap();
    assert!(output.status.success());
    settings["updater"]["autoUpdateEnabled"] = json!(false);
    let actual: Value = state::read_json(&temp.0.join("settings.json")).unwrap();
    assert_eq!(actual, settings);
    assert!(!temp.0.join("update-requested").exists());
}

#[test]
fn upgrade_holds_messages_and_new_inboxes_then_releases_fifo_and_reports_cli() {
    let temp = Temp::new();
    let _server = Server::start(&temp);
    let _inbox = Inbox::start(&temp);
    let record = temp.record();
    std::fs::write(temp.0.join("wait-update"), "1").unwrap();
    let manager = OwnedChild(Some(temp.command().spawn().unwrap()));
    wait(|| temp.0.join("update-requested").exists());
    assert_eq!(status(&record)["held"], true);
    send(&record, "first");
    send(&record, "second");
    assert_eq!(status(&record)["pending"].as_array().unwrap().len(), 2);
    assert!(!temp.0.join("delivered.jsonl").exists());
    // A separate queue starting after discovery inherits the host-wide gate.
    let extra =
        agent_inbox::codex_queue::Queue::open(temp.0.join("late.queue"), "late-thread").unwrap();
    let late_record = codex_registry::Record {
        instance: state::id().unwrap(),
        address: "127.0.0.1:1".into(),
        token_file: temp.0.join("token"),
        thread: "late-thread".into(),
        socket: Some(temp.0.join("server.sock")),
    };
    let registration =
        codex_registry::Registration::new(&temp.0.join("registry"), &late_record, &extra).unwrap();
    assert_eq!(extra.status().unwrap()["held"], true);
    drop(registration);
    std::fs::write(temp.0.join("continue-update"), "1").unwrap();
    let output = manager.wait();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    wait(|| status(&record)["receipts"].as_array().unwrap().len() == 2);
    let delivered = std::fs::read_to_string(temp.0.join("delivered.jsonl")).unwrap();
    let lines: Vec<Value> = delivered
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
    assert!(
        lines[0]["input"][0]["text"]
            .as_str()
            .unwrap()
            .ends_with("first")
    );
    assert!(
        lines[1]["input"][0]["text"]
            .as_str()
            .unwrap()
            .ends_with("second")
    );
    assert_ne!(
        lines[0]["clientUserMessageId"],
        lines[1]["clientUserMessageId"]
    );
    let report = temp.report();
    assert_eq!(report["status"], "updated");
    assert_eq!(
        report["cliUpgradeCommand"],
        "npm install -g @openai/codex@0.163.0"
    );
    assert_eq!(
        report["tuiSessionsMayNeedResume"].as_array().unwrap().len(),
        2
    );
    let notice: Value = serde_json::from_str(
        std::fs::read_to_string(temp.0.join("notices.jsonl"))
            .unwrap()
            .trim(),
    )
    .unwrap();
    assert_eq!(notice, report);
    assert!(!notice.to_string().contains("upgrade-fake-private-token"));
    assert!(!notice.to_string().contains("first"));
    assert!(!temp.0.join("registry/hold.json").exists());
}

#[test]
fn busy_timeout_releases_queue_without_updating() {
    let temp = Temp::new();
    let _server = Server::start(&temp);
    let _inbox = Inbox::start(&temp);
    let record = temp.record();
    std::fs::write(temp.0.join("busy"), "1").unwrap();
    let manager = OwnedChild(Some(temp.command().spawn().unwrap()));
    wait(|| status(&record)["held"] == true);
    send(&record, "held while busy");
    assert!(!temp.0.join("delivered.jsonl").exists());
    let output = manager.wait();
    assert!(output.status.success());
    assert!(!temp.0.join("update-requested").exists());
    wait(|| status(&record)["receipts"].as_array().unwrap().len() == 1);
    assert_eq!(temp.report()["status"], "skippedBusy");
}

#[test]
fn failed_update_and_incomplete_status_never_lose_the_hold_release() {
    for marker in [
        "fail-update",
        "invalid-history",
        "wait-update",
        "unready",
        "unavailable",
    ] {
        let temp = Temp::new();
        let _server = Server::start(&temp);
        let _inbox = Inbox::start(&temp);
        let record = temp.record();
        std::fs::write(temp.0.join(marker), "1").unwrap();
        if marker == "unready" {
            std::fs::copy(temp.0.join("before.json"), temp.0.join("after.json")).unwrap();
        }
        if marker == "unavailable" {
            std::fs::write(temp.0.join("after.json"), r#"{"status":"stopped"}"#).unwrap();
        }
        let output = temp.command().output().unwrap();
        if marker == "invalid-history" {
            assert!(output.status.success());
            assert!(!temp.0.join("update-requested").exists());
        } else {
            assert!(!output.status.success());
        }
        assert_eq!(status(&record)["holdRequested"], false);
        assert!(!temp.0.join("registry/hold.json").exists());
        assert!(
            temp.report()["releaseFailed"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        if marker == "unavailable" {
            assert!(temp.report()["cliUpgradeCommand"].is_null());
        }
        send(&record, "after failed upgrade");
        wait(|| status(&record)["receipts"].as_array().unwrap().len() == 1);
    }
}

#[test]
fn prerelease_metadata_and_a_live_independent_updater_prevent_any_hold() {
    let temp = Temp::new();
    for value in [
        json!({"version":"0.163.0-beta"}),
        json!({"version":"0.163.0","prerelease":true}),
        json!({"version":"0.163.0","draft":true}),
    ] {
        std::fs::write(temp.0.join("latest.json"), value.to_string()).unwrap();
        assert!(!temp.command().output().unwrap().status.success());
        assert!(!temp.0.join("registry/hold.json").exists());
        assert!(!temp.0.join("update-requested").exists());
    }
    std::fs::write(
        temp.0.join("daemon-updater.pid"),
        json!({"pid":std::process::id()}).to_string(),
    )
    .unwrap();
    let output = temp.command().output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("updater PID is still live"));
}

#[test]
fn crashed_manager_lock_and_hold_expire_and_stale_registry_is_pruned() {
    let temp = Temp::new();
    let _server = Server::start(&temp);
    let inbox = Inbox::start(&temp);
    let record = temp.record();
    let lease = state::id().unwrap();
    let expires = chrono::Utc::now().timestamp() as u64 + 2;
    assert_eq!(
        control(
            &record,
            "hold",
            json!({"instance":record.instance,"lease":lease,"expires":expires})
        )
        .0,
        200
    );
    send(&record, "expires safely");
    assert_eq!(status(&record)["held"], true);
    wait(|| status(&record)["receipts"].as_array().unwrap().len() == 1);
    // Real inbox was killed without Drop: flock, rather than a PID, identifies stale state.
    drop(inbox);
    {
        let _barrier = codex_registry::barrier(&temp.0.join("registry")).unwrap();
        assert!(
            codex_registry::discover(&temp.0.join("registry"))
                .unwrap()
                .is_empty()
        );
    }
    std::fs::write(temp.0.join("busy"), "1").unwrap();
    let manager = OwnedChild(Some(temp.command().spawn().unwrap()));
    wait(|| temp.0.join("registry/hold.json").exists());
    let other = temp.command().output().unwrap();
    assert!(!other.status.success());
    assert!(String::from_utf8_lossy(&other.stderr).contains("another Codex upgrade"));
    drop(manager);
    // Kernel lock is released after a crash, and the next run replaces its own lease.
    std::fs::remove_file(temp.0.join("busy")).unwrap();
    let output = temp.command().output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn restarted_inbox_preserves_manual_hold_and_explicit_release_wakes_it() {
    let temp = Temp::new();
    let _server = Server::start(&temp);
    let inbox = Inbox::start(&temp);
    let record = temp.record();
    let lease = state::id().unwrap();
    assert_eq!(control(&record,"hold",json!({"instance":record.instance,"lease":lease,"expires":chrono::Utc::now().timestamp() as u64 + 120})).0, 200);
    send(&record, "held across restart");
    drop(inbox);
    let _inbox = Inbox::start(&temp);
    let record = temp.record();
    assert_eq!(status(&record)["held"], true);
    std::thread::sleep(Duration::from_millis(200));
    assert!(!temp.0.join("delivered.jsonl").exists());
    let output = Command::new(env!("CARGO_BIN_EXE_agent-inbox"))
        .args(["codex-queue", &record.address, "--token-file"])
        .arg(&record.token_file)
        .args(["--release-hold", &lease])
        .output()
        .unwrap();
    assert!(output.status.success());
    wait(|| status(&record)["receipts"].as_array().unwrap().len() == 1);
}

#[test]
fn next_manager_reclaims_crashed_manager_lease_but_preserves_manual_hold() {
    let temp = Temp::new();
    let _server = Server::start(&temp);
    let _inbox = Inbox::start(&temp);
    let record = temp.record();
    std::fs::write(temp.0.join("busy"), "1").unwrap();
    let manager = OwnedChild(Some(temp.command().spawn().unwrap()));
    wait(|| status(&record)["held"] == true);
    let original: codex_registry::Gate =
        state::read_json(&temp.0.join("registry/hold.json")).unwrap();
    drop(manager);
    send(&record, "survives manager crash");
    let manual = state::id().unwrap();
    assert_eq!(control(&record,"hold",json!({"instance":record.instance,"lease":manual,"expires":chrono::Utc::now().timestamp() as u64 + 120})).0, 200);
    std::fs::remove_file(temp.0.join("busy")).unwrap();
    let output = temp.command().output().unwrap();
    assert!(output.status.success());
    assert_eq!(temp.report()["lease"], original.lease);
    let after = status(&record);
    assert!(after["holds"].get(&manual).is_some());
    assert!(after["holds"].get(&original.lease).is_none());
    assert!(!temp.0.join("delivered.jsonl").exists());
    assert_eq!(
        control(
            &record,
            "release",
            json!({"instance":record.instance,"lease":manual})
        )
        .0,
        200
    );
    wait(|| status(&record)["receipts"].as_array().unwrap().len() == 1);
}

#[test]
fn inbox_down_during_release_clears_only_maintenance_hold_on_later_startup() {
    let temp = Temp::new();
    let _server = Server::start(&temp);
    let inbox = Inbox::start(&temp);
    let record = temp.record();
    std::fs::write(temp.0.join("wait-update"), "1").unwrap();
    let manager = OwnedChild(Some(temp.command().spawn().unwrap()));
    wait(|| temp.0.join("update-requested").exists());
    send(&record, "held while inbox exits");
    drop(inbox);
    std::fs::write(temp.0.join("continue-update"), "1").unwrap();
    let output = manager.wait();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        temp.report()["releaseFailed"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let _inbox = Inbox::start(&temp);
    let record = temp.record();
    wait(|| status(&record)["receipts"].as_array().unwrap().len() == 1);
    assert_eq!(status(&record)["holdRequested"], false);
}
