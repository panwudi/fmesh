#![cfg(unix)]

use std::io::{BufRead, BufReader, Read};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agent_inbox::{http, token::Token};
use serde_json::{Value, json};
use tungstenite::{Message, WebSocket};

mod support;
const THREAD: &str = "test-codex-thread";
const SECRET: &str = "private-codex-inbox-test-token";
static NEXT: AtomicU64 = AtomicU64::new(0);

struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let path = PathBuf::from(format!(
            "/tmp/agent-inbox-codex-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("token"), SECRET).unwrap();
        std::fs::set_permissions(path.join("token"), std::fs::Permissions::from_mode(0o600))
            .unwrap();
        Self(path)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Inbox {
    child: Child,
    address: String,
    errors: Arc<Mutex<String>>,
}

impl Inbox {
    fn start(temp: &Temp) -> Self {
        Self::start_notify(temp, None)
    }

    fn start_notify(temp: &Temp, notify: Option<&std::path::Path>) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agent-inbox"));
        command
            .args(["codex", "--thread", THREAD, "--listen", "0", "--token-file"])
            .arg(temp.0.join("token"))
            .arg("--log")
            .arg(temp.0.join("messages.md"))
            .arg("--queue-file")
            .arg(temp.0.join("queue.json"))
            .arg("--registry-dir")
            .arg(temp.0.join("registry"))
            .arg("--codex-bin")
            .arg(support::fake_codex())
            .arg("--sock")
            .arg(temp.0.join("server.sock"))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(notify) = notify {
            command
                .arg("--notify-command")
                .arg(notify)
                .arg("--notify-arg")
                .arg(temp.0.join("notices.jsonl"));
        }
        let mut child = command.spawn().unwrap();
        let stderr = child.stderr.take().unwrap();
        let errors = Arc::new(Mutex::new(String::new()));
        let capture = Arc::clone(&errors);
        let (ready, listening) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                let line = line.unwrap();
                capture.lock().unwrap().push_str(&format!("{line}\n"));
                if let Some(address) = line.strip_prefix("agent-inbox: listening on http://") {
                    let _ = ready.send(address.trim_end_matches('/').to_owned());
                }
            }
        });
        let address = listening.recv_timeout(Duration::from_secs(10)).unwrap();
        assert!(address.starts_with("127.0.0.1:"));
        Self {
            child,
            address,
            errors,
        }
    }

    fn send(&self, temp: &Temp, text: &str) -> agent_inbox::Result<u16> {
        http::send(
            &self.address,
            &Token::read(&temp.0.join("token"))?,
            "claude-test",
            text,
        )
    }

    fn status(&self, temp: &Temp) -> Value {
        let token = Token::read(&temp.0.join("token")).unwrap();
        let (status, body) =
            http::post_response(&self.address, "/control/status", &token, "{}").unwrap();
        assert_eq!(status, 200);
        serde_json::from_str(&body).unwrap()
    }

    fn wait_status(&self, temp: &Temp, check: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let status = self.status(temp);
            if check(&status) {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "missing expected queue status: {status}"
            );
            std::thread::sleep(Duration::from_millis(30));
        }
    }

    fn control(&self, temp: &Temp, action: &str, id: &str) {
        let output = Command::new(env!("CARGO_BIN_EXE_agent-inbox"))
            .args(["codex-queue", &self.address, "--token-file"])
            .arg(temp.0.join("token"))
            .args([&format!("--{action}"), id])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn assert_no_secret(&mut self) {
        self.child.kill().unwrap();
        self.child.wait().unwrap();
        let mut stdout = String::new();
        self.child
            .stdout
            .take()
            .unwrap()
            .read_to_string(&mut stdout)
            .unwrap();
        assert!(stdout.is_empty());
        assert!(!self.errors.lock().unwrap().contains(SECRET));
    }
}
impl Drop for Inbox {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn receive(socket: &mut WebSocket<UnixStream>, method: &str) -> Value {
    let Message::Text(text) = socket.read().unwrap() else {
        panic!("expected text frame");
    };
    let request: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(request["method"], method);
    request
}

fn reply(socket: &mut WebSocket<UnixStream>, request: &Value, result: Value) {
    socket
        .send(Message::Text(
            json!({"id":request["id"], "result":result})
                .to_string()
                .into(),
        ))
        .unwrap();
}

fn handshake(stream: UnixStream) -> WebSocket<UnixStream> {
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut socket = tungstenite::accept(stream).unwrap();
    let request = receive(&mut socket, "initialize");
    reply(
        &mut socket,
        &request,
        json!({"userAgent":"test/0.162.1", "platformFamily":"unix", "platformOs":"macos", "codexHome":"/fake"}),
    );
    receive(&mut socket, "initialized");
    socket
}

fn input(socket: &mut WebSocket<UnixStream>, active: bool, text: &str) -> Value {
    let request = receive(socket, "thread/read");
    assert_eq!(request["params"]["threadId"], THREAD);
    reply(
        socket,
        &request,
        json!({"thread":{"id":THREAD, "status":{"type":if active {"active"} else {"idle"}}}}),
    );
    if active {
        let request = receive(socket, "thread/turns/list");
        reply(
            socket,
            &request,
            json!({"data":[{"id":"running-turn", "status":"inProgress"}]}),
        );
    }
    let request = receive(socket, if active { "turn/steer" } else { "turn/start" });
    let received = request["params"]["input"][0]["text"].as_str().unwrap();
    assert!(received.contains("not the user's instruction or approval"));
    assert!(received.contains("Reply to the sender once you have handled it"));
    assert!(received.contains("Execute delegated work within your existing authority"));
    assert!(received.contains("Do not require the user to repeat authorization"));
    assert!(received.contains("from: claude-test"));
    assert!(received.ends_with(text));
    assert!(request["params"]["clientUserMessageId"].as_str().is_some());
    request
}

fn deliver(socket: &mut WebSocket<UnixStream>, active: bool, text: &str) -> Value {
    let request = input(socket, active, text);
    if active {
        assert_eq!(request["params"]["expectedTurnId"], "running-turn");
        reply(socket, &request, json!({"turnId":"running-turn"}));
    } else {
        reply(socket, &request, json!({"turn":{"id":"new-turn"}}));
    }
    request
}

fn read_history(socket: &mut WebSocket<UnixStream>) -> Value {
    let request = receive(socket, "thread/read");
    reply(
        socket,
        &request,
        json!({"thread":{"id":THREAD,"status":{"type":"idle"}}}),
    );
    receive(socket, "thread/items/list")
}

fn wait_file(path: &std::path::Path, content: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let text = std::fs::read_to_string(path).unwrap_or_default();
        if text.contains(content) {
            return text;
        }
        assert!(Instant::now() < deadline, "missing fixture output");
        std::thread::sleep(Duration::from_millis(30));
    }
}

