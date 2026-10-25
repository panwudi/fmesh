//! Grok Build ACP client for a running shared leader. Only resident sessions are addressable.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::Path;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::Mutex;
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use serde_json::{Value, json};

use crate::{Result, coordination_input};

const MAX_LINE_BYTES: usize = 8 * 1024 * 1024;

pub struct Client {
    child: Child,
    stdin: ChildStdin,
    incoming: Receiver<Result<Value>>,
    next_id: u64,
    timeout: Duration,
}

#[derive(Debug)]
pub struct RpcError {
    method: String,
    code: i64,
    message: String,
    data: Option<String>,
}

impl RpcError {
    pub fn is_missing_session(&self) -> bool {
        self.method == "_x.ai/interject"
            && self.code == -32602
            && (self.message.contains("session not found")
                || self
                    .data
                    .as_deref()
                    .is_some_and(|data| data.contains("session not found")))
    }
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "Grok ACP {} rejected ({}): {}",
            self.method, self.code, self.message
        )
    }
}
impl std::error::Error for RpcError {}

pub struct Shared {
    grok: PathBuf,
    socket: PathBuf,
    timeout: Duration,
    client: Mutex<Option<Client>>,
}

#[derive(Clone)]
pub struct Session {
    pub id: String,
    pub cwd: String,
    pub activity: String,
}

pub fn check_socket(path: &Path) -> Result<()> {
    let metadata = std::fs::metadata(path).map_err(|_| "Grok leader socket unavailable")?;
    if !metadata.file_type().is_socket() || metadata.uid() != unsafe { libc::geteuid() } {
        return Err("Grok leader socket must belong to this OS user".into());
    }
    std::os::unix::net::UnixStream::connect(path)
        .map_err(|_| "Grok leader socket is not accepting connections")?;
    Ok(())
}

pub fn default_socket() -> Result<PathBuf> {
    Ok(
        PathBuf::from(std::env::var_os("HOME").ok_or("HOME is unavailable")?)
            .join(".grok/leader.sock"),
    )
}

impl Shared {
    pub fn new(grok: PathBuf, socket: PathBuf, timeout: Duration) -> Self {
        Self {
            grok,
            socket,
            timeout,
            client: Mutex::new(None),
        }
    }

    fn with_client<T>(&self, action: impl FnOnce(&mut Client) -> Result<T>) -> Result<T> {
        let mut client = self
            .client
            .lock()
            .map_err(|_| "Grok ACP client lock failed")?;
        if client.is_none() {
            *client = Some(Client::connect(&self.grok, &self.socket, self.timeout)?);
        }
        let result = action(client.as_mut().unwrap());
        if result.is_err() {
            *client = None;
        }
        result
    }

    pub fn roster(&self) -> Result<Vec<Session>> {
        self.with_client(Client::roster)
    }

    pub fn disconnect(&self) {
        if let Ok(mut client) = self.client.lock() {
            *client = None;
        }
    }

    pub fn interject(&self, session: &str, sender: &str, text: &str, id: &str) -> Result<()> {
        self.with_client(|client| client.interject(session, sender, text, id))
    }
}

