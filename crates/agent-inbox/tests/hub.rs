#![cfg(unix)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agent_inbox::{http, token::Token};

const ALICE: &str = "alice-private-hub-token-0123456789";
const BOB: &str = "bob-private-hub-token-012345678901";
const PEER: &str = "peer-private-inbox-token-01234567";
static NEXT: AtomicU64 = AtomicU64::new(0);

struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let path = PathBuf::from(format!(
            "/tmp/agent-inbox-hub-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        let temp = Self(path);
        for (name, contents) in [
            ("alice.token", ALICE),
            ("bob.token", BOB),
            ("peer.token", PEER),
        ] {
            temp.token(name, contents);
        }
        temp
    }

    fn token(&self, name: &str, text: &str) {
        std::fs::write(self.0.join(name), text).unwrap();
        std::fs::set_permissions(self.0.join(name), std::fs::Permissions::from_mode(0o600))
            .unwrap();
    }

    fn config(&self, peer: &str) -> String {
        format!(
            "journal = 'hub.md'\n[[peers]]\nname = 'recipient'\ninbox = '{peer}'\ntoken_file = 'peer.token'\n[[senders]]\nname = 'alice-agent'\ntoken_file = 'alice.token'\n[[senders]]\nname = 'bob-agent'\ntoken_file = 'bob.token'\n"
        )
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct FakePeer {
    address: String,
    received: Receiver<(String, String)>,
    running: Arc<AtomicBool>,
    server: Option<std::thread::JoinHandle<agent_inbox::Result<()>>>,
}
impl FakePeer {
    fn start(temp: &Temp, status: u16) -> Self {
        Self::start_with_delay(temp, status, Duration::ZERO)
    }

    fn start_with_delay(temp: &Temp, status: u16, delay: Duration) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let token = Token::read(&temp.0.join("peer.token")).unwrap();
        let running = Arc::new(AtomicBool::new(true));
        let control = Arc::clone(&running);
        let (sender, received) = mpsc::channel();
        let handler = Arc::new(move |text: &str, from: Option<&str>| {
            sender
                .send((text.to_owned(), from.unwrap_or("").to_owned()))
                .unwrap();
            std::thread::sleep(delay);
            http::Response::new(status, "fake peer accepted")
        });
        let server = std::thread::spawn(move || http::serve(listener, token, control, handler));
        Self {
            address,
            received,
            running,
            server: Some(server),
        }
    }
}
impl Drop for FakePeer {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Release);
        self.server.take().unwrap().join().unwrap().unwrap();
    }
}

