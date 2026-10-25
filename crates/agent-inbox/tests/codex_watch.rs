#![cfg(unix)]
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::{Duration, Instant};

use agent_inbox::{codex_watch, http, state, token::Token};
use serde_json::{Value, json};
use tungstenite::Message;
mod support;
static NEXT: AtomicU64 = AtomicU64::new(0);
const HOST: &str = "watch-host-token-00000000000000000000";

struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let root = PathBuf::from(format!(
            "/tmp/ai-watch-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("host.token"), HOST).unwrap();
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
    let deadline = Instant::now() + Duration::from_secs(15);
    while !check() {
        assert!(Instant::now() < deadline, "condition timed out");
        std::thread::sleep(Duration::from_millis(25));
    }
}
fn stop(child: &mut Child, signal: libc::c_int) {
    if child.try_wait().unwrap().is_some() {
        return;
    }
    unsafe {
        libc::kill(child.id() as libc::pid_t, signal);
    }
    let deadline = Instant::now() + Duration::from_secs(4);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
struct Process {
    child: Child,
    address: String,
    errors: Arc<Mutex<String>>,
}
impl Process {
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
        let (sender, ready) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                capture.lock().unwrap().push_str(&format!("{line}\n"));
                if let Some(addr) = line.strip_prefix("agent-inbox: listening on http://") {
                    let _ = sender.send(addr.trim_end_matches('/').to_owned());
                }
            }
        });
        let address = ready
            .recv_timeout(Duration::from_secs(90))
            .unwrap_or_else(|_| {
                stop(&mut child, libc::SIGKILL);
                panic!("fake relay/hub failed to start: {}", errors.lock().unwrap());
            });
        Self {
            child,
            address,
            errors,
        }
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        stop(&mut self.child, libc::SIGTERM);
    }
}

#[derive(Default)]
struct Model {
    rows: BTreeMap<String, Value>,
    loaded: Vec<String>,
    mode: String,
    requests: Vec<Value>,
    deliveries: Vec<Value>,
    history: Vec<Value>,
    lose_ack: bool,
}
impl Model {
    fn add(&mut self, id: &str, source: Value, status: &str) {
        self.rows.insert(id.into(),json!({"id":id,"sessionId":"shared-tree-id-not-a-target","cwd":"/a/nonexistent/shared-project","source":source,"status":{"type":status}}));
        self.loaded.push(id.into());
    }
}
struct Backend {
    model: Arc<Mutex<Model>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    path: PathBuf,
}
impl Backend {
    fn start(root: &Path) -> Self {
        let path = root.join("server.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let model = Arc::new(Mutex::new(Model::default()));
        let state = Arc::clone(&model);
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let thread = std::thread::spawn(move || {
            let mut workers = Vec::new();
            for stream in listener.incoming() {
                if stopping.load(Ordering::Relaxed) {
                    break;
                }
                let model = Arc::clone(&state);
                workers.push(std::thread::spawn(move || serve(stream.unwrap(), &model)));
            }
            for worker in workers {
                worker.join().unwrap();
            }
        });
        Self {
            model,
            stop,
            thread: Some(thread),
            path,
        }
    }
}
impl Drop for Backend {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = UnixStream::connect(&self.path);
        self.thread.take().unwrap().join().unwrap();
    }
}

