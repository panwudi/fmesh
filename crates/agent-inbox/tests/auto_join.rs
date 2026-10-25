#![cfg(unix)]
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{
    Arc, Mutex, OnceLock,
    atomic::{AtomicU64, Ordering},
    mpsc,
};
use std::time::{Duration, Instant};

use agent_inbox::{http, local_control, state, token::Token};
use serde_json::{Value, json};
static NEXT: AtomicU64 = AtomicU64::new(0);
const HOST_TOKEN: &str = "own-host-control-test-private-token-000000";
const NATIVE_TOKEN: &str = "fake-claude-own-child-token-000000000";
static FAKE_CLAUDE: OnceLock<PathBuf> = OnceLock::new();

extern "C" fn cleanup_claude_fixture() {
    if let Some(path) = FAKE_CLAUDE.get() {
        let _ = std::fs::remove_file(path);
    }
}

fn fake_claude() -> PathBuf {
    FAKE_CLAUDE
        .get_or_init(|| {
            let path = std::env::current_exe()
                .unwrap()
                .parent()
                .unwrap()
                .join(format!("fake-claude-{}", std::process::id()));
            let output = Command::new(std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()))
                .arg("--edition=2024")
                .arg(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/support/fake_claude.rs"))
                .arg("-o")
                .arg(&path)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                Command::new(&path)
                    .arg("--version")
                    .status()
                    .unwrap()
                    .success()
            );
            unsafe {
                libc::atexit(cleanup_claude_fixture);
            }
            path
        })
        .clone()
}

struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let root = PathBuf::from(format!(
            "/tmp/ai-join-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(root.join("sample-repo")).unwrap();
        std::fs::write(root.join("host.token"), HOST_TOKEN).unwrap();
        std::fs::set_permissions(
            root.join("host.token"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        Self(root)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn wait(mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !check() {
        assert!(Instant::now() < deadline, "condition timed out");
        std::thread::sleep(Duration::from_millis(30));
    }
}
fn stop(child: &mut Child, signal: libc::c_int) {
    if child.try_wait().unwrap().is_some() {
        return;
    }
    unsafe {
        libc::kill(child.id() as libc::pid_t, signal);
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
struct Daemon {
    child: Child,
    address: String,
    errors: Arc<Mutex<String>>,
}
impl Daemon {
    fn start(args: Vec<String>) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_agent-inbox"))
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stderr = child.stderr.take().unwrap();
        let errors = Arc::new(Mutex::new(String::new()));
        let capture = Arc::clone(&errors);
        let (sent, ready) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                capture.lock().unwrap().push_str(&format!("{line}\n"));
                if let Some(address) = line.strip_prefix("agent-inbox: listening on http://") {
                    let _ = sent.send(address.trim_end_matches('/').to_owned());
                }
            }
        });
        let address = ready
            .recv_timeout(Duration::from_secs(5))
            .unwrap_or_else(|_| {
                stop(&mut child, libc::SIGKILL);
                panic!("fake deployment did not start");
            });
        Self {
            child,
            address,
            errors,
        }
    }
}
impl Drop for Daemon {
    fn drop(&mut self) {
        stop(&mut self.child, libc::SIGTERM);
    }
}

struct Setup {
    relay: Option<Daemon>,
    hub: Daemon,
    temp: Temp,
    relay_address: String,
}
impl Setup {
    fn new(start: bool) -> Self {
        let temp = Temp::new();
        let reserved = TcpListener::bind("127.0.0.1:0").unwrap();
        let relay_address = reserved.local_addr().unwrap().to_string();
        std::fs::write(temp.0.join("hub.toml"),format!("journal='hub.md'\n[[hosts]]\nname='testhost'\nrelay='{relay_address}'\ntoken_file='host.token'\n")).unwrap();
        let hub = Daemon::start(vec![
            "hub".into(),
            "--listen".into(),
            "0".into(),
            "--config".into(),
            temp.0.join("hub.toml").display().to_string(),
        ]);
        std::fs::write(temp.0.join("relay.toml"),format!("host='testhost'\nhub='{}'\nhost_token_file='host.token'\njournal='relay.md'\ncontrol_socket='control/relay.sock'\nsession_ttl_secs=3\ndirectory_report_secs=1\n",hub.address)).unwrap();
        drop(reserved);
        let mut setup = Self {
            relay: None,
            hub,
            temp,
            relay_address,
        };
        if start {
            setup.start();
        }
        setup
    }
    fn start(&mut self) {
        self.relay = Some(Daemon::start(vec![
            "relay".into(),
            "--listen".into(),
            self.relay_address.clone(),
            "--config".into(),
            self.temp.0.join("relay.toml").display().to_string(),
            "--state-dir".into(),
            self.temp.0.join("state").display().to_string(),
            "--no-tunnel".into(),
        ]));
    }
    fn control(&self, value: Value) -> Value {
        local_control::request(&self.temp.0.join("control/relay.sock"), &value).unwrap()
    }
    fn directory(&self) -> Value {
        let token = Token::read(&self.temp.0.join("host.token")).unwrap();
        let (status, body) = http::post_response_from(
            &self.hub.address,
            "/directory/list",
            &token,
            Some("testhost.relay"),
            "{}",
            Duration::from_secs(2),
        )
        .unwrap();
        assert_eq!(status, 200);
        serde_json::from_str(&body).unwrap()
    }
    fn contains(&self, name: &str) -> bool {
        self.directory()["agents"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["name"] == name)
    }
    fn send(&self, name: &str, text: &str) -> u16 {
        http::post(
            &self.relay_address,
            &format!("/to/{name}"),
            &Token::read(&self.temp.0.join("host.token")).unwrap(),
            Some("remote.reviewer"),
            text,
            Duration::from_secs(2),
        )
        .unwrap()
    }
}

struct Inbox {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: mpsc::Receiver<Value>,
    output: Arc<Mutex<String>>,
    errors: Arc<Mutex<String>>,
    address: String,
    credentials: PathBuf,
    next: u64,
}
impl Inbox {
    fn start(setup: &Setup) -> Self {
        Self::start_named(setup, None)
    }
    fn start_named(setup: &Setup, name: Option<&str>) -> Self {
        Self::start_options(setup, name, None)
    }
    fn start_options(
        setup: &Setup,
        name: Option<&str>,
        runtime: Option<(&str, &std::path::Path, bool)>,
    ) -> Self {
        let parent = setup.temp.0.join("sessions");
        let before: Vec<_> = std::fs::read_dir(&parent)
            .into_iter()
            .flatten()
            .map(|e| e.unwrap().path())
            .collect();
        let mut command = if let Some((id, root, native)) = runtime {
            let mut command = Command::new(fake_claude());
            command
                .arg(env!("CARGO_BIN_EXE_agent-inbox"))
                .env("AGENT_INBOX_FAKE_CLAUDE_ROOT", root)
                .env("CLAUDE_CODE_SESSION_ID", id)
                .env("CLAUDE_CONFIG_DIR", root)
                .env("CLAUDE_PROJECT_DIR", setup.temp.0.join("sample-repo"))
                .env("CLAUDE_CODE_MESSAGING_TOKEN", NATIVE_TOKEN);
            if !native {
                command.env("AGENT_INBOX_FAKE_DISABLE_NATIVE", "1");
            }
            command
        } else {
            let mut command = Command::new(env!("CARGO_BIN_EXE_agent-inbox"));
            command.env_remove("CLAUDE_CODE_SESSION_ID");
            command
        };
        command
            .args([
                "claude",
                "--relay",
                &setup.relay_address,
                "--control-socket",
            ])
            .arg(setup.temp.0.join("control/relay.sock"))
            .arg("--session-dir")
            .arg(&parent)
            .arg("--log")
            .arg(setup.temp.0.join("messages.md"))
            .env(
                "AGENT_INBOX_TOKEN_FILE",
                setup.temp.0.join("must-not-read-global-token"),
            )
            .current_dir(setup.temp.0.join("sample-repo"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(name) = name {
            command.args(["--name", name]);
        }
        let mut child = command.spawn().unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let output = Arc::new(Mutex::new(String::new()));
        let errors = Arc::new(Mutex::new(String::new()));
        let (sent, lines) = mpsc::channel();
        let capture = Arc::clone(&output);
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                capture.lock().unwrap().push_str(&format!("{line}\n"));
                let _ = sent.send(serde_json::from_str(&line).unwrap());
            }
        });
        let (listening, ready) = mpsc::channel();
        let capture = Arc::clone(&errors);
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                capture.lock().unwrap().push_str(&format!("{line}\n"));
                if let Some(address) = line.strip_prefix("agent-inbox: listening on http://") {
                    let _ = listening.send(address.trim_end_matches('/').to_owned());
                }
            }
        });
        let address = ready
            .recv_timeout(Duration::from_secs(5))
            .unwrap_or_else(|_| {
                stop(&mut child, libc::SIGKILL);
                panic!("auto inbox did not start");
            });
        let credentials = std::fs::read_dir(&parent)
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| {
                p.is_dir()
                    && p.file_name()
                        .unwrap()
                        .to_string_lossy()
                        .starts_with("session-")
                    && !before.contains(p)
            })
            .unwrap();
        let mut inbox = Self {
            child,
            stdin: Some(stdin),
            lines,
            output,
            errors,
            address,
            credentials,
            next: 1,
        };
        let response = inbox.rpc("initialize", json!({"protocolVersion":"2025-06-18"}));
        let instructions = response["result"]["instructions"].as_str().unwrap();
        assert!(instructions.contains("inbox_profile"));
        assert!(instructions.contains("server:agent-inbox-join"));
        inbox.write(json!({"jsonrpc":"2.0","method":"notifications/initialized"}));
        inbox
    }
    fn write(&mut self, value: Value) {
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{value}").unwrap();
        stdin.flush().unwrap();
    }
    fn rpc(&mut self, method: &str, params: Value) -> Value {
        let id = self.next;
        self.next += 1;
        self.write(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}));
        loop {
            let value = self.lines.recv_timeout(Duration::from_secs(3)).unwrap();
            if value["id"] == id {
                return value;
            }
        }
    }
    fn profile(&mut self, patch: Value) -> Value {
        let response = self.rpc(
            "tools/call",
            json!({"name":"inbox_profile","arguments":patch}),
        );
        assert_eq!(response["result"]["isError"], false);
        response["result"]["structuredContent"].clone()
    }
    fn message(&self, suffix: &str) -> Value {
        loop {
            let value = self.lines.recv_timeout(Duration::from_secs(3)).unwrap();
            if value["params"]["content"]
                .as_str()
                .is_some_and(|s| s.ends_with(suffix))
            {
                return value;
            }
        }
    }
    fn close(&mut self) {
        self.stdin.take();
        let deadline = Instant::now() + Duration::from_secs(2);
        while self.child.try_wait().unwrap().is_none() {
            assert!(Instant::now() < deadline, "graceful shutdown timed out");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!self.credentials.exists());
    }
    fn secret_free(&self, setup: &Setup, secrets: &[String]) {
        let output = self.output.lock().unwrap();
        let errors = self.errors.lock().unwrap();
        for secret in secrets {
            assert!(!output.contains(secret));
            assert!(!errors.contains(secret));
            assert!(!setup.hub.errors.lock().unwrap().contains(secret));
            if let Some(relay) = &setup.relay {
                assert!(!relay.errors.lock().unwrap().contains(secret));
            }
            for file in ["messages.md", "relay.md", "hub.directory.json"] {
                assert!(
                    !std::fs::read_to_string(setup.temp.0.join(file))
                        .unwrap_or_default()
                        .contains(secret)
                );
            }
        }
    }
}
impl Drop for Inbox {
    fn drop(&mut self) {
        stop(&mut self.child, libc::SIGTERM);
    }
}