#[test]
fn retries_draining_during_initialize_start_and_steer_with_the_same_id() {
    for stage in ["initialize", "turn/start", "turn/steer"] {
        let temp = Temp::new();
        let listener = UnixListener::bind(temp.0.join("server.sock")).unwrap();
        let fake = std::thread::spawn(move || {
            let mut first = if stage == "initialize" {
                tungstenite::accept(listener.accept().unwrap().0).unwrap()
            } else {
                handshake(listener.accept().unwrap().0)
            };
            let request = if stage == "initialize" {
                receive(&mut first, "initialize")
            } else {
                input(&mut first, stage == "turn/steer", "draining message")
            };
            let id = request["params"]["clientUserMessageId"].clone();
            first.send(Message::Text(json!({"id":request["id"],"error":{"code":-32600,"message":"Server is draining"}}).to_string().into())).unwrap();
            drop(first);
            let mut next = handshake(listener.accept().unwrap().0);
            let accepted = deliver(&mut next, stage == "turn/steer", "draining message");
            if stage != "initialize" {
                assert_eq!(id, accepted["params"]["clientUserMessageId"]);
            }
        });
        let inbox = Inbox::start(&temp);
        assert_eq!(inbox.send(&temp, "draining message").unwrap(), 202);
        fake.join().unwrap();
        inbox.wait_status(&temp, |status| {
            status["receipts"].as_array().unwrap().len() == 1
        });
        assert_eq!(
            std::fs::read_to_string(temp.0.join("messages.md"))
                .unwrap()
                .matches("## From:")
                .count(),
            1
        );
    }
}