fn serve(stream: UnixStream, model: &Mutex<Model>) {
    stream
        .set_read_timeout(Some(Duration::from_secs(4)))
        .unwrap();
    let Ok(mut socket) = tungstenite::accept(stream) else {
        return;
    };
    while let Ok(Message::Text(text)) = socket.read() {
        let request: Value = serde_json::from_str(&text).unwrap();
        let mut state = model.lock().unwrap();
        state.requests.push(request.clone());
        let method = request["method"].as_str().unwrap();
        if method == "initialized" {
            continue;
        }
        if state.mode == "draining" {
            drop(state);
            let _ = socket.send(Message::Text(
                json!({"id":request["id"],"error":{"code":-32600,"message":"Server is draining"}})
                    .to_string()
                    .into(),
            ));
            continue;
        }
        if ["rejectDelivery", "rejectOnce"].contains(&state.mode.as_str())
            && ["turn/start", "turn/steer"].contains(&method)
        {
            if state.mode == "rejectOnce" {
                state.mode.clear();
            }
            drop(state);
            let _ = socket.send(Message::Text(
                json!({"id":request["id"],"error":{"code":-32602,"message":"Unsupported request"}})
                    .to_string()
                    .into(),
            ));
            continue;
        }
        let result = match method {
            "initialize" => {
                json!({"userAgent":"fake/0.162.1","platformFamily":"unix","platformOs":"macos","codexHome":"/fake"})
            }
            "thread/loaded/list" => {
                if state.mode == "badCursor" {
                    json!({"data":[],"nextCursor":42})
                } else {
                    let index = request["params"]["cursor"]
                        .as_str()
                        .map(|s| s.strip_prefix("page-").unwrap().parse::<usize>().unwrap())
                        .unwrap_or(0);
                    if let Some(id) = state.loaded.get(index) {
                        json!({"data":[id],"nextCursor":if index+1<state.loaded.len() {Some(format!("page-{}",index+1))} else {None}})
                    } else {
                        json!({"data":[]})
                    }
                }
            }
            "thread/read" => {
                let id = request["params"]["threadId"].as_str().unwrap();
                let mut row = state.rows[id].clone();
                if state.mode == "badIdentity" {
                    row["id"] = json!("a-different-thread");
                }
                if state.mode == "badSource" {
                    row["source"] = json!("future-unknown-source");
                }
                if state.mode == "notLoadedRace" {
                    row["status"]["type"] = json!("notLoaded");
                }
                json!({"thread":row})
            }
            "thread/turns/list" => json!({"data":[{"id":"active-turn","status":"inProgress"}]}),
            "thread/items/list" => {
                let id = &request["params"]["threadId"];
                json!({"data":state.history.iter().filter(|h|h["thread"] == *id).map(|h|json!({"turnId":"recorded-turn","item":{"type":"userMessage","id":"history-item","clientId":h["clientId"]}})).collect::<Vec<_>>(),"nextCursor":null})
            }
            "turn/start" | "turn/steer" => {
                state.deliveries.push(request.clone());
                state.history.push(json!({"thread":request["params"]["threadId"],"clientId":request["params"]["clientUserMessageId"]}));
                if state.lose_ack {
                    state.lose_ack = false;
                    return;
                }
                if method == "turn/start" {
                    json!({"turn":{"id":"recorded-turn"}})
                } else {
                    json!({"turnId":"active-turn"})
                }
            }
            other => panic!("unexpected read-only/delivery request {other}"),
        };
        drop(state);
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
fn blocked_auto_join_delivery_alerts_the_codex_thread_directly() {
    let mut setup = Setup::new();
    setup
        .backend
        .model
        .lock()
        .unwrap()
        .add("blocked-alert-thread", json!("cli"), "active");
    setup.start();
    let session = setup.live("blocked-alert-thread");
    setup.backend.model.lock().unwrap().mode = "rejectOnce".into();
    setup.send(&session, "original rejected message");
    wait(|| {
        setup
            .status(&session)
            .is_some_and(|status| status["pending"][0]["phase"] == "blocked")
    });
    wait(|| setup.backend.model.lock().unwrap().deliveries.len() == 1);
    let model = setup.backend.model.lock().unwrap();
    let alert = &model.deliveries[0];
    assert_eq!(alert["method"], "turn/steer");
    let text = alert["params"]["input"][0]["text"].as_str().unwrap();
    assert!(text.contains("FIFO blocks later messages"));
    assert!(text.contains("--resend"));
    assert!(!text.contains("original rejected message"));
}

struct Watcher {
    child: Child,
    errors: Arc<Mutex<String>>,
}

struct JoinedInbox(Child);
impl Drop for JoinedInbox {
    fn drop(&mut self) {
        stop(&mut self.0, libc::SIGTERM);
    }
}
impl Watcher {
    fn start(root: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_agent-inbox"))
            .arg("codex-watch")
            .arg("--config")
            .arg(root.join("watch.toml"))
            .env("CODEX_THREAD_ID", "wrong-inherited-thread-never-use")
            .env("AGENT_INBOX_TEST_DAEMON", root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stderr = child.stderr.take().unwrap();
        let errors = Arc::new(Mutex::new(String::new()));
        let capture = Arc::clone(&errors);
        let (sender, ready) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                capture.lock().unwrap().push_str(&format!("{line}\n"));
                if line.contains("Codex watcher ready") {
                    let _ = sender.send(());
                }
            }
        });
        ready
            .recv_timeout(Duration::from_secs(90))
            .unwrap_or_else(|error| {
                stop(&mut child, libc::SIGKILL);
                panic!(
                    "Codex watcher did not become ready: {error}; {}",
                    errors.lock().unwrap()
                );
            });
        Self { child, errors }
    }
}
impl Drop for Watcher {
    fn drop(&mut self) {
        stop(&mut self.child, libc::SIGTERM);
    }
}