#[test]
fn auto_join_uses_cwd_ephemeral_ports_fresh_credentials_and_live_channel() {
    let setup = Setup::new(true);
    let mut first = Inbox::start(&setup);
    let mut second = Inbox::start(&setup);
    assert_ne!(first.address, second.address);
    assert_eq!(
        first.profile(json!({}))["name"],
        "testhost.sample-repo-claude"
    );
    assert_eq!(
        second.profile(json!({}))["name"],
        "testhost.sample-repo-claude-2"
    );
    let tools = first.rpc("tools/list", json!({}));
    assert_eq!(tools["result"]["tools"][0]["name"], "inbox_profile");
    let mut secrets = vec![HOST_TOKEN.to_owned()];
    for session in [&first, &second] {
        assert_eq!(
            std::fs::metadata(&session.credentials)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        for file in ["inbox.token", "sender.token"] {
            let path = session.credentials.join(file);
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            let secret = std::fs::read_to_string(path).unwrap().trim().to_owned();
            assert_eq!(secret.len(), 64);
            assert!(!secrets.contains(&secret));
            secrets.push(secret);
        }
    }
    wait(|| setup.contains("testhost.sample-repo-claude-2"));
    assert_eq!(setup.send("testhost.sample-repo-claude", "hello live"), 200);
    assert_eq!(
        first.message("hello live")["params"]["meta"]["from"],
        "remote.reviewer"
    );
    let token = Token::read(&first.credentials.join("sender.token")).unwrap();
    assert_eq!(
        http::post_response(&setup.relay_address, "/directory/list", &token, "{}")
            .unwrap()
            .0,
        200
    );
    first.secret_free(&setup, &secrets);
    second.secret_free(&setup, &secrets);
    first.close();
    second.close();
    wait(|| !setup.contains("testhost.sample-repo-claude"));
}

#[test]
fn same_user_directory_is_read_only_and_never_exports_credentials() {
    let setup = Setup::new(true);
    let mut inbox = Inbox::start(&setup);
    inbox.profile(json!({"description":"Maintains this project"}));
    let page = setup.control(json!({"op":"directory"}));
    assert_eq!(page["agents"].as_array().unwrap().len(), 1);
    assert_eq!(page["agents"][0]["name"], "testhost.sample-repo-claude");
    assert_eq!(page["agents"][0]["description"], "Maintains this project");
    assert_eq!(page["agents"][0]["status"], "online");
    let text = page.to_string();
    for file in ["inbox.token", "sender.token"] {
        let secret = std::fs::read_to_string(inbox.credentials.join(file)).unwrap();
        assert!(!text.contains(secret.trim()));
    }
    assert!(!text.contains(HOST_TOKEN));
    assert!(!text.contains(&inbox.address));
    assert_eq!(
        setup.control(json!({"op":"directory","name":"takeover"}))["status"],
        "invalid"
    );
    let output = Command::new(env!("CARGO_BIN_EXE_agent-inbox"))
        .args(["who", "--local", "--json", "--control-socket"])
        .arg(setup.temp.0.join("control/relay.sock"))
        .env_remove("AGENT_INBOX_TOKEN_FILE")
        .output()
        .unwrap();
    assert!(output.status.success());
    let rows: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(rows.as_array().unwrap().len(), 1);
    assert_eq!(rows[0]["name"], page["agents"][0]["name"]);
    assert_eq!(rows[0]["description"], "Maintains this project");
    assert_eq!(
        inbox.profile(json!({}))["name"],
        "testhost.sample-repo-claude"
    );
}

#[test]
fn explicit_name_is_kept_without_a_type_tag_and_accepts_the_full_component_limit() {
    let setup = Setup::new(true);
    let mut inbox = Inbox::start_named(&setup, Some("operator-chosen"));
    assert_eq!(inbox.profile(json!({}))["name"], "testhost.operator-chosen");
    let long = "a".repeat(63);
    let mut long_inbox = Inbox::start_named(&setup, Some(&long));
    assert_eq!(
        long_inbox.profile(json!({}))["name"],
        format!("testhost.{long}")
    );
}

#[test]
fn own_profile_updates_names_descriptions_routing_and_hub_directory() {
    let setup = Setup::new(true);
    let mut first = Inbox::start(&setup);
    let mut second = Inbox::start(&setup);
    let renamed =
        first.profile(json!({"name":"sample-repo-claude-2","description":"Owns relay reviews"}));
    assert_eq!(renamed["name"], "testhost.sample-repo-claude-2-2");
    wait(|| {
        setup.directory()["agents"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| {
                r["name"] == "testhost.sample-repo-claude-2-2"
                    && r["description"] == "Owns relay reviews"
            })
    });
    assert_eq!(setup.send("testhost.sample-repo-claude", "old name"), 404);
    assert_eq!(
        http::post(
            &setup.relay_address,
            "/to/testhost.sample-repo-claude-2",
            &Token::read(&first.credentials.join("sender.token")).unwrap(),
            Some("forged.other"),
            "sender identity follows rename",
            Duration::from_secs(2)
        )
        .unwrap(),
        200
    );
    assert_eq!(
        second.message("sender identity follows rename")["params"]["meta"]["from"],
        "testhost.sample-repo-claude-2-2"
    );
    let rejected = first.rpc(
        "tools/call",
        json!({"name":"inbox_profile","arguments":{"name":"other.agent"}}),
    );
    assert_eq!(rejected["result"]["isError"], true);
    let rejected = first.rpc(
        "tools/call",
        json!({"name":"inbox_profile","arguments":{"id":"another session","name":"takeover"}}),
    );
    assert_eq!(rejected["result"]["isError"], true);
    assert_eq!(
        second.profile(json!({}))["name"],
        "testhost.sample-repo-claude-2"
    );
}

#[test]
fn heartbeats_keep_live_sessions_while_crashes_expire_and_credentials_are_cleaned() {
    let setup = Setup::new(true);
    let mut live = Inbox::start(&setup);
    let mut crashed = Inbox::start(&setup);
    let abandoned = crashed.credentials.clone();
    stop(&mut crashed.child, libc::SIGKILL);
    std::thread::sleep(Duration::from_millis(3600));
    assert_eq!(live.profile(json!({}))["registered"], true);
    wait(|| !setup.contains("testhost.sample-repo-claude-2"));
    assert_eq!(
        setup.send("testhost.sample-repo-claude-2", "expired route"),
        404
    );
    assert_eq!(
        http::post_response(
            &setup.relay_address,
            "/directory/list",
            &Token::read(&abandoned.join("sender.token")).unwrap(),
            "{}"
        )
        .unwrap()
        .0,
        401
    );
    let mut replacement = Inbox::start(&setup);
    assert!(!abandoned.exists());
    assert!(live.credentials.exists());
    assert_eq!(
        replacement.profile(json!({}))["name"],
        "testhost.sample-repo-claude-2"
    );
    let started = Instant::now();
    stop(&mut replacement.child, libc::SIGINT);
    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(!replacement.credentials.exists());
    wait(|| !setup.contains("testhost.sample-repo-claude-2"));
}

#[test]
fn live_inbox_rejoins_after_relay_restart_and_survives_static_reload() {
    let mut setup = Setup::new(true);
    let mut inbox = Inbox::start(&setup);
    drop(setup.relay.take());
    wait(|| inbox.profile(json!({}))["registered"] == false);
    setup.start();
    wait(|| inbox.profile(json!({}))["registered"] == true);
    assert_eq!(
        inbox.profile(json!({}))["name"],
        "testhost.sample-repo-claude"
    );
    let config = setup.temp.0.join("relay.toml");
    let text = std::fs::read_to_string(&config).unwrap();
    std::fs::write(
        &config,
        text.replace("directory_report_secs=1", "directory_report_secs=2"),
    )
    .unwrap();
    std::thread::sleep(Duration::from_millis(1300));
    assert_eq!(
        setup.send("testhost.sample-repo-claude", "after relay restart/reload"),
        200
    );
    inbox.message("after relay restart/reload");
    assert_eq!(inbox.profile(json!({}))["registered"], true);
    let output = inbox.output.lock().unwrap();
    for line in output.lines() {
        let value: Value = serde_json::from_str(line).unwrap();
        if value["method"] == "notifications/claude/channel" {
            assert_eq!(
                value["params"]["content"], "after relay restart/reload",
                "registration changes must not wake the model"
            );
        }
    }
    let journal = std::fs::read_to_string(setup.temp.0.join("messages.md")).unwrap();
    assert!(journal.contains("Local auto-join: registered as"));
    assert!(journal.contains("Local auto-join: registration unconfirmed"));
}

#[test]
fn relay_down_does_not_block_mcp_and_journaled_inbox_joins_later() {
    let mut setup = Setup::new(false);
    let mut inbox = Inbox::start(&setup);
    assert_eq!(inbox.profile(json!({}))["registered"], false);
    assert_eq!(
        http::send(
            &inbox.address,
            &Token::read(&inbox.credentials.join("inbox.token")).unwrap(),
            "teammate",
            "saved before relay starts"
        )
        .unwrap(),
        200
    );
    inbox.message("saved before relay starts");
    setup.start();
    wait(|| inbox.profile(json!({}))["registered"] == true);
    wait(|| setup.contains("testhost.sample-repo-claude"));
    assert!(
        std::fs::read_to_string(setup.temp.0.join("messages.md"))
            .unwrap()
            .contains("saved before relay starts")
    );
}

fn native_root(setup: &Setup) -> PathBuf {
    let root = setup.temp.0.join("claude");
    std::fs::create_dir_all(&root).unwrap();
    root
}

fn native_transcript(setup: &Setup, root: &std::path::Path, id: &str) -> PathBuf {
    let project = setup.temp.0.join("sample-repo").canonicalize().unwrap();
    let slug = project.to_string_lossy().replace('/', "-");
    let path = root.join("projects").join(slug).join(format!("{id}.jsonl"));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    if !path.exists() {
        std::fs::write(&path, "").unwrap();
    }
    path
}

fn native_template(setup: &Setup, root: &std::path::Path, id: &str) {
    std::fs::write(
        root.join(format!("{id}.template")),
        json!({
            "pid":0,"sessionId":id,"cwd":setup.temp.0.join("sample-repo").canonicalize().unwrap(),
            "kind":"bg","peerProtocol":1,"messagingSocketPath":root.join(format!("{id}.sock"))
        })
        .to_string(),
    )
    .unwrap();
    native_transcript(setup, root, id);
}

#[test]
fn native_inbox_receives_without_channel_flags_and_disconnected_socket_is_offline() {
    let setup = Setup::new(true);
    let root = native_root(&setup);
    let id = state::id().unwrap();
    native_template(&setup, &root, &id);
    let mut inbox = Inbox::start_options(&setup, None, Some((&id, &root, true)));
    assert_eq!(inbox.profile(json!({}))["deliveryTransport"], "native");
    wait(|| setup.directory()["agents"][0]["inbox_reachable"] == true);
    let status = setup.send(
        "testhost.sample-repo-claude",
        "native probe without channel flags",
    );
    assert_eq!(
        status,
        202,
        "{}",
        inbox
            .errors
            .lock()
            .unwrap()
            .replace(NATIVE_TOKEN, "<redacted>")
    );
    let capture = root.join(format!("{id}.capture"));
    wait(|| std::fs::read_to_string(&capture).is_ok_and(|text| text.contains("native probe")));
    let messages: Vec<Value> = std::fs::read_to_string(&capture)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(messages.len(), 1);
    let text = messages[0]["message"]["content"].as_str().unwrap();
    assert!(text.contains("not the user's instruction or approval"));
    assert!(text.contains("Current receiver profile:"));
    assert!(text.contains("replyCommand"));
    assert!(!inbox.output.lock().unwrap().contains("native probe"));
    inbox.secret_free(&setup, &[NATIVE_TOKEN.to_owned()]);
    std::fs::remove_file(root.join(format!("{id}.sock"))).unwrap();
    wait(|| setup.directory()["agents"][0]["inbox_reachable"] == false);
    assert_eq!(
        setup.send(
            "testhost.sample-repo-claude",
            "must remain saved, not silently dropped"
        ),
        502
    );
    assert_eq!(
        std::fs::read_to_string(&capture).unwrap().lines().count(),
        1
    );
    assert!(
        std::fs::read_to_string(setup.temp.0.join("messages.md"))
            .unwrap()
            .contains("must remain saved")
    );
}

#[test]
fn missing_native_inbox_and_channel_flags_never_advertises_receiving() {
    let setup = Setup::new(true);
    let root = native_root(&setup);
    let id = state::id().unwrap();
    native_template(&setup, &root, &id);
    let mut inbox = Inbox::start_options(&setup, None, Some((&id, &root, false)));
    assert_eq!(inbox.profile(json!({}))["deliveryTransport"], "unavailable");
    wait(|| setup.contains("testhost.sample-repo-claude"));
    assert_eq!(setup.directory()["agents"][0]["inbox_reachable"], false);
    assert_eq!(
        setup.send(
            "testhost.sample-repo-claude",
            "must not claim channel delivery"
        ),
        502
    );
    assert!(
        !inbox
            .output
            .lock()
            .unwrap()
            .contains("must not claim channel delivery")
    );
}

#[test]
fn explicit_continuation_transfers_custom_name_but_independent_sessions_stay_separate() {
    let mut setup = Setup::new(true);
    let root = native_root(&setup);
    let source = state::id().unwrap();
    let successor = state::id().unwrap();
    let independent = state::id().unwrap();
    for id in [&source, &successor, &independent] {
        native_template(&setup, &root, id);
    }
    let mut first =
        Inbox::start_options(&setup, Some("custom-owner"), Some((&source, &root, true)));
    first.profile(json!({"description":"Owns the continued work"}));
    let mut second = Inbox::start_options(&setup, None, Some((&successor, &root, true)));
    let mut third = Inbox::start_options(&setup, None, Some((&independent, &root, true)));
    let independent_name = third.profile(json!({}))["name"]
        .as_str()
        .unwrap()
        .to_owned();
    wait(|| setup.directory()["agents"].as_array().unwrap().len() == 3);
    let source_file = native_transcript(&setup, &root, &source);
    let record = json!({"type":"continued-in","sessionId":source,"continuedInSessionId":successor})
        .to_string();
    std::fs::write(&source_file, &record).unwrap();
    std::thread::sleep(Duration::from_millis(1100));
    assert_eq!(
        first.profile(json!({}))["registered"],
        true,
        "partial record must not retire a live session"
    );
    std::fs::write(&source_file, format!("{record}\n")).unwrap();
    wait(|| first.profile(json!({}))["registered"] == false);
    assert_eq!(first.profile(json!({}))["continuedTo"], successor);
    wait(|| second.profile(json!({}))["name"] == "testhost.custom-owner");
    assert_eq!(
        second.profile(json!({}))["description"],
        "Owns the continued work"
    );
    assert_eq!(third.profile(json!({}))["name"], independent_name);
    wait(|| setup.directory()["agents"].as_array().unwrap().len() == 2);
    assert_eq!(
        setup.send("testhost.custom-owner", "after explicit continuation"),
        202
    );
    wait(|| {
        std::fs::read_to_string(root.join(format!("{successor}.capture")))
            .is_ok_and(|t| t.contains("after explicit continuation"))
    });
    assert!(!root.join(format!("{source}.capture")).exists());
    drop(setup.relay.take());
    setup.start();
    wait(|| second.profile(json!({}))["registered"] == true);
    assert_eq!(first.profile(json!({}))["registered"], false);
    assert_eq!(second.profile(json!({}))["name"], "testhost.custom-owner");
    assert_eq!(third.profile(json!({}))["name"], independent_name);
    wait(|| setup.directory()["agents"].as_array().unwrap().len() == 2);
    assert_eq!(
        first.rpc(
            "tools/call",
            json!({"name":"inbox_profile","arguments":{"name":"should-not-return"}})
        )["result"]["isError"],
        true
    );
}

#[test]
fn local_control_is_idempotent_scoped_private_and_never_available_over_tcp() {
    let setup = Setup::new(true);
    let owned = TcpListener::bind("127.0.0.1:0").unwrap();
    let id = state::id().unwrap();
    let registration = json!({"op":"register","id":id,"name":"manual","description":"Test session","inbox":owned.local_addr().unwrap().to_string(),"inboxToken":"independent-inbox-secret-00000000000000","senderToken":"independent-sender-secret-0000000000000"});
    assert_eq!(
        setup.control(registration.clone())["name"],
        "testhost.manual"
    );
    assert_eq!(
        setup.control(registration.clone())["name"],
        "testhost.manual"
    );
    let mut duplicate = registration.clone();
    duplicate["id"] = json!(state::id().unwrap());
    duplicate["inboxToken"] = json!("second-inbox-secret-000000000000000000");
    duplicate["senderToken"] = json!("second-sender-secret-00000000000000000");
    assert_eq!(setup.control(duplicate)["name"], "testhost.manual-2");
    for op in ["heartbeat", "profile", "unregister"] {
        assert_eq!(
            setup.control(json!({"op":op,"id":id,"senderToken":"wrong-owner"}))["status"],
            "rejected"
        );
    }
    let mut bad = registration.clone();
    bad["id"] = json!(state::id().unwrap());
    bad["inbox"] = json!("192.0.2.1:1234");
    assert_eq!(setup.control(bad)["status"], "rejected");
    assert_eq!(
        http::post(
            &setup.relay_address,
            "/register",
            &Token::read(&setup.temp.0.join("host.token")).unwrap(),
            Some("testhost.relay"),
            &registration.to_string(),
            Duration::from_secs(1)
        )
        .unwrap(),
        404
    );
    let socket = setup.temp.0.join("control/relay.sock");
    assert_eq!(
        std::fs::metadata(&socket).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        std::fs::metadata(socket.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o666)).unwrap();
    assert!(local_control::request(&socket, &json!({"op":"heartbeat"})).is_err());
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
}

#[test]
fn auto_join_rejects_nonlocal_relay_or_fixed_inbox_configuration() {
    for args in [
        vec!["--relay", "192.0.2.1:3456"],
        vec!["--relay", "127.0.0.1:3456", "--listen", "0"],
        vec!["--relay", "127.0.0.1:3456", "--token-file", "unused"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_agent-inbox"))
            .arg("claude")
            .args(args)
            .output()
            .unwrap();
        assert!(!output.status.success());
    }
}

#[test]
fn static_names_are_reserved_and_conflicting_reload_keeps_the_live_session() {
    let mut setup = Setup::new(false);
    let owned = TcpListener::bind("127.0.0.1:0").unwrap();
    for (file, secret) in [
        ("static.in", "separate-static-inbox-token-00000000000"),
        ("static.send", "separate-static-sender-token-0000000000"),
    ] {
        std::fs::write(setup.temp.0.join(file), secret).unwrap();
        std::fs::set_permissions(
            setup.temp.0.join(file),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
    }
    let config = setup.temp.0.join("relay.toml");
    let original = std::fs::read_to_string(&config).unwrap();
    let static_row = format!(
        "\n[[agents]]\nname='sample-repo-claude'\ndescription='Static entry'\ninbox='{}'\ntoken_file='static.in'\nsender_token_file='static.send'\n",
        owned.local_addr().unwrap()
    );
    std::fs::write(&config, format!("{original}{static_row}")).unwrap();
    setup.start();
    let mut inbox = Inbox::start(&setup);
    assert_eq!(
        inbox.profile(json!({}))["name"],
        "testhost.sample-repo-claude-2"
    );
    std::fs::write(
        &config,
        format!(
            "{original}{}",
            static_row.replace("name='sample-repo-claude'", "name='sample-repo-claude-2'")
        ),
    )
    .unwrap();
    wait(|| {
        setup
            .relay
            .as_ref()
            .unwrap()
            .errors
            .lock()
            .unwrap()
            .contains("conflicts with live sessions")
    });
    assert_eq!(
        setup.send("testhost.sample-repo-claude-2", "live entry retained"),
        200
    );
    inbox.message("live entry retained");
}

#[test]
fn global_wrapper_keeps_cwd_prompt_arguments_and_the_exact_entry_name() {
    let temp = Temp::new();
    let fake = temp.0.join("fake-claude");
    std::fs::write(
        &fake,
        "#!/bin/sh\nprintf '%s\\n' \"$PWD\" \"$@\" > \"$AGENT_INBOX_TEST_CAPTURE\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o700)).unwrap();
    let wrapper =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("deploy/claude-with-inbox.bash");
    for cwd in [&temp.0, temp.0.join("sample-repo").as_path()] {
        let cases = [
            vec!["a prompt with spaces"],
            vec!["--resume", "thread with spaces", "--", "prompt 'quoted'"],
            vec![
                "--dangerously-load-development-channels",
                "server:another",
                "-p",
                "hello",
            ],
        ];
        for arguments in cases {
            let capture = temp.0.join("captured");
            let output = Command::new("bash")
                .arg(&wrapper)
                .args(&arguments)
                .current_dir(cwd)
                .env("AGENT_INBOX_CLAUDE_BIN", &fake)
                .env("AGENT_INBOX_TEST_CAPTURE", &capture)
                .output()
                .unwrap();
            assert!(output.status.success());
            let lines = std::fs::read_to_string(&capture).unwrap();
            let lines: Vec<_> = lines.lines().collect();
            assert_eq!(
                PathBuf::from(lines[0]).canonicalize().unwrap(),
                cwd.canonicalize().unwrap()
            );
            assert!(lines.windows(2).any(|pair| pair
                == [
                    "--dangerously-load-development-channels",
                    "server:agent-inbox-join"
                ]));
            for argument in arguments {
                assert!(lines.contains(&argument));
            }
            if lines.contains(&"a prompt with spaces") {
                assert!(
                    lines.iter().position(|s| *s == "a prompt with spaces")
                        < lines
                            .iter()
                            .position(|s| *s == "--dangerously-load-development-channels")
                );
            }
        }
    }
    let entry: Value = serde_json::from_str(
        &std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("deploy/claude-user-mcp.json.example"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(entry["mcpServers"]["agent-inbox-join"]["args"][0], "claude");
    assert_eq!(
        entry["mcpServers"]["agent-inbox-join"]["args"][1],
        "--relay"
    );
}
