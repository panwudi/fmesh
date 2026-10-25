#![cfg(unix)]

use agent_inbox::{http, token::Token};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const SECRET: &str = "gui-test-sender-token-00000000000000000";

struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = PathBuf::from(format!(
            "/tmp/agent-inbox-gui-{}-{stamp}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();
        fs::write(path.join("sender.token"), SECRET).unwrap();
        fs::set_permissions(path.join("sender.token"), fs::Permissions::from_mode(0o600)).unwrap();
        Self(path)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct Gui(Child);
impl Drop for Gui {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn request(
    address: &str,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> (u16, String, String) {
    let mut stream = TcpStream::connect(address).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    write!(stream, "{method} {path} HTTP/1.1\r\nHost: {address}\r\nContent-Length: {}\r\nConnection: close\r\n", body.len()).unwrap();
    for (name, value) in headers {
        write!(stream, "{name}: {value}\r\n").unwrap();
    }
    write!(stream, "\r\n{body}").unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    let (head, content) = response.split_once("\r\n\r\n").unwrap();
    let code = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    (code, head.to_owned(), content.to_owned())
}

#[test]
fn on_demand_gui_keeps_sender_token_server_side_and_requires_session_and_csrf() {
    let temp = Temp::new();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let relay = listener.local_addr().unwrap().to_string();
    let running = Arc::new(AtomicBool::new(true));
    let control = Arc::clone(&running);
    let token = Token::read(&temp.0.join("sender.token")).unwrap();
    let authenticate = Arc::new(move |header: &[u8]| {
        token
            .authorizes(header)
            .then_some(http::SenderIdentity::Authenticated("gui-test".into()))
    });
    let (sent, received) = std::sync::mpsc::channel();
    let handler = Arc::new(
        move |path: &str, text: &str, _: Option<&str>, _: http::Origin| {
            if path == "/whoami" {
                http::Response::json(200, serde_json::json!({"name":"alpha.gui"}))
            } else if path == "/directory/list" {
                http::Response::json(
                    200,
                    serde_json::json!({"agents":[{"name":"alpha.worker","description":"Builds things","status":"online","host_online":true,"inbox_reachable":true}],"nextCursor":null}),
                )
            } else if path == "/to/alpha.worker" {
                sent.send(text.to_owned()).unwrap();
                http::Response::new(202, "queued")
            } else {
                http::Response::new(404, "unknown")
            }
        },
    );
    let relay_thread = std::thread::spawn(move || {
        http::serve_authenticated(listener, control, authenticate, handler).unwrap();
    });

    let bootstrap_file = temp.0.join("bootstrap.token");
    let mut child = Command::new(env!("CARGO_BIN_EXE_agent-inbox"))
        .args(["gui", "--hub", &relay, "--token-file"])
        .arg(temp.0.join("sender.token"))
        .arg("--no-open")
        .arg("--bootstrap-file")
        .arg(&bootstrap_file)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut ready = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut ready)
        .unwrap();
    let gui = Gui(child);
    let address = ready
        .trim()
        .strip_prefix("agent-inbox GUI ready at http://")
        .unwrap()
        .trim_end_matches('/');
    let bootstrap = fs::read_to_string(&bootstrap_file).unwrap();
    assert!(!ready.contains(SECRET));
    assert!(!ready.contains(&bootstrap));

    let (code, _, page) = request(address, "GET", "/", &[], "");
    assert_eq!(code, 200);
    assert!(page.contains("agent-inbox"));
    assert!(!page.contains(SECRET));
    assert!(!page.contains(&bootstrap));
    assert_eq!(request(address, "GET", "/api/agents", &[], "").0, 401);

    let origin = format!("http://{address}");
    assert_eq!(
        request(
            address,
            "POST",
            "/api/bootstrap",
            &[
                ("Origin", "http://elsewhere"),
                ("X-Gui-Bootstrap", &bootstrap)
            ],
            ""
        )
        .0,
        403
    );
    let (code, headers, body) = request(
        address,
        "POST",
        "/api/bootstrap",
        &[("Origin", &origin), ("X-Gui-Bootstrap", &bootstrap)],
        "",
    );
    assert_eq!(code, 200);
    assert_eq!(
        request(
            address,
            "POST",
            "/api/bootstrap",
            &[("Origin", &origin), ("X-Gui-Bootstrap", &bootstrap)],
            ""
        )
        .0,
        401
    );
    let cookie = headers
        .lines()
        .find_map(|line| line.strip_prefix("Set-Cookie: "))
        .unwrap()
        .split(';')
        .next()
        .unwrap();
    let csrf = serde_json::from_str::<serde_json::Value>(&body).unwrap()["csrf"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(!headers.contains(SECRET));
    let (code, _, body) = request(address, "GET", "/api/agents", &[("Cookie", cookie)], "");
    assert_eq!(code, 200);
    assert!(body.contains("alpha.worker"));
    let message = r#"{"to":"alpha.worker","message":"hello from gui"}"#;
    assert_eq!(
        request(
            address,
            "POST",
            "/api/send",
            &[
                ("Origin", &origin),
                ("Cookie", cookie),
                ("Content-Type", "application/json")
            ],
            message
        )
        .0,
        403
    );
    let (code, _, body) = request(
        address,
        "POST",
        "/api/send",
        &[
            ("Origin", &origin),
            ("Cookie", cookie),
            ("Content-Type", "application/json"),
            ("X-Csrf", &csrf),
        ],
        message,
    );
    assert_eq!(code, 202);
    assert!(body.contains("queued"));
    assert_eq!(
        received.recv_timeout(Duration::from_secs(3)).unwrap(),
        "hello from gui"
    );

    drop(gui);
    running.store(false, Ordering::Release);
    relay_thread.join().unwrap();
}