struct Setup {
    watcher: Option<Watcher>,
    relay: Process,
    hub: Process,
    backend: Backend,
    temp: Temp,
}
impl Setup {
    fn new() -> Self {
        let temp = Temp::new();
        let backend = Backend::start(&temp.0);
        let reserved = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = reserved.local_addr().unwrap().to_string();
        std::fs::write(temp.0.join("hub.toml"),format!("journal='hub.md'\n[[hosts]]\nname='testhost'\nrelay='{addr}'\ntoken_file='host.token'\n")).unwrap();
        let hub = Process::start(vec![
            "hub".into(),
            "--listen".into(),
            "0".into(),
            "--config".into(),
            temp.0.join("hub.toml").display().to_string(),
        ]);
        std::fs::write(temp.0.join("relay.toml"),format!("host='testhost'\nhub='{}'\nhost_token_file='host.token'\ncontrol_socket='control/relay.sock'\njournal='relay.md'\nsession_ttl_secs=3\ndirectory_report_secs=1\n",hub.address)).unwrap();
        drop(reserved);
        let relay = Process::start(vec![
            "relay".into(),
            "--listen".into(),
            addr,
            "--config".into(),
            temp.0.join("relay.toml").display().to_string(),
            "--state-dir".into(),
            temp.0.join("relay-state").display().to_string(),
            "--no-tunnel".into(),
        ]);
        std::fs::write(temp.0.join("watch.toml"),format!("relay='{}'\ncontrol_socket='control/relay.sock'\nstate_dir='watch-state'\nregistry_dir='registry'\ncodex_bin={:?}\nsocket='server.sock'\npoll_ms=100\nrpc_timeout_secs=1\nsnapshot_timeout_secs=3\n",relay.address,support::fake_codex())).unwrap();
        Self {
            watcher: None,
            relay,
            hub,
            backend,
            temp,
        }
    }
    fn start(&mut self) {
        self.watcher = Some(Watcher::start(&self.temp.0));
    }
    fn path(&self, id: &str) -> PathBuf {
        self.temp
            .0
            .join("watch-state/threads")
            .join(codex_watch::key(id).unwrap())
            .join("session.json")
    }
    fn presence(&self, id: &str) -> Option<Value> {
        agent_inbox::codex_presence::read(&self.path(id), id).ok()
    }
    fn live(&self, id: &str) -> Value {
        wait(|| {
            self.presence(id)
                .is_some_and(|v| v["profile"]["registered"] == true)
        });
        let value = self.presence(id).unwrap();
        wait(|| self.status(&value).is_some());
        value
    }
    fn status(&self, presence: &Value) -> Option<Value> {
        let token = Token::read(Path::new(presence["tokenFile"].as_str()?)).ok()?;
        let (status, body) =
            http::post_response(presence["inbox"].as_str()?, "/control/status", &token, "{}")
                .ok()?;
        if status != 200 {
            return None;
        }
        serde_json::from_str(&body).ok()
    }
    fn send(&self, presence: &Value, text: &str) {
        assert_eq!(
            http::post(
                &self.hub.address,
                &format!("/to/{}", presence["profile"]["name"].as_str().unwrap()),
                &Token::read(&self.temp.0.join("host.token")).unwrap(),
                Some("testhost.reviewer"),
                text,
                Duration::from_secs(3)
            )
            .unwrap(),
            202
        );
    }
    fn directory(&self) -> Value {
        let (_, body) = http::post_response_from(
            &self.hub.address,
            "/directory/list",
            &Token::read(&self.temp.0.join("host.token")).unwrap(),
            Some("testhost.relay"),
            "{}",
            Duration::from_secs(2),
        )
        .unwrap();
        serde_json::from_str(&body).unwrap()
    }
}

