#![cfg(unix)]

use agent_inbox::{http, token::Token};
use std::fs;
use std::io::{BufRead, BufReader};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const SESSION: &str = "01a16c01-a2c4-7630-abfe-8b347ab7061b";

struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = PathBuf::from(format!(
            "/tmp/agent-inbox-grok-watch-{}-{stamp}",
            std::process::id()
        ));
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        Self(root)
    }
    fn token(&self, name: &str, value: &str) -> PathBuf {
        let path = self.0.join(name);
        fs::write(&path, value).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        path
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            unsafe {
                libc::kill(self.0.id() as libc::pid_t, libc::SIGTERM);
            }
            let deadline = Instant::now() + Duration::from_secs(5);
            while self.0.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            if self.0.try_wait().ok().flatten().is_none() {
                let _ = self.0.kill();
            }
            let _ = self.0.wait();
        }
    }
}

fn until(mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(8);
    while !check() {
        assert!(
            Instant::now() < deadline,
            "Grok watcher did not reach expected state"
        );
        std::thread::sleep(Duration::from_millis(30));
    }
}

fn queue_status(status: &serde_json::Value) -> serde_json::Value {
    let inbox = status["inbox"].as_str().unwrap();
    let token = Token::read(Path::new(status["inboxTokenFile"].as_str().unwrap())).unwrap();
    let (code, body) = http::post_response(inbox, "/control/status", &token, "{}").unwrap();
    assert_eq!(code, 200);
    serde_json::from_str(&body).unwrap()
}