impl Client {
    pub fn connect(grok: &Path, socket: &Path, timeout: Duration) -> Result<Self> {
        check_socket(socket)?;
        let mut child = Command::new(grok)
            .args(["agent", "--leader", "--leader-socket"])
            .arg(socket)
            .arg("stdio")
            .env("GROK_DISABLE_AUTOUPDATER", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let stdin = child.stdin.take().ok_or("Grok ACP stdin unavailable")?;
        let stdout = child.stdout.take().ok_or("Grok ACP stdout unavailable")?;
        let (sender, incoming) = mpsc::sync_channel(16);
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut line = String::new();
                let read = reader.read_line(&mut line);
                match read {
                    Ok(0) => break,
                    Ok(_) if line.len() > MAX_LINE_BYTES => {
                        let _ = sender.send(Err("Grok ACP line exceeded 8 MiB".into()));
                        break;
                    }
                    Ok(_) => match serde_json::from_str::<Value>(&line) {
                        Ok(value) if value.get("method").is_none() && value.get("id").is_some() => {
                            if sender.send(Ok(value)).is_err() {
                                break;
                            }
                        }
                        Ok(_) => {}
                        Err(_) => {
                            let _ = sender.send(Err("invalid Grok ACP JSON line".into()));
                            break;
                        }
                    },
                    Err(_) => {
                        let _ = sender.send(Err("Grok ACP stdout failed".into()));
                        break;
                    }
                }
            }
        });
        let mut client = Self {
            child,
            stdin,
            incoming,
            next_id: 1,
            timeout,
        };
        let initialized = client.request(
            "initialize",
            json!({
                "protocolVersion":1,
                "clientCapabilities":{},
                "_meta":{"clientIdentifier":"agent-inbox"}
            }),
        )?;
        if initialized["protocolVersion"] != 1 {
            return Err("Grok ACP protocol version mismatch".into());
        }
        let methods = initialized["authMethods"]
            .as_array()
            .ok_or("Grok ACP auth methods missing")?;
        if methods.iter().any(|method| method["id"] == "cached_token") {
            client.request(
                "authenticate",
                json!({
                    "methodId":"cached_token","_meta":{"headless":true}
                }),
            )?;
        }
        Ok(client)
    }

    pub fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let message = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
        serde_json::to_writer(&mut self.stdin, &message)?;
        self.stdin.write_all(b"\n")?;
        self.stdin.flush()?;
        loop {
            let response = self
                .incoming
                .recv_timeout(self.timeout)
                .map_err(|_| "Grok ACP response timed out or connection closed")??;
            if response["id"] != id {
                continue;
            }
            if let Some(error) = response.get("error") {
                let code = error["code"].as_i64().unwrap_or(0);
                let reason = error["message"].as_str().unwrap_or("request rejected");
                return Err(RpcError {
                    method: method.to_owned(),
                    code,
                    message: reason.to_owned(),
                    data: error.get("data").map(|data| {
                        data.as_str()
                            .map(str::to_owned)
                            .unwrap_or_else(|| data.to_string())
                    }),
                }
                .into());
            }
            return response
                .get("result")
                .cloned()
                .ok_or_else(|| "Grok ACP response missing result".into());
        }
    }

    pub fn roster(&mut self) -> Result<Vec<Session>> {
        let response = self.request("_x.ai/sessions/list", json!({}))?;
        let payload = response.get("result").unwrap_or(&response);
        let entries = payload["sessions"]
            .as_array()
            .ok_or("Grok ACP roster missing sessions")?;
        if entries.len() > 4096 {
            return Err("Grok ACP roster exceeded 4096 sessions".into());
        }
        let mut result = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for entry in entries {
            if entry["resident"] != true || entry["origin"]["kind"] != "local" {
                continue;
            }
            if entry["sessionKind"] == "subagent" || entry["sessionKind"] == "headless" {
                continue;
            }
            let id = entry["sessionId"]
                .as_str()
                .ok_or("Grok roster session ID missing")?;
            if !crate::state::is_id(id) || !seen.insert(id.to_owned()) {
                return Err("Grok roster has invalid or repeated session ID".into());
            }
            let cwd = entry["cwd"].as_str().ok_or("Grok roster cwd missing")?;
            if !Path::new(cwd).is_absolute() {
                return Err("Grok roster cwd must be absolute".into());
            }
            let activity = entry["activity"]
                .as_str()
                .ok_or("Grok roster activity missing")?;
            if !matches!(activity, "working" | "idle" | "needs_input" | "completed") {
                return Err("Grok roster has unknown resident activity".into());
            }
            result.push(Session {
                id: id.to_owned(),
                cwd: cwd.to_owned(),
                activity: activity.to_owned(),
            });
        }
        Ok(result)
    }

    pub fn interject(&mut self, session: &str, sender: &str, text: &str, id: &str) -> Result<()> {
        let wrapped = coordination_input(sender, text)[0]["text"]
            .as_str()
            .ok_or("Grok coordination input missing")?
            .to_owned();
        let response = self.request(
            "_x.ai/interject",
            json!({
                "sessionId":session,"text":wrapped,"interjectionId":id
            }),
        )?;
        let payload = response.get("result").unwrap_or(&response);
        if payload["status"] != "queued" {
            return Err("Grok ACP interjection was not acknowledged".into());
        }
        Ok(())
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