#[test]
fn refused_connections_recover_and_keep_fifo() {
    let temp = Temp::new();
    let inbox = Inbox::start(&temp);
    assert_eq!(inbox.send(&temp, "first after restart").unwrap(), 202);
    assert_eq!(inbox.send(&temp, "second after restart").unwrap(), 202);
    std::thread::sleep(Duration::from_millis(100));
    let listener = UnixListener::bind(temp.0.join("server.sock")).unwrap();
    let fake = std::thread::spawn(move || {
        for text in ["first after restart", "second after restart"] {
            let mut socket = handshake(listener.accept().unwrap().0);
            deliver(&mut socket, false, text);
        }
    });
    fake.join().unwrap();
    inbox.wait_status(&temp, |status| {
        status["receipts"].as_array().unwrap().len() == 2
    });
}

#[test]
fn hold_during_metadata_read_prevents_the_mutation_and_release_retries() {
    let temp = Temp::new();
    let listener = UnixListener::bind(temp.0.join("server.sock")).unwrap();
    let (reading, ready) = mpsc::channel();
    let (proceed, resume) = mpsc::channel();
    let (closed, stopped) = mpsc::channel();
    let fake = std::thread::spawn(move || {
        let mut first = handshake(listener.accept().unwrap().0);
        let request = receive(&mut first, "thread/read");
        reading.send(()).unwrap();
        resume.recv_timeout(Duration::from_secs(3)).unwrap();
        reply(
            &mut first,
            &request,
            json!({"thread":{"id":THREAD,"status":{"type":"idle"}}}),
        );
        assert!(
            !matches!(first.read(), Ok(Message::Text(_))),
            "held worker sent a mutation"
        );
        closed.send(()).unwrap();
        let mut next = handshake(listener.accept().unwrap().0);
        deliver(&mut next, false, "hold race");
    });
    let inbox = Inbox::start(&temp);
    inbox.send(&temp, "hold race").unwrap();
    ready.recv_timeout(Duration::from_secs(3)).unwrap();
    let status = inbox.status(&temp);
    let lease = agent_inbox::state::id().unwrap();
    let token = Token::read(&temp.0.join("token")).unwrap();
    let body = json!({"instance":status["instance"],"lease":lease,"expires":chrono::Utc::now().timestamp() as u64 + 60});
    assert_eq!(
        http::post_response(&inbox.address, "/control/hold", &token, &body.to_string())
            .unwrap()
            .0,
        200
    );
    proceed.send(()).unwrap();
    stopped.recv_timeout(Duration::from_secs(3)).unwrap();
    let held = inbox.status(&temp);
    assert_eq!(held["held"], true);
    assert_eq!(held["pending"][0]["phase"], "queued");
    assert_eq!(
        http::post_response(
            &inbox.address,
            "/control/release",
            &token,
            &body.to_string()
        )
        .unwrap()
        .0,
        200
    );
    fake.join().unwrap();
    inbox.wait_status(&temp, |s| s["receipts"].as_array().unwrap().len() == 1);
}

#[test]
fn hold_is_not_quiescent_until_the_inflight_ack_is_saved() {
    let temp = Temp::new();
    let listener = UnixListener::bind(temp.0.join("server.sock")).unwrap();
    let (sending, sent) = mpsc::channel();
    let (ack, resume) = mpsc::channel();
    let fake = std::thread::spawn(move || {
        let mut socket = handshake(listener.accept().unwrap().0);
        let request = input(&mut socket, false, "already sent");
        sending.send(()).unwrap();
        resume.recv_timeout(Duration::from_secs(3)).unwrap();
        reply(&mut socket, &request, json!({"turn":{"id":"new-turn"}}));
    });
    let inbox = Inbox::start(&temp);
    inbox.send(&temp, "already sent").unwrap();
    sent.recv_timeout(Duration::from_secs(3)).unwrap();
    let status = inbox.status(&temp);
    let token = Token::read(&temp.0.join("token")).unwrap();
    let body = json!({"instance":status["instance"],"lease":agent_inbox::state::id().unwrap(),"expires":chrono::Utc::now().timestamp() as u64 + 60});
    assert_eq!(
        http::post_response(&inbox.address, "/control/hold", &token, &body.to_string())
            .unwrap()
            .0,
        200
    );
    let status = inbox.status(&temp);
    assert_eq!(status["holdRequested"], true);
    assert_eq!(status["held"], false);
    ack.send(()).unwrap();
    fake.join().unwrap();
    inbox.wait_status(&temp, |s| {
        s["held"] == true && s["receipts"].as_array().unwrap().len() == 1
    });
}