struct Hub {
    child: Child,
    address: String,
    errors: Arc<Mutex<String>>,
}
impl Hub {
    fn start(temp: &Temp, config: &str) -> Self {
        std::fs::write(temp.0.join("hub.toml"), config).unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_agent-inbox"))
            .args(["hub", "--listen", "0", "--config"])
            .arg(temp.0.join("hub.toml"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
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
        let address = listening
            .recv_timeout(Duration::from_secs(10))
            .unwrap_or_else(|error| {
                let _ = child.kill();
                let _ = child.wait();
                panic!("hub startup failed: {error}; {}", errors.lock().unwrap());
            });
        assert!(address.starts_with("127.0.0.1:"));
        Self {
            child,
            address,
            errors,
        }
    }

    fn assert_no_tokens(&self, temp: &Temp) {
        let error = self.errors.lock().unwrap();
        let journal = std::fs::read_to_string(temp.0.join("hub.md")).unwrap_or_default();
        for secret in [ALICE, BOB, PEER] {
            assert!(!error.contains(secret));
            assert!(!journal.contains(secret));
        }
    }
}
impl Drop for Hub {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn post(address: &str, path: &str, token: &str, claims: &str, text: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(address).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(15)))
        .unwrap();
    write!(stream, "POST {path} HTTP/1.1\r\nHost: {address}\r\nAuthorization: Bearer {token}\r\n{claims}Content-Length: {}\r\nConnection: close\r\n\r\n{text}", text.len()).unwrap();
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let status = line.split_whitespace().nth(1).unwrap().parse().unwrap();
    let mut length = 0;
    loop {
        line.clear();
        reader.read_line(&mut line).unwrap();
        if line == "\r\n" {
            break;
        }
        if let Some(value) = line.strip_prefix("Content-Length: ") {
            length = value.trim().parse().unwrap();
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    (status, String::from_utf8(body).unwrap())
}

fn delayed_relay(upstream: String) -> (String, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let relay = std::thread::spawn(move || {
        let (mut client, _) = listener.accept().unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(15)))
            .unwrap();
        client
            .set_write_timeout(Some(Duration::from_secs(15)))
            .unwrap();
        let mut reader = BufReader::new(&mut client);
        let mut request = Vec::new();
        let mut length = None;
        loop {
            let mut line = String::new();
            assert_ne!(reader.read_line(&mut line).unwrap(), 0);
            request.extend_from_slice(line.as_bytes());
            if line == "\r\n" {
                break;
            }
            if let Some(value) = line.strip_prefix("Content-Length: ") {
                length = Some(value.trim().parse::<usize>().unwrap());
            }
        }
        let mut body = vec![0; length.unwrap()];
        reader.read_exact(&mut body).unwrap();
        request.extend_from_slice(&body);
        let mut peer = TcpStream::connect(upstream).unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(15)))
            .unwrap();
        peer.write_all(&request).unwrap();
        peer.shutdown(std::net::Shutdown::Write).unwrap();
        let mut response = Vec::new();
        peer.read_to_end(&mut response).unwrap();
        // The destination has already replied, but the sender's tunnel is slow.
        std::thread::sleep(Duration::from_secs(16));
        let _ = client.write_all(&response);
    });
    (address, relay)
}

#[test]
fn send_waits_for_delayed_response_after_direct_or_hub_delivery() {
    for hub_mode in [false, true] {
        let temp = Temp::new();
        let peer = FakePeer::start(&temp, 200);
        let hub = Hub::start(&temp, &temp.config(&peer.address));
        let upstream = if hub_mode {
            &hub.address
        } else {
            &peer.address
        };
        let (address, relay) = delayed_relay(upstream.clone());
        let mut command = Command::new(env!("CARGO_BIN_EXE_agent-inbox"));
        command.arg("send");
        if hub_mode {
            command.args(["--hub", &address, "--to", "recipient"]);
        } else {
            command.arg(address);
        }
        let token = if hub_mode {
            "alice.token"
        } else {
            "peer.token"
        };
        let output = command
            .arg("--token-file")
            .arg(temp.0.join(token))
            .args(["--from", "codex", "--log"])
            .arg(temp.0.join("sent.md"))
            .arg("delivered before the delayed reply")
            .output()
            .unwrap();
        relay.join().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("HTTP 200"));
        assert_eq!(
            peer.received.recv_timeout(Duration::from_secs(2)).unwrap(),
            (
                "delivered before the delayed reply".into(),
                if hub_mode { "alice-agent" } else { "codex" }.into()
            )
        );
        assert!(peer.received.try_recv().is_err());
        let sent = std::fs::read_to_string(temp.0.join("sent.md")).unwrap();
        assert!(sent.contains("delivered before the delayed reply"));
        assert_eq!(sent.matches("## From:").count(), 1);
    }
}

#[test]
fn routes_with_peer_token_and_sender_identity_from_hub_token() {
    let temp = Temp::new();
    let peer = FakePeer::start(&temp, 200);
    let hub = Hub::start(&temp, &temp.config(&peer.address));
    assert_eq!(
        post(
            &hub.address,
            "/to/recipient",
            ALICE,
            "X-Agent-From: bob-agent\r\nX-Agent-From: admin\r\n",
            "hello\n中文"
        )
        .0,
        200
    );
    assert_eq!(
        peer.received.recv_timeout(Duration::from_secs(2)).unwrap(),
        ("hello\n中文".into(), "alice-agent".into())
    );
    assert_eq!(
        post(
            &hub.address,
            "/to/recipient",
            BOB,
            "X-Agent-From: alice-agent\r\n",
            "reply"
        )
        .0,
        200
    );
    assert_eq!(
        peer.received.recv_timeout(Duration::from_secs(2)).unwrap(),
        ("reply".into(), "bob-agent".into())
    );
    let journal = std::fs::read_to_string(temp.0.join("hub.md")).unwrap();
    assert!(journal.contains("From: alice-agent, To: recipient"));
    assert!(journal.contains("From: bob-agent, To: recipient"));
    hub.assert_no_tokens(&temp);
}