#[test]
fn discovers_exact_ids_same_cwd_sources_and_routes_start_or_steer() {
    let mut setup = Setup::new();
    {
        let mut m = setup.backend.model.lock().unwrap();
        m.add("a-thread", json!("cli"), "idle");
        m.add("b-thread", json!("vscode"), "active");
        m.add("c-thread", json!("appServer"), "idle");
        m.add("exec-thread", json!("exec"), "idle");
        m.add(
            "sub-thread",
            json!({"subAgent":{"thread_spawn":{}}}),
            "idle",
        );
    }
    setup.start();
    let a = setup.live("a-thread");
    let b = setup.live("b-thread");
    let c = setup.live("c-thread");
    assert_ne!(a["inbox"], b["inbox"]);
    assert_ne!(a["profile"]["name"], b["profile"]["name"]);
    assert_ne!(b["profile"]["name"], c["profile"]["name"]);
    let names: std::collections::BTreeSet<_> = [&a, &b, &c]
        .map(|value| value["profile"]["name"].as_str().unwrap())
        .into_iter()
        .collect();
    assert_eq!(
        names,
        [
            "testhost.shared-project-codex",
            "testhost.shared-project-codex-2",
            "testhost.shared-project-codex-3"
        ]
        .into_iter()
        .collect()
    );
    assert!(setup.presence("exec-thread").is_none() && setup.presence("sub-thread").is_none());
    assert!(setup.backend.model.lock().unwrap().deliveries.is_empty());
    setup.send(&a, "first idle");
    setup.send(&a, "second idle");
    setup.send(&b, "active steer");
    wait(|| setup.backend.model.lock().unwrap().deliveries.len() == 3);
    let m = setup.backend.model.lock().unwrap();
    let a_calls: Vec<_> = m
        .deliveries
        .iter()
        .filter(|r| r["params"]["threadId"] == "a-thread")
        .collect();
    assert_eq!(a_calls.len(), 2);
    assert!(
        a_calls[0]["params"]["input"][0]["text"]
            .as_str()
            .unwrap()
            .ends_with("first idle")
    );
    assert!(
        a_calls[1]["params"]["input"][0]["text"]
            .as_str()
            .unwrap()
            .ends_with("second idle")
    );
    assert_eq!(a_calls[0]["method"], "turn/start");
    assert!(
        m.deliveries
            .iter()
            .any(|r| r["method"] == "turn/steer" && r["params"]["threadId"] == "b-thread")
    );
    assert!(
        a_calls[0]["params"]["input"][0]["text"]
            .as_str()
            .unwrap()
            .contains("codex-self")
    );
    assert!(
        m.requests
            .iter()
            .filter(|r| r["method"] == "thread/read")
            .any(|r| r["params"]["includeTurns"] == false)
    );
    assert!(!m.requests.iter().any(|r| r["method"] == "thread/resume"
        || r["params"]["threadId"] == "wrong-inherited-thread-never-use"));
    drop(m);
    let output = Command::new(env!("CARGO_BIN_EXE_agent-inbox"))
        .arg("codex-self")
        .args(["--thread", "a-thread", "--state-dir"])
        .arg(setup.temp.0.join("watch-state"))
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap()["thread"],
        "a-thread"
    );
    wait(|| setup.directory()["agents"].as_array().unwrap().len() == 3);
    let secrets = [
        std::fs::read_to_string(a["tokenFile"].as_str().unwrap()).unwrap(),
        std::fs::read_to_string(a["senderTokenFile"].as_str().unwrap()).unwrap(),
        HOST.into(),
    ];
    for secret in secrets {
        let secret = secret.trim();
        assert!(!String::from_utf8_lossy(&output.stdout).contains(secret));
        assert!(
            !setup
                .backend
                .model
                .lock()
                .unwrap()
                .deliveries
                .iter()
                .any(|r| r.to_string().contains(secret))
        );
        assert!(
            !setup
                .watcher
                .as_ref()
                .unwrap()
                .errors
                .lock()
                .unwrap()
                .contains(secret)
        );
        assert!(!setup.relay.errors.lock().unwrap().contains(secret));
    }
}

