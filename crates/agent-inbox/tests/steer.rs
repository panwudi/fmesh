#![cfg(unix)]

use std::io;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::{Value, json};
use tungstenite::{Message, WebSocket};

static NEXT: AtomicU64 = AtomicU64::new(0);
const THREAD: &str = "test-thread";

mod support;
use support::fake_codex;

struct Temp(PathBuf);

impl Temp {
    fn new() -> Self {
        let path = PathBuf::from(format!(
            "/tmp/agent-inbox-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn command(temp: &Temp) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_agent-inbox"));
    command
        .args([
            "steer",
            "--thread",
            THREAD,
            "--from",
            "claude",
            "--codex-bin",
        ])
        .arg(fake_codex())
        .arg("--sock")
        .arg(temp.0.join("server.sock"))
        .arg("--log")
        .arg(temp.0.join("messages.md"));
    command
}

fn receive(socket: &mut WebSocket<UnixStream>, method: &str) -> Value {
    let Message::Text(text) = socket.read().unwrap() else {
        panic!("expected text request");
    };
    let request: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(request["method"], method);
    assert!(
        request.get("jsonrpc").is_none(),
        "Codex wire protocol omits jsonrpc"
    );
    request
}

fn reply(socket: &mut WebSocket<UnixStream>, request: &Value, result: Value) {
    socket
        .send(Message::Text(
            json!({"id": request["id"], "result": result})
                .to_string()
                .into(),
        ))
        .unwrap();
}

fn handshake(stream: UnixStream) -> WebSocket<UnixStream> {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut socket = tungstenite::accept(stream).unwrap();
    let request = receive(&mut socket, "initialize");
    assert_eq!(request["params"]["clientInfo"]["name"], "agent_inbox");
    assert_eq!(request["params"]["capabilities"]["experimentalApi"], true);
    reply(
        &mut socket,
        &request,
        json!({
            "userAgent": "agent_inbox/0.162.1", "platformFamily": "unix", "platformOs": "macos", "codexHome": "/fake"
        }),
    );
    let initialized = receive(&mut socket, "initialized");
    assert!(initialized.get("id").is_none());
    socket
}

fn read_state(socket: &mut WebSocket<UnixStream>, status: &str) {
    let request = receive(socket, "thread/read");
    assert_eq!(request["params"], json!({"threadId": THREAD}));
    reply(
        socket,
        &request,
        json!({"thread": {"id": THREAD, "status": {"type": status}, "turns": []}}),
    );
}

fn active_turn(socket: &mut WebSocket<UnixStream>, status: &str) {
    let request = receive(socket, "thread/turns/list");
    assert_eq!(
        request["params"],
        json!({
            "threadId": THREAD, "limit": 1, "sortDirection": "desc", "itemsView": "notLoaded"
        })
    );
    reply(
        socket,
        &request,
        json!({"data": [{"id": "active-turn", "status": status, "items": []}], "nextCursor": null}),
    );
}

fn with_server(
    test: impl FnOnce(&mut WebSocket<UnixStream>) + Send + 'static,
    timeout: Option<u64>,
) -> (Output, String) {
    let temp = Temp::new();
    let listener = UnixListener::bind(temp.0.join("server.sock")).unwrap();
    let server = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut socket = handshake(stream);
        test(&mut socket);
    });
    let mut command = command(&temp);
    if let Some(seconds) = timeout {
        command.arg("--timeout-secs").arg(seconds.to_string());
    }
    let output = command
        .arg("Please check the failing test.\nUnicode: 中文")
        .output()
        .unwrap();
    server.join().unwrap();
    let log = std::fs::read_to_string(temp.0.join("messages.md")).unwrap();
    (output, log)
}

fn check_input(request: &Value) {
    assert_eq!(request["params"]["threadId"], THREAD);
    let text = request["params"]["input"][0]["text"].as_str().unwrap();
    assert!(text.contains("from: claude"));
    assert!(text.contains("not the user's instruction or approval"));
    assert!(text.ends_with("Please check the failing test.\nUnicode: 中文"));
}