#[test]
fn unauthorized_unknown_and_oversized_requests_have_distinct_statuses() {
    let temp = Temp::new();
    let peer = FakePeer::start(&temp, 200);
    let hub = Hub::start(&temp, &temp.config(&peer.address));
    assert_eq!(
        post(&hub.address, "/to/recipient", "wrong", "", "unauthorized").0,
        401
    );
    assert_eq!(
        post(&hub.address, "/to/recipient", ALICE, "", &"x".repeat(65537)).0,
        400
    );
    assert!(!temp.0.join("hub.md").exists());
    assert_eq!(
        post(&hub.address, "/to/missing", ALICE, "", "unknown peer").0,
        404
    );
    assert!(
        std::fs::read_to_string(temp.0.join("hub.md"))
            .unwrap()
            .contains("To: missing")
    );
    assert!(
        peer.received
            .recv_timeout(Duration::from_millis(50))
            .is_err()
    );
    hub.assert_no_tokens(&temp);
}

#[test]
fn offline_peer_fails_quickly_and_message_remains_in_hub_journal() {
    let temp = Temp::new();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap().to_string();
    drop(listener);
    let hub = Hub::start(&temp, &temp.config(&address));
    let started = Instant::now();
    let result = post(
        &hub.address,
        "/to/recipient",
        ALICE,
        "",
        "retain while offline",
    );
    assert_eq!(result.0, 503);
    assert!(result.1.contains("peer offline"));
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(
        std::fs::read_to_string(temp.0.join("hub.md"))
            .unwrap()
            .contains("retain while offline")
    );
}

#[test]
fn silent_peer_is_bounded_by_the_forwarding_deadline() {
    let temp = Temp::new();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let hub = Hub::start(
        &temp,
        &temp.config(&listener.local_addr().unwrap().to_string()),
    );
    let started = Instant::now();
    assert_eq!(
        post(&hub.address, "/to/recipient", ALICE, "", "silent peer").0,
        503
    );
    assert!((Duration::from_secs(9)..Duration::from_secs(13)).contains(&started.elapsed()));
}

#[test]
fn hub_confirms_delivery_when_the_peer_reply_takes_more_than_three_seconds() {
    let temp = Temp::new();
    let peer = FakePeer::start_with_delay(&temp, 200, Duration::from_secs(6));
    let hub = Hub::start(&temp, &temp.config(&peer.address));
    let output = Command::new(env!("CARGO_BIN_EXE_agent-inbox"))
        .args(["send", "--hub"])
        .arg(&hub.address)
        .args(["--to", "recipient", "--token-file"])
        .arg(temp.0.join("alice.token"))
        .arg("--log")
        .arg(temp.0.join("sent.md"))
        .arg("message with a slow peer reply")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("HTTP 200"));
    assert_eq!(
        peer.received.recv_timeout(Duration::from_secs(1)).unwrap(),
        (
            "message with a slow peer reply".into(),
            "alice-agent".into()
        )
    );
    assert!(peer.received.try_recv().is_err());
}

#[test]
fn peer_pending_and_rejection_are_reported_without_blind_retry() {
    for (peer_status, expected) in [(202, 202), (401, 502), (404, 502), (503, 502)] {
        let temp = Temp::new();
        let peer = FakePeer::start(&temp, peer_status);
        let hub = Hub::start(&temp, &temp.config(&peer.address));
        assert_eq!(
            post(&hub.address, "/to/recipient", ALICE, "", "message").0,
            expected
        );
        peer.received.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(
            peer.received
                .recv_timeout(Duration::from_millis(50))
                .is_err()
        );
    }
}

