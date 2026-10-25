#![cfg(unix)]
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

mod support;

struct Fixture {
    root: PathBuf,
    address: String,
    _hub: TcpListener,
}
impl Fixture {
    fn new() -> Self {
        let root =
            PathBuf::from("/tmp").join(format!("ai-desktop-{}", agent_inbox::state::id().unwrap()));
        fs::create_dir(&root).unwrap();
        fs::write(
            root.join("host.token"),
            "desktop-test-host-token-0000000000",
        )
        .unwrap();
        fs::set_permissions(root.join("host.token"), fs::Permissions::from_mode(0o600)).unwrap();
        let port = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = port.local_addr().unwrap().to_string();
        let hub = TcpListener::bind("127.0.0.1:0").unwrap();
        fs::write(root.join("relay.toml"),format!("host='desktop-test'\nhub='{}'\nhost_token_file='host.token'\nstate_dir='relay-state'\njournal='relay.md'\ncontrol_socket='control/relay.sock'\n",hub.local_addr().unwrap())).unwrap();
        fs::write(root.join("watch.toml"),format!("relay='{address}'\nstate_dir='watch-state'\nregistry_dir='watch-registry'\ncontrol_socket='control/relay.sock'\ncodex_bin='{}'\nsocket='missing-daemon.sock'\npoll_ms=100\nrpc_timeout_secs=1\nsnapshot_timeout_secs=1\n",support::fake_codex().display())).unwrap();
        fs::write(root.join("desktop.toml"),format!("state_dir='desktop-state'\n[relay]\nlisten='{address}'\nconfig='relay.toml'\nno_tunnel=true\n[codex_watch]\nconfig='watch.toml'\n")).unwrap();
        drop(port);
        Self {
            root,
            address,
            _hub: hub,
        }
    }
    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agent-inbox"));
        command
            .args(["daemon", "--config"])
            .arg(self.root.join("desktop.toml"));
        command
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

struct Daemon {
    child: Child,
    lines: Arc<Mutex<Vec<String>>>,
}
impl Daemon {
    fn start(f: &Fixture) -> Self {
        let mut child = f
            .command()
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stderr = child.stderr.take().unwrap();
        let lines = Arc::new(Mutex::new(Vec::new()));
        let log = lines.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                log.lock().unwrap().push(line);
            }
        });
        Self { child, lines }
    }
    fn pid(&self, name: &str) -> Option<u32> {
        let prefix = format!("agent-inbox: desktop {name} started (pid ");
        self.lines.lock().unwrap().iter().rev().find_map(|line| {
            line.strip_prefix(&prefix)
                .and_then(|pid| pid.strip_suffix(')'))
                .and_then(|pid| pid.parse().ok())
        })
    }
    fn watcher_ready_count(&self) -> usize {
        self.lines
            .lock()
            .unwrap()
            .iter()
            .filter(|line| line.contains("Codex watcher ready"))
            .count()
    }
    fn wait(&self, mut check: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while !check() {
            assert!(
                Instant::now() < deadline,
                "desktop wait failed: {:?}",
                self.lines.lock().unwrap()
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    }
    fn stop(&mut self, signal: i32) {
        if self.child.try_wait().unwrap().is_none() {
            unsafe {
                libc::kill(self.child.id() as libc::pid_t, signal);
            }
            let deadline = Instant::now() + Duration::from_secs(12);
            while self.child.try_wait().unwrap().is_none() {
                if Instant::now() >= deadline {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}
impl Drop for Daemon {
    fn drop(&mut self) {
        self.stop(libc::SIGTERM);
    }
}
fn alive(pid: u32) -> bool {
    if unsafe { libc::kill(pid as libc::pid_t, 0) } != 0 {
        return false;
    }
    let output = Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    let state = String::from_utf8_lossy(&output.stdout);
    !state.trim().is_empty() && !state.trim_start().starts_with('Z')
}
fn healthy(address: &str) -> bool {
    let Ok(mut stream) = TcpStream::connect(address) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_millis(300)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(300)));
    if stream
        .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .is_err()
    {
        return false;
    }
    let mut bytes = String::new();
    stream.take(512).read_to_string(&mut bytes).is_ok() && bytes.starts_with("HTTP/1.1 200 ")
}

#[test]
fn single_desktop_service_restarts_each_component_and_shuts_down_cleanly() {
    let f = Fixture::new();
    let mut daemon = Daemon::start(&f);
    daemon.wait(|| daemon.watcher_ready_count() == 1 && healthy(&f.address));
    let relay = daemon.pid("relay").unwrap();
    let watcher = daemon.pid("codex-watch").unwrap();
    let duplicate = f.command().output().unwrap();
    assert!(!duplicate.status.success());
    assert!(String::from_utf8_lossy(&duplicate.stderr).contains("another desktop daemon"));
    unsafe {
        libc::kill(relay as libc::pid_t, libc::SIGKILL);
    }
    daemon.wait(|| daemon.pid("relay").is_some_and(|pid| pid != relay) && healthy(&f.address));
    assert_eq!(daemon.pid("codex-watch"), Some(watcher));
    let relay = daemon.pid("relay").unwrap();
    unsafe {
        libc::kill(watcher as libc::pid_t, libc::SIGKILL);
    }
    daemon.wait(|| {
        daemon.pid("codex-watch").is_some_and(|pid| pid != watcher)
            && daemon.watcher_ready_count() == 2
    });
    assert_eq!(daemon.pid("relay"), Some(relay));
    let watcher = daemon.pid("codex-watch").unwrap();
    daemon.stop(libc::SIGTERM);
    daemon.wait(|| !alive(relay) && !alive(watcher));
    let mut again = Daemon::start(&f);
    again.wait(|| healthy(&f.address));
    again.stop(libc::SIGTERM);
}

#[test]
fn killed_desktop_parent_leaves_no_components_or_instance_locks() {
    let f = Fixture::new();
    let mut daemon = Daemon::start(&f);
    daemon.wait(|| daemon.watcher_ready_count() == 1 && healthy(&f.address));
    let relay = daemon.pid("relay").unwrap();
    let watcher = daemon.pid("codex-watch").unwrap();
    daemon.stop(libc::SIGKILL);
    daemon.wait(|| !alive(relay) && !alive(watcher));
    let mut again = Daemon::start(&f);
    again.wait(|| healthy(&f.address));
    again.stop(libc::SIGTERM);
}

#[test]
fn check_is_read_only_and_rejects_mismatched_or_nonlocal_components() {
    let f = Fixture::new();
    let output = f.command().arg("--check").output().unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("relay and Codex auto-join"));
    assert!(!f.root.join("desktop-state").exists());
    assert!(!f.root.join("relay-state").exists());
    assert!(!f.root.join("watch-state").exists());
    let path = f.root.join("watch.toml");
    let original = fs::read_to_string(&path).unwrap();
    fs::write(&path, original.replace(&f.address, "127.0.0.1:1")).unwrap();
    let output = f.command().arg("--check").output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("must use this desktop relay"));
    fs::write(&path, original).unwrap();
    let path = f.root.join("desktop.toml");
    let original = fs::read_to_string(&path).unwrap();
    fs::write(&path, original.replace(&f.address, "0.0.0.0:8831")).unwrap();
    assert!(
        !f.command()
            .arg("--check")
            .output()
            .unwrap()
            .status
            .success()
    );
}

#[test]
fn desktop_checks_optional_grok_watcher_against_the_same_relay() {
    let f = Fixture::new();
    let grok = f.root.join("grok.toml");
    let config = format!(
        "relay='{}'\ncontrol_socket='control/relay.sock'\nstate_dir='grok-state'\ngrok_bin='grok'\nleader_socket='missing-leader.sock'\n",
        f.address
    );
    fs::write(&grok, &config).unwrap();
    let desktop = f.root.join("desktop.toml");
    let mut text = fs::read_to_string(&desktop).unwrap();
    text.push_str("[grok_watch]\nconfig='grok.toml'\n");
    fs::write(&desktop, text).unwrap();
    let output = f.command().arg("--check").output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("Grok auto-join"));
    assert!(!f.root.join("grok-state").exists());
    fs::write(&grok, config.replace(&f.address, "127.0.0.1:1")).unwrap();
    let output = f.command().arg("--check").output().unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("Grok watcher must use this desktop relay")
    );
}

#[test]
fn claude_only_desktop_needs_no_codex_configuration() {
    let f = Fixture::new();
    let path = f.root.join("desktop.toml");
    let config = fs::read_to_string(&path)
        .unwrap()
        .split("[codex_watch]")
        .next()
        .unwrap()
        .to_owned();
    fs::write(&path, config).unwrap();
    fs::remove_file(f.root.join("watch.toml")).unwrap();
    assert!(
        f.command()
            .arg("--check")
            .output()
            .unwrap()
            .status
            .success()
    );
    let mut daemon = Daemon::start(&f);
    daemon.wait(|| healthy(&f.address));
    assert!(daemon.pid("codex-watch").is_none());
    daemon.stop(libc::SIGTERM);
}
