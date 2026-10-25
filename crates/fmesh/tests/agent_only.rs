//! Acceptance: Agent Mesh alone. `fmesh install agent` into an empty HOME, the daemon in the
//! foreground, two Claude sessions joining; one messages the other. No meshd, no root, no
//! service manager. Needs the workspace's agent-inbox binary (`cargo test --workspace`
//! builds it); every process gets a clean environment so nothing reaches a real relay or the
//! calling session.
#![cfg(unix)]

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

fn target_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_fmesh")).parent().unwrap().to_path_buf()
}

fn clean(cmd: &mut Command, home: &Path) {
    cmd.env_clear().env("HOME", home).env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin");
}

struct Kill(Vec<Child>);
impl Drop for Kill {
    fn drop(&mut self) {
        for c in &mut self.0 {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

struct Session {
    stdin: ChildStdin,
    lines: mpsc::Receiver<Value>,
    instructions: String,
}

fn session(bin: &Path, home: &Path, dir: &str, port: u16, procs: &mut Kill) -> Session {
    let cwd = home.join(dir);
    std::fs::create_dir(&cwd).unwrap();
    let mut cmd = Command::new(bin);
    clean(&mut cmd, home);
    let mut child = cmd
        .args(["claude", "--relay", &format!("127.0.0.1:{port}")])
        .current_dir(&cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    procs.0.push(child);
    let (tx, lines) = mpsc::channel();
    std::thread::spawn(move || {
        for l in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Ok(v) = serde_json::from_str::<Value>(&l) {
                let _ = tx.send(v);
            }
        }
    });
    writeln!(stdin, "{}", json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}})).unwrap();
    let instructions = loop {
        let v = lines.recv_timeout(Duration::from_secs(10)).expect("initialize answer");
        if v["id"] == 1 {
            break v["result"]["instructions"].as_str().unwrap().to_owned();
        }
    };
    writeln!(stdin, "{}", json!({"jsonrpc":"2.0","method":"notifications/initialized"})).unwrap();
    Session { stdin, lines, instructions }
}

#[test]
fn agent_mesh_works_alone_on_one_machine() {
    let target = target_dir();
    assert!(target.join("agent-inbox").exists(), "build agent-inbox first (cargo test --workspace)");
    // Short path: Unix socket paths are limited to about 100 bytes.
    let home = PathBuf::from(format!("/tmp/fma-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir(&home).unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let mut procs = Kill(vec![]);

    let mut install = Command::new(env!("CARGO_BIN_EXE_fmesh"));
    clean(&mut install, &home);
    let st = install
        .args(["install", "agent", "--host", "testbox", "--port", &port.to_string(), "--no-service", "--from"])
        .arg(&target)
        .stdout(Stdio::null())
        .status()
        .unwrap();
    assert!(st.success());
    let bin = home.join(".local/bin/agent-inbox");
    let fmesh = home.join(".local/bin/fmesh");

    let mut daemon = Command::new(&bin);
    clean(&mut daemon, &home);
    procs.0.push(
        daemon
            .args(["daemon", "--config"])
            .arg(home.join(".config/agent-inbox/daemon.toml"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(Instant::now() < deadline, "the relay did not start");
        std::thread::sleep(Duration::from_millis(100));
    }

    let alpha = session(&bin, &home, "alpha", port, &mut procs);
    let mut beta = session(&bin, &home, "beta", port, &mut procs);

    let deadline = Instant::now() + Duration::from_secs(10);
    let names = loop {
        let mut agents = Command::new(&fmesh);
        clean(&mut agents, &home);
        let out = agents.args(["agents", "--json"]).output().unwrap();
        let v: Value = serde_json::from_slice(&out.stdout).unwrap();
        let names: Vec<String> = v["agents"].as_array().unwrap().iter().map(|a| a["name"].as_str().unwrap().to_owned()).collect();
        if names.len() >= 2 || Instant::now() > deadline {
            break names;
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    assert!(names.contains(&"testbox.alpha-claude".into()) && names.contains(&"testbox.beta-claude".into()), "{names:?}");

    // alpha replies the way its instructions say, with its own sender token.
    let token = alpha.instructions.split("--token-file '").nth(1).and_then(|r| r.split('\'').next()).expect("reply recipe").to_owned();
    let mut send = Command::new(&bin);
    clean(&mut send, &home);
    let st = send
        .args(["send", "--hub", &format!("127.0.0.1:{port}"), "--token-file", &token, "--to", "testbox.beta-claude", "hello from alpha"])
        .stdout(Stdio::null())
        .status()
        .unwrap();
    assert!(st.success());
    let deadline = Instant::now() + Duration::from_secs(10);
    let got = loop {
        let v = beta.lines.recv_timeout(Duration::from_secs(10)).expect("beta got nothing");
        if v["method"] == "notifications/claude/channel" && v["params"]["content"].as_str().is_some_and(|c| c.contains("hello from alpha")) {
            break v;
        }
        assert!(Instant::now() < deadline);
    };
    assert_eq!(got["params"]["meta"]["from"], "testbox.alpha-claude");
    let _ = beta.stdin.flush();
    drop(procs);
    let _ = std::fs::remove_dir_all(&home);
}
