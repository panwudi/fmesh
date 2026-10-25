#![cfg(unix)]

use std::fs::OpenOptions;
use std::io::{BufRead, BufReader};
use std::net::TcpListener;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture {
    root: PathBuf,
    ports: Vec<TcpListener>,
}
impl Fixture {
    fn new() -> Self {
        let root = PathBuf::from(format!(
            "/tmp/agent-inbox-deploy-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&root).unwrap();
        let ports: Vec<_> = (0..4)
            .map(|_| TcpListener::bind("127.0.0.1:0").unwrap())
            .collect();
        std::fs::write(root.join("key with spaces"), "fake fixture key").unwrap();
        std::fs::write(root.join("known_hosts"), "fake pinned host key").unwrap();
        std::fs::write(
            root.join("ssh_config"),
            "Host *\nProxyCommand /does/not/run\n",
        )
        .unwrap();
        std::fs::write(
            root.join("facts.txt"),
            "address 172.16.0.12\ngateway 172.16.0.1\n",
        )
        .unwrap();
        let output = Command::new(std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()))
            .arg("--edition=2024")
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/support/fake_ssh.rs"))
            .arg("-o")
            .arg(root.join("fake-ssh"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            Command::new(root.join("fake-ssh"))
                .arg("--warmup")
                .status()
                .unwrap()
                .success()
        );
        let config = format!(
            "INBOX_BIN='{}'\nINBOX_SSH_USER='fixture-user'\nINBOX_SSH_KEY='{}/key with spaces'\nINBOX_KNOWN_HOSTS='{}/known_hosts'\nINBOX_HOST_KEY_ALIAS='fixture-hub'\nINBOX_SSH_CONFIG='{}/ssh_config'\nINBOX_INTR_HOST='intranet.test'\nINBOX_INTR_PORT=22\nINBOX_PUBLIC_HOST='public.test'\nINBOX_PUBLIC_PORT=2222\nINBOX_INTR_PREFIX='10.20.110.0/23'\nINBOX_INTR_GATEWAY='10.20.110.1'\nINBOX_DIAL_LOCK_FILE='{}/relay.lock'\nINBOX_SSH_BIN='{}/fake-ssh'\nINBOX_NC_BIN='{}/fake-ssh'\nINBOX_LOCAL_FACTS_BIN='{}/fake-ssh'\nINBOX_LOCAL_INBOX_PORT={}\nINBOX_REVERSE_PORT={}\nINBOX_LOCAL_SEND_PORT={}\nINBOX_HUB_PORT={}\n",
            env!("CARGO_BIN_EXE_agent-inbox"),
            root.display(),
            root.display(),
            root.display(),
            root.display(),
            root.display(),
            root.display(),
            root.display(),
            ports[0].local_addr().unwrap().port(),
            ports[1].local_addr().unwrap().port(),
            ports[2].local_addr().unwrap().port(),
            ports[3].local_addr().unwrap().port()
        );
        std::fs::write(root.join("tunnel.env"), config).unwrap();
        Self { root, ports }
    }
    fn command(&self, script: &str, behavior: &str) -> Command {
        let mut command = Command::new("sh");
        command
            .arg(
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("deploy")
                    .join(script),
            )
            .arg("--config")
            .arg(self.root.join("tunnel.env"))
            .env("INBOX_FAKE_SSH_LOG", self.root.join("calls.log"))
            .env("INBOX_FAKE_SSH_BEHAVIOR", behavior)
            .env("INBOX_FAKE_REVERSE_BUSY", self.root.join("reverse.busy"))
            .env("INBOX_FAKE_DROP", self.root.join("drop.now"))
            .env("INBOX_FAKE_FACTS_FILE", self.root.join("facts.txt"));
        for name in [
            "INBOX_BACKOFF_INITIAL",
            "INBOX_BACKOFF_MAX",
            "INBOX_STABLE_RESET",
            "INBOX_DIAL_TIMEOUT",
            "INBOX_OUTER_CONNECT_TIMEOUT",
        ] {
            command.env_remove(name);
        }
        command
    }
    fn supervisor(&self, behavior: &str) -> Supervisor {
        let mut child = self
            .command("agent-inbox-tunnels.sh", behavior)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stderr = child.stderr.take().unwrap();
        let (sender, errors) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                let _ = sender.send(line);
            }
        });
        Supervisor(child, errors)
    }
    fn log(&self) -> String {
        std::fs::read_to_string(self.root.join("calls.log")).unwrap_or_default()
    }
    fn rows(&self, kind: &str) -> Vec<Vec<String>> {
        self.log()
            .lines()
            .filter(|line| line.starts_with(&format!("{kind}\t")))
            .map(|line| line.split('\t').map(str::to_owned).collect())
            .collect()
    }
    fn wait_rows(&self, kind: &str, count: usize, seconds: u64) -> Vec<Vec<String>> {
        let deadline = Instant::now() + Duration::from_secs(seconds);
        loop {
            let rows = self.rows(kind);
            if rows.len() >= count {
                return rows;
            }
            assert!(Instant::now() < deadline, "missing {kind} event");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    fn append(&self, settings: &str) {
        use std::io::Write;
        writeln!(
            OpenOptions::new()
                .append(true)
                .open(self.root.join("tunnel.env"))
                .unwrap(),
            "{settings}"
        )
        .unwrap();
    }
    fn share_lock(&self, first: &Self) {
        self.append(&format!(
            "INBOX_DIAL_LOCK_FILE='{}/relay.lock'",
            first.root.display()
        ));
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
struct Supervisor(Child, Receiver<String>);
impl Supervisor {
    fn wait_error(&self, text: &str) {
        loop {
            let line = self.1.recv_timeout(Duration::from_secs(5)).unwrap();
            if line.contains(text) {
                return;
            }
        }
    }
    fn stop(&mut self) {
        signal(self.0.id(), "TERM");
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = self.0.try_wait().unwrap() {
                assert!(status.success());
                break;
            }
            assert!(
                Instant::now() < deadline,
                "supervisor did not stop promptly"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    fn crash(&mut self) {
        signal(self.0.id(), "KILL");
        self.0.wait().unwrap();
    }
}
impl Drop for Supervisor {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            signal(self.0.id(), "TERM");
            let _ = self.0.wait();
        }
    }
}
fn signal(pid: u32, name: &str) {
    let _ = Command::new("kill")
        .args([&format!("-{name}"), &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}
fn alive(pid: &str) -> bool {
    Command::new("kill")
        .args(["-0", pid])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .success()
}
fn wait_dead(pid: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while alive(pid) {
        assert!(Instant::now() < deadline, "owned SSH survived");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn route_selection_uses_only_local_prefix_or_gateway_and_no_ssh_probe() {
    let fixture = Fixture::new();
    for (facts, host) in [
        ("address 10.20.110.12\n", "intranet.test\t22"),
        ("address 10.20.111.254\n", "intranet.test\t22"),
        ("address 10.20.112.1\n", "public.test\t2222"),
        (
            "address 172.16.0.12\ngateway 10.20.110.1\n",
            "intranet.test\t22",
        ),
        ("", "public.test\t2222"),
    ] {
        std::fs::write(fixture.root.join("facts.txt"), facts).unwrap();
        let output = fixture
            .command("hub-ssh-proxy.sh", "hold")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, b"raw transport ready");
        assert!(fixture.log().lines().last().unwrap().ends_with(host));
    }
    assert!(fixture.rows("dial").is_empty());
    assert!(fixture.rows("auth").is_empty());
    assert!(!fixture.log().contains("\t-z\t"));
}

#[test]
fn malformed_prefix_fails_without_starting_transport() {
    let fixture = Fixture::new();
    fixture.append("INBOX_INTR_PREFIX='10.20.110.0/33'");
    let output = fixture
        .command("hub-ssh-proxy.sh", "hold")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(fixture.rows("transport").is_empty());
    assert!(fixture.rows("dial").is_empty());
}

#[test]
fn aggressive_legacy_retry_settings_fail_before_a_dial() {
    for setting in [
        "INBOX_BACKOFF_INITIAL=2",
        "INBOX_BACKOFF_MAX=301",
        "INBOX_STABLE_RESET=60",
    ] {
        let fixture = Fixture::new();
        fixture.append(setting);
        let output = fixture
            .command("agent-inbox-tunnels.sh", "hold")
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("unsafe retry settings"));
        assert!(fixture.rows("dial").is_empty());
    }
}

#[test]
fn shared_flock_serializes_dials_and_releases_for_established_tunnels() {
    let first = Fixture::new();
    let second = Fixture::new();
    second.share_lock(&first);
    let mut one = first.supervisor("slow-auth");
    let dial = first.wait_rows("dial", 1, 5);
    let mut two = second.supervisor("hold");
    std::thread::sleep(Duration::from_millis(300));
    assert!(second.rows("dial").is_empty());
    let auth = first.wait_rows("auth", 1, 5);
    let next = second.wait_rows("dial", 1, 5);
    second.wait_rows("auth", 1, 5);
    first.wait_rows("reverse-ready", 1, 5);
    assert!(next[0][1].parse::<u128>().unwrap() >= auth[0][1].parse::<u128>().unwrap());
    assert!(alive(&dial[0][2]));
    assert!(alive(&next[0][2]));
    let log = first.log();
    assert!(log.contains("ProxyCommand=none"));
    assert!(log.contains("ConnectionAttempts=1"));
    assert!(log.contains("ConnectTimeout=45"));
    assert!(log.contains("ControlPersist=no"));
    assert!(log.contains("ForkAfterAuthentication=no"));
    assert!(log.contains("ProxyJump=none"));
    assert!(log.contains("ExitOnForwardFailure=yes"));
    assert!(log.contains("StrictHostKeyChecking=yes"));
    let ports: Vec<_> = first
        .ports
        .iter()
        .map(|p| p.local_addr().unwrap().port())
        .collect();
    assert!(log.contains(&format!(
        "\t-R\t127.0.0.1:{}:127.0.0.1:{}",
        ports[1], ports[0]
    )));
    assert!(log.contains(&format!(
        "\t-L\t127.0.0.1:{}:127.0.0.1:{}",
        ports[2], ports[3]
    )));
    one.stop();
    two.stop();
    for rows in [dial, next] {
        wait_dead(&rows[0][2]);
        let socket = rows[0].windows(2).find(|pair| pair[0] == "-S").unwrap();
        assert!(!Path::new(&socket[1]).parent().unwrap().exists());
    }
    assert_eq!(
        std::fs::metadata(first.root.join("relay.lock"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

#[test]
fn failed_dial_releases_shared_lock_without_a_fast_retry() {
    let first = Fixture::new();
    let second = Fixture::new();
    second.share_lock(&first);
    let mut one = first.supervisor("fail");
    first.wait_rows("dial", 1, 5);
    let mut two = second.supervisor("hold");
    second.wait_rows("auth", 1, 5);
    assert_eq!(first.rows("dial").len(), 1);
    let started = Instant::now();
    one.stop();
    assert!(started.elapsed() < Duration::from_secs(2));
    two.stop();
}

#[test]
fn crashed_supervisor_keeps_lock_until_its_guard_stops_the_ssh() {
    let first = Fixture::new();
    let second = Fixture::new();
    second.share_lock(&first);
    let mut one = first.supervisor("hang-auth");
    let dial = first.wait_rows("dial", 1, 5);
    one.crash();
    let mut two = second.supervisor("hold");
    second.wait_rows("auth", 1, 5);
    wait_dead(&dial[0][2]);
    wait_dead(&dial[0][3]);
    two.stop();
    let socket = dial[0].windows(2).find(|pair| pair[0] == "-S").unwrap();
    assert!(!Path::new(&socket[1]).parent().unwrap().exists());
}

#[test]
fn crashed_guard_is_cleaned_up_before_releasing_the_parent_lock() {
    let first = Fixture::new();
    let second = Fixture::new();
    second.share_lock(&first);
    let mut one = first.supervisor("hang-auth");
    let dial = first.wait_rows("dial", 1, 5);
    signal(dial[0][3].parse().unwrap(), "KILL");
    let mut two = second.supervisor("hold");
    second.wait_rows("auth", 1, 5);
    wait_dead(&dial[0][2]);
    one.stop();
    two.stop();
}

#[test]
fn stopping_a_supervisor_waiting_for_the_flock_does_not_dial() {
    let fixture = Fixture::new();
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(fixture.root.join("relay.lock"))
        .unwrap();
    lock.lock().unwrap();
    let mut supervisor = fixture.supervisor("hold");
    supervisor.wait_error("waiting for shared relay dial lock");
    supervisor.stop();
    assert!(fixture.rows("dial").is_empty());
}

#[test]
fn authentication_deadline_releases_the_lock_after_stopping_the_attempt() {
    let first = Fixture::new();
    let second = Fixture::new();
    second.share_lock(&first);
    first.append("INBOX_DIAL_TIMEOUT=1");
    let mut one = first.supervisor("hang-auth");
    let dial = first.wait_rows("dial", 1, 5);
    let mut two = second.supervisor("hold");
    second.wait_rows("auth", 1, 5);
    wait_dead(&dial[0][2]);
    one.stop();
    two.stop();
}

#[test]
fn gentle_retry_waits_with_jitter_and_reselects_from_fresh_local_facts() {
    let fixture = Fixture::new();
    std::fs::write(fixture.root.join("facts.txt"), "address 10.20.110.2\n").unwrap();
    let mut supervisor = fixture.supervisor("fail-then-hold");
    fixture.wait_rows("dial", 1, 5);
    std::fs::write(fixture.root.join("facts.txt"), "address 172.16.0.12\n").unwrap();
    let rows = fixture.wait_rows("dial", 2, 25);
    let delta = rows[1][1].parse::<u128>().unwrap() - rows[0][1].parse::<u128>().unwrap();
    assert!((11_900..=21_000).contains(&delta), "retry delta {delta}ms");
    assert!(rows[0].last().unwrap() == "intranet.test");
    assert!(rows[1].last().unwrap() == "public.test");
    assert_eq!(fixture.rows("dial").len(), 2);
    supervisor.stop();
    wait_dead(&rows[1][2]);
}

fn relay_fixture(fixture: &Fixture) -> Supervisor {
    relay_fixture_at(
        fixture,
        &fixture.ports[3].local_addr().unwrap().to_string(),
        "hold",
    )
}

fn relay_fixture_at(fixture: &Fixture, hub: &str, behavior: &str) -> Supervisor {
    let token = fixture.root.join("host.token");
    std::fs::write(&token, "fixture-relay-host-token-private").unwrap();
    std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o600)).unwrap();
    let config = fixture.root.join("relay.toml");
    std::fs::write(&config,format!("host='fixture'\nhub='{hub}'\nhost_token_file='host.token'\njournal='relay.md'\ncontrol_socket='control/relay.sock'\nstate_dir='state'\ntunnel_env='tunnel.env'\ntunnel_script='{}'\n",Path::new(env!("CARGO_MANIFEST_DIR")).join("deploy/agent-inbox-tunnels.sh").display())).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_agent-inbox"))
        .args(["relay", "--listen", "0", "--config"])
        .arg(config)
        .env("INBOX_FAKE_SSH_LOG", fixture.root.join("calls.log"))
        .env("INBOX_FAKE_SSH_BEHAVIOR", behavior)
        .env("INBOX_FAKE_REVERSE_BUSY", fixture.root.join("reverse.busy"))
        .env("INBOX_FAKE_DROP", fixture.root.join("drop.now"))
        .env("INBOX_FAKE_FACTS_FILE", fixture.root.join("facts.txt"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stderr = child.stderr.take().unwrap();
    let (sender, errors) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            let _ = sender.send(line);
        }
    });
    Supervisor(child, errors)
}

#[test]
fn relay_owns_one_supervised_tunnel_and_stops_it_on_exit() {
    let fixture = Fixture::new();
    let mut relay = relay_fixture(&fixture);
    relay.wait_error("listening on");
    let rows = fixture.wait_rows("auth", 1, 5);
    assert_eq!(fixture.rows("dial").len(), 1);
    relay.stop();
    wait_dead(&rows[0][2]);
    wait_dead(&rows[0][3]);
}

#[test]
fn relay_crash_is_detected_by_its_supervisor_parent_guard() {
    let fixture = Fixture::new();
    let mut relay = relay_fixture(&fixture);
    relay.wait_error("listening on");
    let rows = fixture.wait_rows("auth", 1, 5);
    let output = Command::new("ps")
        .args(["-o", "ppid=", "-p", &rows[0][3]])
        .output()
        .unwrap();
    let supervisor = String::from_utf8(output.stdout).unwrap().trim().to_owned();
    relay.crash();
    wait_dead(&rows[0][2]);
    wait_dead(&rows[0][3]);
    wait_dead(&supervisor);
    let socket = rows[0].windows(2).find(|pair| pair[0] == "-S").unwrap();
    assert!(!Path::new(&socket[1]).parent().unwrap().exists());
}

#[test]
fn lingering_reverse_port_keeps_master_and_directory_live_then_attaches_without_redial() {
    let mut fixture = Fixture::new();
    let hub_forward = fixture.ports[2].local_addr().unwrap().to_string();
    drop(fixture.ports.remove(2));
    fixture.append("INBOX_DIAL_TIMEOUT=1");
    std::fs::write(
        fixture.root.join("reverse.busy"),
        "previous sshd still owns -R",
    )
    .unwrap();
    let mut relay = relay_fixture_at(&fixture, &hub_forward, "serve-hub");
    relay.wait_error("directory waiting for tunnel");
    let busy = fixture.wait_rows("reverse-busy", 1, 5);
    fixture.wait_rows("report", 1, 5);
    relay.wait_error("directory hub connection restored");
    assert!(fixture.rows("reverse-ready").is_empty());
    assert_eq!(fixture.rows("dial").len(), 1);

    // A busy -R must not hold the authentication lock or block another session's dial.
    let other = Fixture::new();
    other.share_lock(&fixture);
    let mut second = other.supervisor("hold");
    other.wait_rows("auth", 1, 5);
    second.stop();
    // Keep the old reverse port across multiple requests, not just a single startup failure.
    fixture.wait_rows("reverse-busy", 2, 6);
    std::fs::remove_file(fixture.root.join("reverse.busy")).unwrap();
    let ready = fixture.wait_rows("reverse-ready", 1, 6);
    relay.wait_error("both loopback forwards ready on existing SSH");
    assert_eq!(ready[0][2], busy[0][2]);
    assert!(alive(&ready[0][2]));
    assert_eq!(fixture.rows("dial").len(), 1);
    assert_eq!(fixture.rows("auth").len(), 1);
    assert!(fixture.rows("initial-reverse-collision").is_empty());
    let reports = fixture.rows("report");
    assert!(reports[0][1].parse::<u128>().unwrap() < ready[0][1].parse::<u128>().unwrap());
    let requests = fixture.rows("forward");
    for pair in requests.windows(2) {
        let gap = pair[1][1].parse::<u128>().unwrap() - pair[0][1].parse::<u128>().unwrap();
        assert!((2400..=4500).contains(&gap), "forward retry gap {gap}ms");
    }
    assert!(
        relay
            .1
            .try_iter()
            .all(|line| !line.contains("directory waiting") && !line.contains("SSH ended"))
    );
    relay.stop();
    wait_dead(&ready[0][2]);
    wait_dead(&ready[0][3]);
}

#[test]
fn real_drop_reaps_owned_ssh_before_redial_and_stale_reverse_does_not_kill_replacement() {
    let fixture = Fixture::new();
    let mut supervisor = fixture.supervisor("hold");
    let first = fixture.wait_rows("reverse-ready", 1, 5);
    std::fs::write(
        fixture.root.join("reverse.busy"),
        "remote sshd still owns reverse port",
    )
    .unwrap();
    std::fs::write(fixture.root.join("drop.now"), "drop first SSH").unwrap();
    fixture.wait_rows("drop", 1, 5);
    wait_dead(&first[0][2]);
    std::fs::remove_file(fixture.root.join("drop.now")).unwrap();
    let replacement = fixture.wait_rows("reverse-busy", 1, 25);
    assert_ne!(first[0][2], replacement[0][2]);
    assert_eq!(fixture.rows("dial").len(), 2);
    std::fs::remove_file(fixture.root.join("reverse.busy")).unwrap();
    let ready = fixture.wait_rows("reverse-ready", 2, 6);
    assert_eq!(ready[1][2], replacement[0][2]);
    assert_eq!(fixture.rows("dial").len(), 2);
    supervisor.stop();
    wait_dead(&ready[1][2]);
}

#[test]
fn stopping_during_pending_multiplex_request_stops_both_children() {
    let fixture = Fixture::new();
    let mut supervisor = fixture.supervisor("hang-forward");
    let master = fixture.wait_rows("auth", 1, 5);
    let requests = fixture.wait_rows("forward", 1, 5);
    supervisor.stop();
    wait_dead(&requests[0][2]);
    wait_dead(&master[0][2]);
}
