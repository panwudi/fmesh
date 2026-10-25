use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};
use tungstenite::{Message, WebSocket, protocol::WebSocketConfig};

use crate::{Result, coordination_input, proxy::ProxyStream};

pub struct Client {
    socket: WebSocket<ProxyStream>,
    timeout: Duration,
    next_id: u64,
}

impl Client {
    pub fn connect(codex: &Path, socket: &Path, timeout: Duration) -> Result<Self> {
        let stream = ProxyStream::spawn(codex, socket, timeout).map_err(|error| {
            format!("WebSocket handshake through codex app-server proxy failed: {error}")
        })?;
        let config = WebSocketConfig::default()
            .max_message_size(Some(8 * 1024 * 1024))
            .max_frame_size(Some(8 * 1024 * 1024));
        let (socket, _) =
            tungstenite::client::client_with_config("ws://localhost/", stream, Some(config))
                .map_err(|e| {
                    format!("WebSocket handshake through codex app-server proxy failed: {e}")
                })?;
        let mut client = Self {
            socket,
            timeout,
            next_id: 1,
        };
        let initialized = client.request("initialize", json!({
            "clientInfo": {"name": "agent_inbox", "title": "Agent Inbox", "version": env!("CARGO_PKG_VERSION")},
            "capabilities": {"experimentalApi": true}
        }))?;
        for field in ["userAgent", "platformFamily", "platformOs", "codexHome"] {
            if initialized[field].as_str().is_none() {
                return Err(format!("protocol mismatch: initialize missing {field}").into());
            }
        }
        client.send(json!({"method": "initialized", "params": {}}))?;
        Ok(client)
    }

    fn send(&mut self, value: Value) -> Result<()> {
        self.socket
            .send(Message::Text(serde_json::to_string(&value)?.into()))?;
        Ok(())
    }