#[test]
fn same_directory_pairs_have_distinct_type_tags_and_explicit_worker_names_are_kept() {
    let mut setup = Setup::new();
    let cwd = setup.temp.0.join("my-project");
    std::fs::create_dir(&cwd).unwrap();
    {
        let mut model = setup.backend.model.lock().unwrap();
        for id in ["pair-a", "pair-b", "pair-c", "explicit-worker"] {
            model.add(
                id,
                json!(if id == "explicit-worker" {
                    "exec"
                } else {
                    "cli"
                }),
                "idle",
            );
            model.rows.get_mut(id).unwrap()["cwd"] = json!(cwd);
        }
    }
    let mut claudes = Vec::new();
    for number in 1..=3 {
        claudes.push(JoinedInbox(
            Command::new(env!("CARGO_BIN_EXE_agent-inbox"))
                .args([
                    "claude",
                    "--relay",
                    &setup.relay.address,
                    "--control-socket",
                ])
                .arg(setup.temp.0.join("control/relay.sock"))
                .arg("--session-dir")
                .arg(setup.temp.0.join("claude-credentials"))
                .arg("--log")
                .arg(setup.temp.0.join("claude.md"))
                .current_dir(&cwd)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        ));
        wait(|| setup.directory()["agents"].as_array().unwrap().len() == number);
    }
    setup.start();
    let a = setup.live("pair-a");
    setup.live("pair-b");
    setup.live("pair-c");
    let status = setup.path("explicit-worker");
    let directory = status.parent().unwrap();
    agent_inbox::local_control::private_dir(directory).unwrap();
    let explicit = JoinedInbox(
        Command::new(env!("CARGO_BIN_EXE_agent-inbox"))
            .args([
                "codex-worker",
                "--thread",
                "explicit-worker",
                "--name",
                "operator-chosen",
                "--relay",
                &setup.relay.address,
            ])
            .arg("--control-socket")
            .arg(setup.temp.0.join("control/relay.sock"))
            .arg("--queue-file")
            .arg(directory.join("queue.json"))
            .arg("--log")
            .arg(directory.join("messages.md"))
            .arg("--status-file")
            .arg(&status)
            .arg("--session-dir")
            .arg(setup.temp.0.join("manual-credentials"))
            .arg("--registry-dir")
            .arg(setup.temp.0.join("registry"))
            .arg("--codex-bin")
            .arg(support::fake_codex())
            .arg("--sock")
            .arg(setup.temp.0.join("server.sock"))
            .env("AGENT_INBOX_TEST_DAEMON", &setup.temp.0)
            .current_dir(&cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let overridden = setup.live("explicit-worker");
    assert_eq!(overridden["profile"]["name"], "testhost.operator-chosen");
    let expected: std::collections::BTreeSet<_> = [
        "testhost.my-project-claude",
        "testhost.my-project-claude-2",
        "testhost.my-project-claude-3",
        "testhost.my-project-codex",
        "testhost.my-project-codex-2",
        "testhost.my-project-codex-3",
        "testhost.operator-chosen",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    wait(|| {
        setup.directory()["agents"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["name"].as_str().unwrap().to_owned())
            .collect::<std::collections::BTreeSet<_>>()
            == expected
    });
    setup.send(&a, "default routes to exact thread");
    setup.send(&overridden, "explicit override preserved");
    wait(|| setup.backend.model.lock().unwrap().deliveries.len() == 2);
    let delivered: std::collections::BTreeSet<_> = setup
        .backend
        .model
        .lock()
        .unwrap()
        .deliveries
        .iter()
        .map(|call| call["params"]["threadId"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        delivered,
        ["pair-a".to_owned(), "explicit-worker".to_owned()]
            .into_iter()
            .collect()
    );
    drop(explicit);
    drop(claudes);
}

#[test]
fn errors_and_incomplete_snapshots_retain_children_and_fifo_until_valid_absence() {
    let mut setup = Setup::new();
    setup
        .backend
        .model
        .lock()
        .unwrap()
        .add("retained", json!("cli"), "idle");
    setup.start();
    let first = setup.live("retained");
    for mode in [
        "badCursor",
        "badIdentity",
        "badSource",
        "notLoadedRace",
        "draining",
    ] {
        let count = {
            let mut m = setup.backend.model.lock().unwrap();
            m.mode = mode.into();
            m.requests.len()
        };
        wait(|| setup.backend.model.lock().unwrap().requests.len() > count + 3);
        assert_eq!(setup.live("retained")["processId"], first["processId"]);
    }
    setup.send(&first, "held through draining one");
    setup.send(&first, "held through draining two");
    wait(|| {
        setup
            .status(&first)
            .is_some_and(|s| s["pending"].as_array().unwrap().len() == 2)
    });
    assert!(setup.backend.model.lock().unwrap().deliveries.is_empty());
    setup.backend.model.lock().unwrap().mode.clear();
    wait(|| setup.backend.model.lock().unwrap().deliveries.len() == 2);
    assert_eq!(setup.live("retained")["processId"], first["processId"]);
    setup.backend.model.lock().unwrap().loaded.clear();
    wait(|| setup.presence("retained").is_none());
    wait(|| setup.directory()["agents"].as_array().unwrap().is_empty());
    assert!(
        setup
            .temp
            .0
            .join("watch-state/threads")
            .join(codex_watch::key("retained").unwrap())
            .join("queue.json")
            .exists()
    );
}

#[test]
fn lost_ack_and_worker_restart_reconcile_without_double_delivery() {
    let mut setup = Setup::new();
    {
        let mut m = setup.backend.model.lock().unwrap();
        m.add("ack-thread", json!("cli"), "idle");
        m.lose_ack = true;
    }
    setup.start();
    let first = setup.live("ack-thread");
    setup.send(&first, "acknowledgement will be lost");
    wait(|| {
        setup
            .status(&first)
            .is_some_and(|s| s["pending"][0]["phase"] == "unconfirmed")
    });
    let id = setup.status(&first).unwrap()["pending"][0]["id"].clone();
    unsafe {
        libc::kill(
            first["processId"].as_u64().unwrap() as libc::pid_t,
            libc::SIGKILL,
        );
    }
    wait(|| {
        setup.presence("ack-thread").is_some_and(|v| {
            v["processId"] != first["processId"] && v["profile"]["registered"] == true
        })
    });
    let next = setup.live("ack-thread");
    wait(|| {
        setup
            .status(&next)
            .is_some_and(|s| s["receipts"].as_array().unwrap().len() == 1)
    });
    assert_eq!(setup.status(&next).unwrap()["receipts"][0]["id"], id);
    let model = setup.backend.model.lock().unwrap();
    assert_eq!(
        model
            .deliveries
            .iter()
            .filter(|request| {
                request["params"]["input"][0]["text"]
                    .as_str()
                    .is_some_and(|text| text.contains("acknowledgement will be lost"))
            })
            .count(),
        1
    );
}

#[test]
fn watcher_crash_stops_owned_children_and_restart_keeps_pending_ids() {
    let mut setup = Setup::new();
    setup
        .backend
        .model
        .lock()
        .unwrap()
        .add("restart-thread", json!("cli"), "idle");
    setup.start();
    let first = setup.live("restart-thread");
    setup.backend.model.lock().unwrap().mode = "draining".into();
    setup.send(&first, "survives watcher crash");
    wait(|| {
        setup
            .status(&first)
            .is_some_and(|s| s["pending"].as_array().unwrap().len() == 1)
    });
    let id = setup.status(&first).unwrap()["pending"][0]["id"].clone();
    stop(&mut setup.watcher.as_mut().unwrap().child, libc::SIGKILL);
    wait(|| setup.presence("restart-thread").is_none());
    wait(|| setup.directory()["agents"].as_array().unwrap().is_empty());
    drop(setup.watcher.take());
    setup.backend.model.lock().unwrap().mode.clear();
    setup.start();
    let next = setup.live("restart-thread");
    wait(|| {
        setup
            .status(&next)
            .is_some_and(|s| s["receipts"].as_array().unwrap().len() == 1)
    });
    assert_eq!(setup.status(&next).unwrap()["receipts"][0]["id"], id);
    assert_eq!(setup.backend.model.lock().unwrap().deliveries.len(), 1);
}

#[test]
fn auto_inbox_inherits_upgrade_hold_and_another_watcher_is_rejected() {
    let mut setup = Setup::new();
    let registry = setup.temp.0.join("registry");
    state::parent(&registry.join("hold.json")).unwrap();
    let lease = state::id().unwrap();
    state::write_json(
        &registry.join("hold.json"),
        &json!({"lease":lease,"expires":chrono::Utc::now().timestamp()+120}),
    )
    .unwrap();
    setup
        .backend
        .model
        .lock()
        .unwrap()
        .add("held-thread", json!("cli"), "idle");
    setup.start();
    let first = setup.live("held-thread");
    setup.send(&first, "held for safe upgrade");
    assert_eq!(setup.status(&first).unwrap()["held"], true);
    assert!(setup.backend.model.lock().unwrap().deliveries.is_empty());
    let message_id = setup.status(&first).unwrap()["pending"][0]["id"].clone();
    setup.backend.model.lock().unwrap().loaded.clear();
    wait(|| setup.presence("held-thread").is_none());
    setup
        .backend
        .model
        .lock()
        .unwrap()
        .loaded
        .push("held-thread".into());
    let first = setup.live("held-thread");
    assert_eq!(setup.status(&first).unwrap()["held"], true);
    assert_eq!(
        setup.status(&first).unwrap()["pending"][0]["id"],
        message_id
    );
    let output = Command::new(env!("CARGO_BIN_EXE_agent-inbox"))
        .arg("codex-watch")
        .arg("--config")
        .arg(setup.temp.0.join("watch.toml"))
        .output()
        .unwrap();
    assert!(!output.status.success());
    let token = Token::read(Path::new(first["tokenFile"].as_str().unwrap())).unwrap();
    let status = setup.status(&first).unwrap();
    assert_eq!(
        http::post_response(
            first["inbox"].as_str().unwrap(),
            "/control/release",
            &token,
            &json!({"instance":status["instance"],"lease":lease}).to_string()
        )
        .unwrap()
        .0,
        200
    );
    wait(|| setup.backend.model.lock().unwrap().deliveries.len() == 1);
}

#[test]
fn daemon_version_recovery_and_socket_change_restart_inbox_with_the_same_queue() {
    let mut setup = Setup::new();
    setup
        .backend
        .model
        .lock()
        .unwrap()
        .add("moved-thread", json!("cli"), "idle");
    let config = setup.temp.0.join("watch.toml");
    let text = std::fs::read_to_string(&config).unwrap();
    std::fs::write(&config, text.replace("socket='server.sock'\n", "")).unwrap();
    let version = setup.temp.0.join("before.json");
    state::write_json(
        &version,
        &json!({"status":"running","socketPath":setup.backend.path}),
    )
    .unwrap();
    setup.start();
    let first = setup.live("moved-thread");
    setup.backend.model.lock().unwrap().mode = "draining".into();
    state::write_json(&version, &json!({"status":"stopped"})).unwrap();
    setup.send(&first, "follows the daemon socket");
    wait(|| {
        setup
            .status(&first)
            .is_some_and(|s| s["pending"].as_array().unwrap().len() == 1)
    });
    let id = setup.status(&first).unwrap()["pending"][0]["id"].clone();
    assert_eq!(setup.live("moved-thread")["processId"], first["processId"]);
    let other_dir = setup.temp.0.join("new-daemon");
    std::fs::create_dir(&other_dir).unwrap();
    let other = Backend::start(&other_dir);
    other
        .model
        .lock()
        .unwrap()
        .add("moved-thread", json!("cli"), "idle");
    state::write_json(
        &version,
        &json!({"status":"running","socketPath":other.path}),
    )
    .unwrap();
    wait(|| {
        setup.presence("moved-thread").is_some_and(|v| {
            v["processId"] != first["processId"] && v["profile"]["registered"] == true
        })
    });
    let next = setup.live("moved-thread");
    wait(|| {
        setup
            .status(&next)
            .is_some_and(|s| s["receipts"].as_array().unwrap().len() == 1)
    });
    assert_eq!(setup.status(&next).unwrap()["receipts"][0]["id"], id);
    assert_eq!(other.model.lock().unwrap().deliveries.len(), 1);
    assert!(setup.backend.model.lock().unwrap().deliveries.is_empty());
    drop(setup.watcher.take());
}

#[test]
fn stopping_an_inbox_cancels_and_reaps_its_blocking_notify_process_group() {
    struct Hook(Option<i32>);
    impl Drop for Hook {
        fn drop(&mut self) {
            if let Some(pid) = self.0 {
                unsafe {
                    libc::kill(-pid, libc::SIGKILL);
                }
            }
        }
    }
    let mut setup = Setup::new();
    setup
        .backend
        .model
        .lock()
        .unwrap()
        .add("notify-thread", json!("cli"), "idle");
    let config = setup.temp.0.join("watch.toml");
    let mut text = std::fs::read_to_string(&config).unwrap();
    text.push_str(&format!(
        "notify_command={:?}\nnotify_args=['test-blocking-notify']\n",
        support::fake_codex()
    ));
    std::fs::write(config, text).unwrap();
    setup.start();
    let first = setup.live("notify-thread");
    setup.backend.model.lock().unwrap().mode = "rejectDelivery".into();
    setup.send(&first, "rejected for notify test");
    let pid_file = setup.temp.0.join("notify.pid");
    wait(|| pid_file.exists());
    let pid = std::fs::read_to_string(pid_file)
        .unwrap()
        .parse::<i32>()
        .unwrap();
    let mut hook = Hook(Some(pid));
    let start = Instant::now();
    setup.backend.model.lock().unwrap().loaded.clear();
    wait(|| setup.presence("notify-thread").is_none());
    wait(|| unsafe { libc::kill(pid, 0) } != 0);
    hook.0 = None;
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "hook did not cancel before the worker shutdown deadline"
    );
}