#[test]
fn queued_ids_survive_inbox_restart() {
    let temp = Temp::new();
    let mut first = Inbox::start(&temp);
    assert_eq!(first.send(&temp, "persisted message").unwrap(), 202);
    let old: Value =
        serde_json::from_str(&std::fs::read_to_string(temp.0.join("queue.json")).unwrap()).unwrap();
    let id = old["pending"][0]["id"].as_str().unwrap().to_owned();
    first.child.kill().unwrap();
    first.child.wait().unwrap();
    let listener = UnixListener::bind(temp.0.join("server.sock")).unwrap();
    let fake = std::thread::spawn(move || {
        let mut socket = handshake(listener.accept().unwrap().0);
        let request = deliver(&mut socket, false, "persisted message");
        assert_eq!(request["params"]["clientUserMessageId"], id);
    });
    let second = Inbox::start(&temp);
    fake.join().unwrap();
    second.wait_status(&temp, |status| {
        status["receipts"].as_array().unwrap().len() == 1
    });
}

#[test]
fn lost_ack_reconciles_a_found_id_without_replaying_and_preserves_fifo() {
    let temp = Temp::new();
    let listener = UnixListener::bind(temp.0.join("server.sock")).unwrap();
    let fake = std::thread::spawn(move || {
        let mut socket = handshake(listener.accept().unwrap().0);
        let request = input(&mut socket, false, "ack lost");
        let id = request["params"]["clientUserMessageId"].clone();
        drop(socket);
        let mut socket = handshake(listener.accept().unwrap().0);
        let request = read_history(&mut socket);
        reply(
            &mut socket,
            &request,
            json!({"data":[{"turnId":"already-accepted","item":{"type":"userMessage","clientId":id,"id":"server-item","content":[]}}],"nextCursor":null}),
        );
        drop(socket);
        let mut socket = handshake(listener.accept().unwrap().0);
        deliver(&mut socket, false, "following message");
    });
    let inbox = Inbox::start(&temp);
    assert_eq!(inbox.send(&temp, "ack lost").unwrap(), 202);
    assert_eq!(inbox.send(&temp, "following message").unwrap(), 202);
    fake.join().unwrap();
    let status = inbox.wait_status(&temp, |status| {
        status["receipts"].as_array().unwrap().len() == 2
    });
    assert_eq!(status["receipts"][0]["turnId"], "already-accepted");
}

#[test]
fn complete_absent_history_allows_resend_with_same_client_id() {
    let temp = Temp::new();
    let listener = UnixListener::bind(temp.0.join("server.sock")).unwrap();
    let fake = std::thread::spawn(move || {
        let mut socket = handshake(listener.accept().unwrap().0);
        let first = input(&mut socket, false, "not persisted");
        let id = first["params"]["clientUserMessageId"].clone();
        drop(socket);
        let mut socket = handshake(listener.accept().unwrap().0);
        let request = read_history(&mut socket);
        reply(
            &mut socket,
            &request,
            json!({"data":[],"nextCursor":"next-page"}),
        );
        let request = receive(&mut socket, "thread/items/list");
        assert_eq!(request["params"]["cursor"], "next-page");
        reply(&mut socket, &request, json!({"data":[],"nextCursor":null}));
        let accepted = deliver(&mut socket, false, "not persisted");
        assert_eq!(accepted["params"]["clientUserMessageId"], id);
    });
    let inbox = Inbox::start(&temp);
    assert_eq!(inbox.send(&temp, "not persisted").unwrap(), 202);
    fake.join().unwrap();
    inbox.wait_status(&temp, |status| {
        status["receipts"].as_array().unwrap().len() == 1
    });
}