#[test]
fn grok_watcher_registers_resident_session_and_preserves_uncertain_fifo() {
    let temp = Temp::new();
    let host_token = temp.token("host.token", "grok-watch-host-private-token-0000000000");
    let tester_token = temp.token("tester.token", "grok-watch-sender-private-token-00000000");
    let fake = temp.0.join("fake-grok");
    let output = Command::new(std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()))
        .arg("--edition=2024")
        .arg(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/support/fake_grok.rs"))
        .arg("-o")
        .arg(&fake)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        Command::new(&fake)
            .arg("--version")
            .output()
            .unwrap()
            .status
            .success()
    );
    let leader_socket = temp.0.join("leader.sock");
    let _leader = UnixListener::bind(&leader_socket).unwrap();
    let control = temp.0.join("control/relay.sock");
    let relay_config = temp.0.join("relay.toml");
    fs::write(&relay_config, format!("host='alpha'\nhub='127.0.0.1:9'\nhost_token_file='{}'\njournal='{}'\ncontrol_socket='{}'\n[[agents]]\nname='tester'\ndescription='Test sender'\nsender_token_file='{}'\n", host_token.display(), temp.0.join("relay.md").display(), control.display(), tester_token.display())).unwrap();
    let mut relay = Command::new(env!("CARGO_BIN_EXE_agent-inbox"))
        .args(["relay", "--listen", "127.0.0.1:0", "--config"])
        .arg(&relay_config)
        .arg("--state-dir")
        .arg(temp.0.join("relay-state"))
        .arg("--no-tunnel")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut errors = BufReader::new(relay.stderr.take().unwrap());
    let mut line = String::new();
    let relay_address = loop {
        line.clear();
        assert!(
            errors.read_line(&mut line).unwrap() > 0,
            "relay exited before listening"
        );
        if let Some(address) = line
            .trim()
            .strip_prefix("agent-inbox: listening on http://")
        {
            break address.trim_end_matches('/').to_owned();
        }
    };
    let relay = Process(relay);
    let watch_config = temp.0.join("watch.toml");
    let watch_root = temp.0.join("watch-state");
    fs::write(&watch_config, format!("relay='{relay_address}'\nstate_dir='{}'\ncontrol_socket='{}'\ngrok_bin='{}'\nleader_socket='{}'\npoll_ms=100\nrpc_timeout_secs=2\n", watch_root.display(), control.display(), fake.display(), leader_socket.display())).unwrap();
    let log = temp.0.join("interject.jsonl");
    let fail = temp.0.join("reject-interject");
    let missing = temp.0.join("missing-session");
    let watcher = Process(
        Command::new(env!("CARGO_BIN_EXE_agent-inbox"))
            .args(["grok-watch", "--config"])
            .arg(&watch_config)
            .env("AGENT_INBOX_TEST_GROK_LOG", &log)
            .env("AGENT_INBOX_TEST_GROK_FAIL", &fail)
            .env("AGENT_INBOX_TEST_GROK_MISSING", &missing)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let key: String = SESSION.bytes().map(|byte| format!("{byte:02x}")).collect();
    let status_path = watch_root.join("threads").join(key).join("session.json");
    until(|| {
        fs::read(&status_path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            .is_some_and(|status| status["profile"]["registered"] == true)
    });
    let status: serde_json::Value =
        serde_json::from_slice(&fs::read(&status_path).unwrap()).unwrap();
    assert_eq!(status["profile"]["name"], "alpha.project-grok");
    let self_status = Command::new(env!("CARGO_BIN_EXE_agent-inbox"))
        .args(["grok-self", "--session", SESSION, "--status-file"])
        .arg(&status_path)
        .output()
        .unwrap();
    assert!(self_status.status.success());
    assert!(String::from_utf8_lossy(&self_status.stdout).contains("replyCommand"));
    let token = Token::read(&tester_token).unwrap();
    let send = |text: &str| {
        http::post_response(&relay_address, "/to/alpha.project-grok", &token, text)
            .unwrap()
            .0
    };
    assert_eq!(send("first Grok task"), 202);
    until(|| {
        fs::read_to_string(&log)
            .ok()
            .is_some_and(|body| body.contains("first Grok task"))
    });
    until(|| {
        queue_status(&status)["pending"]
            .as_array()
            .is_some_and(Vec::is_empty)
    });

    fs::write(&fail, "1").unwrap();
    assert_eq!(send("uncertain Grok task"), 202);
    until(|| queue_status(&status)["pending"][0]["phase"] == "unconfirmed");
    assert_eq!(send("later Grok task"), 202);
    let blocked = queue_status(&status);
    assert_eq!(blocked["pending"].as_array().unwrap().len(), 2);
    let id = blocked["pending"][0]["id"].as_str().unwrap();
    fs::remove_file(&fail).unwrap();
    let queue_output = Command::new(env!("CARGO_BIN_EXE_agent-inbox"))
        .args([
            "grok-queue",
            status["inbox"].as_str().unwrap(),
            "--token-file",
            status["inboxTokenFile"].as_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(queue_output.status.success());
    assert!(String::from_utf8_lossy(&queue_output.stdout).contains("unconfirmed"));
    let drop_output = Command::new(env!("CARGO_BIN_EXE_agent-inbox"))
        .args([
            "grok-queue",
            status["inbox"].as_str().unwrap(),
            "--token-file",
            status["inboxTokenFile"].as_str().unwrap(),
            "--drop",
            id,
        ])
        .output()
        .unwrap();
    assert!(drop_output.status.success());
    until(|| {
        queue_status(&status)["pending"]
            .as_array()
            .is_some_and(Vec::is_empty)
    });
    let sent = fs::read_to_string(&log).unwrap();
    assert!(sent.contains("alpha.tester"));
    assert!(sent.contains("later Grok task"));
    assert!(sent.contains("Reply to the incoming sender"));

    fs::write(&missing, "1").unwrap();
    assert_eq!(send("wait for resident Grok"), 202);
    until(|| {
        queue_status(&status)["pending"][0]["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("waiting for resume"))
    });
    fs::remove_file(&missing).unwrap();
    until(|| {
        queue_status(&status)["pending"]
            .as_array()
            .is_some_and(Vec::is_empty)
    });
    drop(watcher);
    assert!(!status_path.exists());
    drop(relay);
}