    pub fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        self.socket.get_mut().reset_deadline(self.timeout);
        self.send(json!({"id": id, "method": method, "params": params}))?;
        loop {
            let message = match self.socket.read()? {
                Message::Text(text) => serde_json::from_str::<Value>(&text)?,
                Message::Ping(_) | Message::Pong(_) => {
                    self.socket.flush()?;
                    continue;
                }
                Message::Close(_) => {
                    return Err(format!("app-server closed while awaiting {method}").into());
                }
                _ => return Err("protocol mismatch: expected a WebSocket text frame".into()),
            };
            if message.get("method").is_some() || message.get("id").is_none() {
                continue;
            }
            if message["id"] != id {
                return Err("protocol mismatch: unexpected JSON-RPC response id".into());
            }
            if let Some(error) = message.get("error") {
                return Err(RpcError {
                    method: method.to_owned(),
                    code: error["code"]
                        .as_i64()
                        .ok_or("protocol mismatch: error code missing")?,
                    message: error["message"]
                        .as_str()
                        .ok_or("protocol mismatch: error message missing")?
                        .to_owned(),
                }
                .into());
            }
            return message
                .get("result")
                .cloned()
                .ok_or_else(|| "protocol mismatch: response has no result".into());
        }
    }

    pub fn read_thread(&mut self, thread: &str) -> Result<Value> {
        let result = self.request("thread/read", json!({"threadId": thread}))?;
        if result["thread"]["id"] != thread {
            return Err("protocol mismatch: thread/read returned another thread".into());
        }
        Ok(result["thread"].clone())
    }

    pub fn active_turn(&mut self, thread: &str) -> Result<Option<String>> {
        let turns = self.request(
            "thread/turns/list",
            json!({
                "threadId": thread, "limit": 1, "sortDirection": "desc", "itemsView": "notLoaded"
            }),
        )?;
        let data = turns["data"]
            .as_array()
            .ok_or("protocol mismatch: turns/list data missing")?;
        let Some(latest) = data.first() else {
            return Ok(None);
        };
        match latest["status"].as_str() {
            Some("inProgress") => {}
            Some("completed" | "failed" | "interrupted") => return Ok(None),
            _ => return Err("protocol mismatch: unknown turn status".into()),
        }
        Ok(Some(
            latest["id"]
                .as_str()
                .filter(|id| !id.is_empty())
                .ok_or("protocol mismatch: active turn has no id")?
                .to_owned(),
        ))
    }

    pub fn deliver(&mut self, thread: &str, sender: &str, text: &str) -> Result<Delivery> {
        self.deliver_tracked(thread, sender, text, None, || Ok(()))
    }

    pub fn deliver_tracked(
        &mut self,
        thread: &str,
        sender: &str,
        text: &str,
        client_id: Option<&str>,
        mut before_send: impl FnMut() -> Result<()>,
    ) -> Result<Delivery> {
        let input = coordination_input(sender, text);
        for _ in 0..4 {
            let mut state = self.read_thread(thread)?;
            if state["status"]["type"] == "notLoaded" {
                let result = self.request(
                    "thread/resume",
                    json!({"threadId": thread, "excludeTurns": true}),
                )?;
                if result["thread"]["id"] != thread {
                    return Err("protocol mismatch: thread/resume returned another thread".into());
                }
                state = result["thread"].clone();
            }
            match state["status"]["type"].as_str() {
                Some("active") => {
                    let Some(expected_turn) = self.active_turn(thread)? else {
                        continue;
                    };
                    let mut params =
                        json!({"threadId":thread, "expectedTurnId":expected_turn, "input":input});
                    if let Some(id) = client_id {
                        params["clientUserMessageId"] = json!(id);
                    }
                    before_send()?;
                    let response = self.request("turn/steer", params);
                    let result = match response {
                        Ok(result) => result,
                        Err(error)
                            if error
                                .downcast_ref::<RpcError>()
                                .is_some_and(RpcError::is_turn_race) =>
                        {
                            continue;
                        }
                        Err(error) => return Err(error),
                    };
                    if result["turnId"] != expected_turn {
                        return Err("protocol mismatch after turn/steer; delivery uncertain, do not blindly retry".into());
                    }
                    return Ok(Delivery {
                        method: "turn/steer",
                        turn: expected_turn,
                    });
                }
                Some("idle") => {
                    let mut params = json!({"threadId":thread, "input":input});
                    if let Some(id) = client_id {
                        params["clientUserMessageId"] = json!(id);
                    }
                    before_send()?;
                    let result = match self.request("turn/start", params) {
                        Ok(result) => result,
                        Err(error)
                            if error
                                .downcast_ref::<RpcError>()
                                .is_some_and(RpcError::is_turn_race) =>
                        {
                            continue;
                        }
                        Err(error) => return Err(error),
                    };
                    let turn = result["turn"]["id"].as_str().filter(|id| !id.is_empty())
                    .ok_or("protocol mismatch after turn/start; delivery uncertain, do not blindly retry")?;
                    return Ok(Delivery {
                        method: "turn/start",
                        turn: turn.to_owned(),
                    });
                }
                _ => {
                    return Err(
                        "thread is unavailable or has an unknown status; nothing sent".into(),
                    );
                }
            }
        }
        Err("thread kept changing after 3 retries; message was not accepted".into())
    }

    /// Absence is conclusive only after every page was read successfully.
    pub fn find_client_message(&mut self, thread: &str, client_id: &str) -> Result<Option<String>> {
        let state = self.read_thread(thread)?;
        if state["status"]["type"] == "notLoaded" {
            let resumed = self.request(
                "thread/resume",
                json!({"threadId":thread,"excludeTurns":true}),
            )?;
            if resumed["thread"]["id"] != thread {
                return Err("protocol mismatch: reconciliation resumed another thread".into());
            }
        }
        let mut cursor = None;
        let mut seen = std::collections::HashSet::new();
        loop {
            if seen.len() > 4096 {
                return Err("history incomplete: excessive pagination".into());
            }
            let result = self.request(
                "thread/items/list",
                json!({"threadId":thread, "limit":100, "sortDirection":"desc", "cursor":cursor}),
            )?;
            let data = result["data"]
                .as_array()
                .ok_or("protocol mismatch: history data missing")?;
            for entry in data {
                let item = entry
                    .get("item")
                    .ok_or("protocol mismatch: history item missing")?;
                if item["type"].as_str().is_none() {
                    return Err("history incomplete: malformed item or excessive pagination".into());
                }
                if item["type"] == "userMessage" && item["clientId"] == client_id {
                    return Ok(Some(
                        entry["turnId"]
                            .as_str()
                            .filter(|id| !id.is_empty())
                            .ok_or("protocol mismatch: history turn id missing")?
                            .to_owned(),
                    ));
                }
            }
            match result.get("nextCursor") {
                None | Some(Value::Null) => return Ok(None),
                Some(Value::String(next)) if !next.is_empty() && seen.insert(next.clone()) => {
                    cursor = Some(next.clone())
                }
                _ => {
                    return Err(
                        "history incomplete: missing or repeated continuation cursor".into(),
                    );
                }
            }
        }
    }
}

#[derive(Debug)]
pub struct RpcError {
    method: String,
    code: i64,
    message: String,
}

impl RpcError {
    pub fn method(&self) -> &str {
        &self.method
    }

    pub fn code(&self) -> i64 {
        self.code
    }

    pub fn is_draining(&self) -> bool {
        self.code == -32600 && self.message.starts_with("Server is draining")
    }
    pub fn rejected_delivery(&self) -> bool {
        self.method == "turn/start" || self.method == "turn/steer"
    }
    fn is_turn_race(&self) -> bool {
        match self.method.as_str() {
            "turn/start" => self.message == "thread already has an active or pending turn",
            "turn/steer" => {
                self.message == "no active turn to steer"
                    || self.message.starts_with("expected active turn id ")
            }
            _ => false,
        }
    }
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "app-server rejected {} (code {}): {}",
            self.method, self.code, self.message
        )
    }
}

impl std::error::Error for RpcError {}

pub struct Delivery {
    pub method: &'static str,
    pub turn: String,
}