#[test]
fn incomplete_history_blocks_fifo_notifies_and_explicit_drop_releases_next() {
    let temp = Temp::new();
    let notify = temp.0.join("fake-notify");
    assert!(
        Command::new("rustc")
            .arg("--edition=2024")
            .arg(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/support/fake_notify.rs")
            )
            .arg("-o")
            .arg(&notify)
            .status()
            .unwrap()
            .success()
    );
    let listener = UnixListener::bind(temp.0.join("server.sock")).unwrap();
    let fake = std::thread::spawn(move || {
        let mut socket = handshake(listener.accept().unwrap().0);
        input(&mut socket, false, "history unknown");
        drop(socket);
        let mut socket = handshake(listener.accept().unwrap().0);
        let request = read_history(&mut socket);
        reply(
            &mut socket,
            &request,
            json!({"data":[],"nextCursor":"missing-page"}),
        );
        receive(&mut socket, "thread/items/list");
        drop(socket);
        let mut socket = handshake(listener.accept().unwrap().0);
        deliver(&mut socket, false, "after blocked");
    });
    let inbox = Inbox::start_notify(&temp, Some(&notify));
    assert_eq!(inbox.send(&temp, "history unknown").unwrap(), 202);
    assert_eq!(inbox.send(&temp, "after blocked").unwrap(), 202);
    let status = inbox.wait_status(&temp, |status| {
        status["pending"][0]["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("history unavailable"))
    });
    assert_eq!(status["pending"].as_array().unwrap().len(), 2);
    let notices = wait_file(&temp.0.join("notices.jsonl"), "history unavailable");
    assert!(notices.contains("fifoBlocked"));
    assert!(notices.contains("--resend"));
    assert!(notices.contains("--drop"));
    assert!(!notices.contains(SECRET));
    let journal = std::fs::read_to_string(temp.0.join("messages.md")).unwrap();
    assert!(journal.contains("FIFO blocks later messages"));
    inbox.control(&temp, "drop", status["pending"][0]["id"].as_str().unwrap());
    fake.join().unwrap();
    inbox.wait_status(&temp, |status| {
        status["pending"].as_array().unwrap().is_empty()
    });
}

#[test]
fn inbox_crash_after_send_reconciles_persisted_attempt() {
    let temp = Temp::new();
    let listener = UnixListener::bind(temp.0.join("server.sock")).unwrap();
    let (sent, received) = mpsc::channel();
    let fake = std::thread::spawn(move || {
        let mut socket = handshake(listener.accept().unwrap().0);
        let request = input(&mut socket, false, "crash during ack");
        let id = request["params"]["clientUserMessageId"].clone();
        sent.send(()).unwrap();
        drop(socket);
        let mut socket = handshake(listener.accept().unwrap().0);
        let request = read_history(&mut socket);
        reply(
            &mut socket,
            &request,
            json!({"data":[{"turnId":"accepted-before-crash","item":{"type":"userMessage","clientId":id}}],"nextCursor":null}),
        );
    });
    let mut first = Inbox::start(&temp);
    assert_eq!(first.send(&temp, "crash during ack").unwrap(), 202);
    received.recv_timeout(Duration::from_secs(5)).unwrap();
    first.child.kill().unwrap();
    first.child.wait().unwrap();
    let next = Inbox::start(&temp);
    fake.join().unwrap();
    let status = next.wait_status(&temp, |status| {
        status["receipts"].as_array().unwrap().len() == 1
    });
    assert_eq!(status["receipts"][0]["turnId"], "accepted-before-crash");
}

#[test]
fn explicit_resend_of_blocked_message_uses_existing_id() {
    let temp = Temp::new();
    let listener = UnixListener::bind(temp.0.join("server.sock")).unwrap();
    let fake = std::thread::spawn(move || {
        let mut socket = handshake(listener.accept().unwrap().0);
        let first = input(&mut socket, false, "resend after review");
        socket
            .send(Message::Text(
                json!({"id":first["id"],"error":{"code":-32601,"message":"unsupported method"}})
                    .to_string()
                    .into(),
            ))
            .unwrap();
        let id = first["params"]["clientUserMessageId"].clone();
        drop(socket);
        let mut socket = handshake(listener.accept().unwrap().0);
        let accepted = deliver(&mut socket, false, "resend after review");
        assert_eq!(accepted["params"]["clientUserMessageId"], id);
    });
    let inbox = Inbox::start(&temp);
    assert_eq!(inbox.send(&temp, "resend after review").unwrap(), 202);
    let status = inbox.wait_status(&temp, |status| status["pending"][0]["phase"] == "blocked");
    inbox.control(
        &temp,
        "resend",
        status["pending"][0]["id"].as_str().unwrap(),
    );
    fake.join().unwrap();
    inbox.wait_status(&temp, |status| {
        status["receipts"].as_array().unwrap().len() == 1
    });
}

#[test]
fn http_inbox_steers_active_and_starts_idle_against_fake_stdio_app_server() {
    let temp = Temp::new();
    let listener = UnixListener::bind(temp.0.join("server.sock")).unwrap();
    let fake = std::thread::spawn(move || {
        for (active, text) in [(true, "active message\n中文"), (false, "idle message")] {
            let mut socket = handshake(listener.accept().unwrap().0);
            deliver(&mut socket, active, text);
        }
    });
    let mut inbox = Inbox::start(&temp);
    assert_eq!(inbox.send(&temp, "active message\n中文").unwrap(), 202);
    assert_eq!(inbox.send(&temp, "idle message").unwrap(), 202);
    fake.join().unwrap();
    inbox.wait_status(&temp, |status| {
        status["receipts"].as_array().unwrap().len() == 2
    });
    let log = std::fs::read_to_string(temp.0.join("messages.md")).unwrap();
    assert_eq!(log.matches("## From:").count(), 2);
    assert!(log.contains(THREAD));
    assert!(log.contains("idle message"));
    assert!(!log.contains(SECRET));
    inbox.assert_no_secret();
}

#[test]
fn failed_delivery_is_logged_and_next_message_connects_afresh() {
    let temp = Temp::new();
    let listener = UnixListener::bind(temp.0.join("server.sock")).unwrap();
    let fake = std::thread::spawn(move || {
        let mut socket = handshake(listener.accept().unwrap().0);
        let request = receive(&mut socket, "thread/read");
        socket.send(Message::Text(json!({"id":request["id"], "error":{"code":-32601, "message":"test server temporarily unavailable"}}).to_string().into())).unwrap();
        drop(socket);
        let mut socket = handshake(listener.accept().unwrap().0);
        deliver(&mut socket, true, "second message");
    });
    let mut inbox = Inbox::start(&temp);
    assert_eq!(inbox.send(&temp, "first message").unwrap(), 202);
    let status = inbox.wait_status(&temp, |status| status["pending"][0]["phase"] == "blocked");
    assert!(
        std::fs::read_to_string(temp.0.join("messages.md"))
            .unwrap()
            .contains("first message")
    );
    assert_eq!(inbox.send(&temp, "second message").unwrap(), 202);
    inbox.control(&temp, "drop", status["pending"][0]["id"].as_str().unwrap());
    fake.join().unwrap();
    inbox.wait_status(&temp, |status| {
        status["pending"].as_array().unwrap().is_empty()
    });
    inbox.assert_no_secret();
}

#[test]
fn unavailable_daemon_keeps_message_in_receiver_journal() {
    let temp = Temp::new();
    let mut inbox = Inbox::start(&temp);
    assert_eq!(inbox.send(&temp, "saved while disconnected").unwrap(), 202);
    assert!(
        std::fs::read_to_string(temp.0.join("messages.md"))
            .unwrap()
            .contains("saved while disconnected")
    );
    inbox.assert_no_secret();
}

#[test]
fn unauthorized_message_never_reaches_app_server_or_journal() {
    let temp = Temp::new();
    let listener = UnixListener::bind(temp.0.join("server.sock")).unwrap();
    listener.set_nonblocking(true).unwrap();
    std::fs::write(temp.0.join("wrong-token"), "wrong-token").unwrap();
    std::fs::set_permissions(
        temp.0.join("wrong-token"),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    let mut inbox = Inbox::start(&temp);
    let error = http::send(
        &inbox.address,
        &Token::read(&temp.0.join("wrong-token")).unwrap(),
        "claude",
        "not authorized",
    )
    .unwrap_err();
    assert!(error.to_string().contains("HTTP 401"));
    assert!(!temp.0.join("messages.md").exists());
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    inbox.assert_no_secret();
}

#[test]
fn codex_requires_thread_and_listen() {
    let temp = Temp::new();
    for args in [
        vec!["codex", "--thread", THREAD, "--token-file"],
        vec!["codex", "--listen", "0", "--token-file"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_agent-inbox"))
            .args(args)
            .arg(temp.0.join("token"))
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("is required"));
    }
}