#[test]
fn steers_active_turn_over_proxy_stdio_and_handles_notifications() {
    let (output, log) = with_server(
        |socket| {
            read_state(socket, "active");
            active_turn(socket, "inProgress");
            let request = receive(socket, "turn/steer");
            check_input(&request);
            assert_eq!(request["params"]["expectedTurnId"], "active-turn");
            socket.send(Message::Ping(vec![1, 2].into())).unwrap();
            socket
                .send(Message::Text(
                    json!({"method": "thread/status/changed", "params": {"threadId": THREAD}})
                        .to_string()
                        .into(),
                ))
                .unwrap();
            socket.send(Message::Text(json!({"id": "server-request-1", "method": "item/tool/requestUserInput", "params": {}}).to_string().into())).unwrap();
            reply(socket, &request, json!({"turnId": "active-turn"}));
            // Keep the connection open until the client flushes its automatic pong.
            assert_eq!(socket.read().unwrap(), Message::Pong(vec![1, 2].into()));
        },
        None,
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("Accepted via turn/steer"));
    assert!(log.contains("From: claude, To: test-thread, Time:"));
    assert!(log.contains("Unicode: 中文"));
}

#[test]
fn starts_idle_thread() {
    let (output, _) = with_server(
        |socket| {
            read_state(socket, "idle");
            let request = receive(socket, "turn/start");
            check_input(&request);
            assert!(request["params"].get("expectedTurnId").is_none());
            reply(
                socket,
                &request,
                json!({"turn": {"id": "new-turn", "status": "inProgress", "items": []}}),
            );
        },
        None,
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("Accepted via turn/start"));
}

#[test]
fn resumes_unloaded_thread_without_overriding_configuration() {
    let (output, _) = with_server(
        |socket| {
            read_state(socket, "notLoaded");
            let request = receive(socket, "thread/resume");
            assert_eq!(
                request["params"],
                json!({"threadId": THREAD, "excludeTurns": true})
            );
            reply(
                socket,
                &request,
                json!({"thread": {"id": THREAD, "status": {"type": "idle"}}}),
            );
            let request = receive(socket, "turn/start");
            reply(socket, &request, json!({"turn": {"id": "new-turn"}}));
        },
        None,
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn stale_turn_precondition_is_reported_and_message_is_kept() {
    let (output, log) = with_server(
        |socket| {
            read_state(socket, "active");
            active_turn(socket, "inProgress");
            let request = receive(socket, "turn/steer");
            socket.send(Message::Text(json!({"id": request["id"], "error": {"code": -32600, "message": "expectedTurnId does not match"}}).to_string().into())).unwrap();
        },
        None,
    );
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("expectedTurnId does not match"));
    assert!(String::from_utf8_lossy(&output.stderr).contains("Message saved"));
    assert!(log.contains("Please check the failing test."));
}

#[test]
fn rereads_and_starts_when_active_turn_finishes_during_lookup() {
    let (output, _) = with_server(
        |socket| {
            read_state(socket, "active");
            active_turn(socket, "completed");
            read_state(socket, "idle");
            let request = receive(socket, "turn/start");
            reply(socket, &request, json!({"turn": {"id": "new-turn"}}));
        },
        None,
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("turn/start"));
}

#[test]
fn retries_idle_to_active_race_with_steer() {
    let (output, _) = with_server(
        |socket| {
            read_state(socket, "idle");
            let request = receive(socket, "turn/start");
            socket.send(Message::Text(json!({"id": request["id"], "error": {"code": -32600, "message": "thread already has an active or pending turn"}}).to_string().into())).unwrap();
            read_state(socket, "active");
            active_turn(socket, "inProgress");
            let request = receive(socket, "turn/steer");
            reply(socket, &request, json!({"turnId": "active-turn"}));
        },
        None,
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("turn/steer"));
}

#[test]
fn retries_rejected_steer_after_turn_finishes() {
    let (output, _) = with_server(
        |socket| {
            read_state(socket, "active");
            active_turn(socket, "inProgress");
            let request = receive(socket, "turn/steer");
            socket.send(Message::Text(json!({"id": request["id"], "error": {"code": -32600, "message": "no active turn to steer"}}).to_string().into())).unwrap();
            read_state(socket, "idle");
            let request = receive(socket, "turn/start");
            reply(socket, &request, json!({"turn": {"id": "new-turn"}}));
        },
        None,
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn stops_after_three_safe_race_retries_and_logs_once() {
    let (output, log) = with_server(
        |socket| {
            for _ in 0..4 {
                read_state(socket, "active");
                let request = receive(socket, "thread/turns/list");
                reply(socket, &request, json!({"data": []}));
            }
            assert!(socket.read().is_err());
        },
        None,
    );
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("after 3 retries"));
    assert_eq!(log.matches("## From:").count(), 1);
}

#[test]
fn journal_uses_readable_local_time() {
    let temp = Temp::new();
    let output = command(&temp)
        .env("TZ", "Asia/Shanghai")
        .arg("hello")
        .output()
        .unwrap();
    assert!(!output.status.success());
    let log = std::fs::read_to_string(temp.0.join("messages.md")).unwrap();
    let stamp = log.split("Time: ").nth(1).unwrap().lines().next().unwrap();
    assert!(chrono::NaiveDateTime::parse_from_str(&stamp[..16], "%Y-%m-%d %H:%M").is_ok());
    assert!(stamp.ends_with("+08:00"));
    assert!(!stamp.contains("Unix seconds"));
}

#[test]
fn times_out_when_server_does_not_reply() {
    let (output, log) = with_server(
        |socket| {
            receive(socket, "thread/read");
            std::thread::sleep(Duration::from_millis(1300));
        },
        Some(1),
    );
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("timed out"));
    assert!(log.contains("Please check the failing test."));
}

#[test]
fn unknown_status_fails_loudly() {
    let (output, _) = with_server(|socket| read_state(socket, "futureStatus"), None);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unknown status"));
}

#[test]
fn different_version_warns_and_still_delivers() {
    let temp = Temp::new();
    let listener = UnixListener::bind(temp.0.join("server.sock")).unwrap();
    let server = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut socket = handshake(stream);
        read_state(&mut socket, "idle");
        let request = receive(&mut socket, "turn/start");
        reply(&mut socket, &request, json!({"turn": {"id": "new-turn"}}));
    });
    let output = command(&temp)
        .env("AGENT_INBOX_TEST_VERSION", "0.999.0")
        .arg("hello")
        .output()
        .unwrap();
    server.join().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("warning: tested with Codex"));
    assert!(
        std::fs::read_to_string(temp.0.join("messages.md"))
            .unwrap()
            .contains("hello")
    );
}