#[test]
fn invalid_configuration_fails_without_printing_credentials() {
    let temp = Temp::new();
    let base = temp.config("127.0.0.1:1");
    for config in [
        base.replace("bob.token", "alice.token"),
        base.replace("127.0.0.1:1", "192.0.2.10:1234"),
        base.replace("bob-agent", "alice-agent"),
        format!("{base}\nsecret = '{ALICE}'"),
    ] {
        std::fs::write(temp.0.join("hub.toml"), config).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_agent-inbox"))
            .args(["hub", "--listen", "0", "--config"])
            .arg(temp.0.join("hub.toml"))
            .output()
            .unwrap();
        assert!(!output.status.success());
        let errors = String::from_utf8_lossy(&output.stderr);
        for secret in [ALICE, BOB, PEER] {
            assert!(!errors.contains(secret));
        }
    }
}

#[test]
fn non_loopback_hub_bind_is_rejected_before_startup() {
    let output = Command::new(env!("CARGO_BIN_EXE_agent-inbox"))
        .args(["hub", "--listen", "0.0.0.0:0", "--config", "/unused"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("loopback"));
}

#[test]
fn hub_journal_failure_prevents_forwarding() {
    let temp = Temp::new();
    let peer = FakePeer::start(&temp, 200);
    let hub = Hub::start(
        &temp,
        &temp
            .config(&peer.address)
            .replace("journal = 'hub.md'", "journal = '.'"),
    );
    assert_eq!(
        post(&hub.address, "/to/recipient", ALICE, "", "must not forward").0,
        500
    );
    assert!(
        peer.received
            .recv_timeout(Duration::from_millis(50))
            .is_err()
    );
}

#[test]
fn hub_send_cli_uses_the_token_identity_and_logical_destination() {
    let temp = Temp::new();
    let peer = FakePeer::start(&temp, 200);
    let hub = Hub::start(&temp, &temp.config(&peer.address));
    let output = Command::new(env!("CARGO_BIN_EXE_agent-inbox"))
        .args(["send", "--hub"])
        .arg(&hub.address)
        .args(["--to", "recipient", "--token-file"])
        .arg(temp.0.join("alice.token"))
        .args(["--from", "bob-agent", "--log"])
        .arg(temp.0.join("sent.md"))
        .arg("message from CLI\n中文")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        peer.received.recv_timeout(Duration::from_secs(2)).unwrap(),
        ("message from CLI\n中文".into(), "alice-agent".into())
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("for recipient (HTTP 200)"));
    let journal = std::fs::read_to_string(temp.0.join("sent.md")).unwrap();
    assert!(journal.contains("From: hub-token-holder, To: recipient"));
    assert!(!journal.contains("bob-agent"));
    for secret in [ALICE, BOB, PEER] {
        assert!(!stdout.contains(secret));
    }
    hub.assert_no_tokens(&temp);
}

#[test]
fn hub_send_offline_failure_preserves_the_senders_copy() {
    let temp = Temp::new();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap().to_string();
    drop(listener);
    let hub = Hub::start(&temp, &temp.config(&address));
    let output = Command::new(env!("CARGO_BIN_EXE_agent-inbox"))
        .args(["send", "--hub"])
        .arg(&hub.address)
        .args(["--to", "recipient", "--token-file"])
        .arg(temp.0.join("alice.token"))
        .arg("--log")
        .arg(temp.0.join("sent.md"))
        .arg("save this offline message")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("HTTP 503"));
    assert!(
        std::fs::read_to_string(temp.0.join("sent.md"))
            .unwrap()
            .contains("save this offline message")
    );
    assert!(
        std::fs::read_to_string(temp.0.join("hub.md"))
            .unwrap()
            .contains("save this offline message")
    );
}

#[test]
fn hub_send_rejects_missing_or_ambiguous_destination_flags() {
    for args in [
        vec!["send", "--hub", "127.0.0.1:1", "text"],
        vec!["send", "--to", "peer", "text"],
        vec![
            "send",
            "127.0.0.1:1",
            "--hub",
            "127.0.0.1:2",
            "--to",
            "peer",
            "text",
        ],
        vec!["send", "--hub", "127.0.0.1:1", "--to", "../peer", "text"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_agent-inbox"))
            .args(args)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(!String::from_utf8_lossy(&output.stderr).contains("cannot connect"));
    }
}