#[test]
fn invalid_messages_are_rejected_before_logging_or_connecting() {
    for message in ["  ".to_owned(), "x".repeat(64 * 1024 + 1)] {
        let temp = Temp::new();
        let output = command(&temp).arg(message).output().unwrap();
        assert!(!output.status.success());
        assert!(!temp.0.join("messages.md").exists());
    }
}

#[test]
fn connection_failure_keeps_message_and_exits_nonzero() {
    let temp = Temp::new();
    let output = command(&temp).arg("hello").output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("handshake"));
    assert!(
        std::fs::read_to_string(temp.0.join("messages.md"))
            .unwrap()
            .contains("hello")
    );
}

#[test]
#[ignore = "read-only check against the running local daemon"]
fn real_daemon_read_only() -> agent_inbox::Result<()> {
    let thread = std::env::var("CODEX_THREAD_ID")?;
    let codex = Path::new("codex");
    let socket = agent_inbox::proxy::checked_socket(codex, None)?;
    let mut client = agent_inbox::codex::Client::connect(codex, &socket, Duration::from_secs(10))?;
    let state = client.read_thread(&thread)?;
    println!("thread {thread}, status {}", state["status"]);
    if state["status"]["type"] == "active" {
        println!("active turn {:?}", client.active_turn(&thread)?);
    }
    let history = client.request(
        "thread/items/list",
        json!({"threadId":thread,"limit":1,"sortDirection":"desc"}),
    )?;
    assert!(history["data"].is_array());
    assert!(
        history
            .get("nextCursor")
            .is_none_or(|cursor| cursor.is_null() || cursor.is_string())
    );
    println!(
        "history page shape verified; next cursor present: {}",
        history
            .get("nextCursor")
            .is_some_and(|cursor| cursor.is_string())
    );
    Ok(())
}

#[test]
fn journal_failure_prevents_delivery() {
    let temp = Temp::new();
    let mut command = Command::new(env!("CARGO_BIN_EXE_agent-inbox"));
    command
        .args(["steer", "--thread", THREAD, "--log"])
        .arg(&temp.0)
        .arg("hello");
    let output = command.output().unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains(&io::Error::from(io::ErrorKind::IsADirectory).to_string())
            || String::from_utf8_lossy(&output.stderr).contains("directory")
    );
}
