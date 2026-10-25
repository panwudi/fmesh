//! Persistent one-chat/one-agent routing, bound to the authenticated sender.
use std::collections::BTreeMap;
use std::fs::File;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{Result, http, hub, state, token::Token};

pub fn validate_chat(chat: &str) -> Result<()> {
    if !chat.starts_with("oc_")
        || chat.len() <= 3
        || chat.len() > 128
        || !chat.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        return Err("chat must be an oc_ Feishu chat id".into());
    }
    Ok(())
}

#[derive(Clone, Serialize, Deserialize)]
struct Snapshot {
    version: u32,
    #[serde(default)]
    revision: u64,
    routes: BTreeMap<String, String>,
}

pub struct Routes {
    path: PathBuf,
    data: Mutex<Snapshot>,
    _lock: File,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Change {
    chat: String,
}

impl Routes {
    pub fn open(path: PathBuf) -> Result<Self> {
        state::parent(&path)?;
        let lock = state::open_private(&path.with_extension("chat-routes-lock"), true)?;
        lock.try_lock()
            .map_err(|_| "another hub owns this chat routing table")?;
        let data = if path.exists() {
            state::read_json::<Snapshot>(&path)?
        } else {
            Snapshot {
                version: 1,
                revision: 0,
                routes: BTreeMap::new(),
            }
        };
        if data.version != 1 || data.routes.len() > 4096 {
            return Err("invalid chat route state version or capacity".into());
        }
        for (chat, agent) in &data.routes {
            validate_chat(chat)?;
            hub::qualified(agent)?;
        }
        state::write_json(&path, &data)?;
        Ok(Self {
            path,
            data: Mutex::new(data),
            _lock: lock,
        })
    }

    pub fn handle(&self, path: &str, sender: &str, text: &str) -> http::Response {
        if path == "/chats/list" {
            return match self.page(text) {
                Ok(page) => http::Response::json(200, page),
                Err(_) => http::Response::new(400, "invalid chat route query"),
            };
        }
        if !matches!(path, "/chats/register" | "/chats/remove") {
            return http::Response::new(404, "unknown chat route endpoint");
        }
        if hub::qualified(sender).is_err() {
            return http::Response::new(
                403,
                "chat routes require a qualified authenticated sender",
            );
        }
        let change: Change = match serde_json::from_str(text) {
            Ok(change) => change,
            Err(_) => return http::Response::new(400, "chat route accepts only chat"),
        };
        if validate_chat(&change.chat).is_err() {
            return http::Response::new(400, "invalid Feishu chat id");
        }
        let Ok(mut data) = self.data.lock() else {
            return http::Response::new(500, "chat routing lock failed");
        };
        if let Some(owner) = data.routes.get(&change.chat)
            && owner != sender
        {
            return http::Response::json(
                409,
                json!({"error":"chat is registered to another agent","chat":change.chat,"agent":owner}),
            );
        }
        let mut next = data.clone();
        if path == "/chats/register" {
            if !next.routes.contains_key(&change.chat) && next.routes.len() >= 4096 {
                return http::Response::new(409, "chat routing capacity reached");
            }
            next.routes.insert(change.chat.clone(), sender.to_owned());
        } else {
            next.routes.remove(&change.chat);
        }
        next.revision = next.revision.saturating_add(1);
        if state::write_json(&self.path, &next).is_err() {
            return http::Response::new(500, "chat routing save failed; unchanged");
        }
        *data = next;
        http::Response::json(
            200,
            json!({"status":if path=="/chats/register"{"registered"}else{"removed"},"chat":change.chat,"agent":sender}),
        )
    }

    fn page(&self, text: &str) -> Result<Value> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Query {
            cursor: Option<usize>,
        }
        let query: Query = serde_json::from_str(text).map_err(|_| "invalid chat route query")?;
        let cursor = query.cursor.unwrap_or(0);
        if cursor > 4096 {
            return Err("invalid chat route cursor".into());
        }
        let data = self.data.lock().map_err(|_| "chat route lock failed")?;
        let rows: Vec<_> = data
            .routes
            .iter()
            .skip(cursor)
            .take(64)
            .map(|(chat, agent)| json!({"chat":chat,"agent":agent}))
            .collect();
        let next = (cursor + rows.len() < data.routes.len()).then_some(cursor + rows.len());
        Ok(json!({"routes":rows,"nextCursor":next,"revision":data.revision}))
    }
}

pub fn run(mut args: impl Iterator<Item = String>) -> Result<()> {
    let action = args
        .next()
        .ok_or("chat-route requires register, remove or list")?;
    if !matches!(action.as_str(), "register" | "remove" | "list") {
        return Err("unknown chat-route action".into());
    }
    let mut relay = std::env::var("AGENT_INBOX_RELAY").ok();
    let mut token_path = std::env::var_os("AGENT_INBOX_TOKEN_FILE").map(PathBuf::from);
    let mut chat = None;
    while let Some(flag) = args.next() {
        let value = args.next().ok_or("missing chat-route option value")?;
        match flag.as_str() {
            "--hub" => relay = Some(value),
            "--token-file" => token_path = Some(value.into()),
            "--chat" => chat = Some(value),
            _ => return Err("unknown chat-route option".into()),
        }
    }
    if action == "list" && chat.is_some() {
        return Err("list does not accept --chat".into());
    }
    if action != "list" {
        validate_chat(chat.as_deref().ok_or("register/remove require --chat")?)?;
    }
    let relay = hub::loopback_endpoint(&relay.ok_or("chat-route requires --hub")?)?;
    let token = Token::read(&token_path.ok_or("chat-route requires --token-file")?)?;
    let mut cursor = None;
    let mut rows = Vec::new();
    let mut revision = None;
    loop {
        let body = if action == "list" {
            json!({"cursor":cursor})
        } else {
            json!({"chat":chat})
        };
        let (code, body) = http::post_response(
            &relay,
            &format!("/chats/{action}"),
            &token,
            &body.to_string(),
        )?;
        if code != 200 {
            return Err(format!("chat-route rejected or unconfirmed (HTTP {code}); use list to inspect the current registration").into());
        }
        let page: Value = serde_json::from_str(&body)?;
        if action != "list" {
            println!("{}", serde_json::to_string_pretty(&page)?);
            break;
        }
        let current = page["revision"]
            .as_u64()
            .ok_or("invalid chat route revision")?;
        if revision.is_some_and(|previous| previous != current) {
            return Err("chat routes changed during pagination; retry list".into());
        }
        revision = Some(current);
        rows.extend(
            page["routes"]
                .as_array()
                .ok_or("invalid chat route page")?
                .iter()
                .cloned(),
        );
        match page["nextCursor"].as_u64() {
            None if page["nextCursor"].is_null() => {
                println!("{}", serde_json::to_string_pretty(&rows)?);
                break;
            }
            Some(next) if cursor.is_none_or(|previous| next > previous) => cursor = Some(next),
            _ => return Err("invalid chat route cursor".into()),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("inbox-chat-routes-{}", state::id().unwrap()));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn open(&self) -> Routes {
            Routes::open(self.0.join("routes.json")).unwrap()
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn self_registration_cannot_overwrite_others_or_supply_a_target_and_survives_restart() {
        let temp = Temp::new();
        let routes = temp.open();
        let body = r#"{"chat":"oc_team"}"#;
        assert_eq!(
            routes.handle("/chats/register", "host.alice", body).status,
            200
        );
        assert_eq!(
            routes.handle("/chats/register", "host.alice", body).status,
            200
        );
        assert_eq!(
            routes.handle("/chats/register", "other.bob", body).status,
            409
        );
        assert_eq!(
            routes.handle("/chats/remove", "other.bob", body).status,
            409
        );
        assert_eq!(
            routes
                .handle(
                    "/chats/register",
                    "host.alice",
                    r#"{"chat":"oc_second","agent":"other.bob"}"#
                )
                .status,
            400
        );
        assert_eq!(
            routes
                .handle("/chats/register", "legacy", r#"{"chat":"oc_new"}"#)
                .status,
            403
        );
        assert_eq!(
            routes
                .handle("/chats/register", "host.alice", r#"{"chat":"oc_../bad"}"#)
                .status,
            400
        );
        assert!(Routes::open(temp.0.join("routes.json")).is_err());
        drop(routes);
        let routes = temp.open();
        let page = routes.page("{}").unwrap();
        assert_eq!(
            page["routes"],
            json!([{"chat":"oc_team","agent":"host.alice"}])
        );
        assert_eq!(
            routes.handle("/chats/remove", "host.alice", body).status,
            200
        );
        assert!(
            routes.page("{}").unwrap()["routes"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn paged_routes_cover_the_table_and_write_failure_keeps_active_state() {
        let temp = Temp::new();
        let routes = temp.open();
        for n in 0..70 {
            assert_eq!(
                routes
                    .handle(
                        "/chats/register",
                        "host.alice",
                        &json!({"chat":format!("oc_{n:03}")}).to_string()
                    )
                    .status,
                200
            );
        }
        let first = routes.page("{}").unwrap();
        let second = routes.page(r#"{"cursor":64}"#).unwrap();
        assert_eq!(first["routes"].as_array().unwrap().len(), 64);
        assert_eq!(second["routes"].as_array().unwrap().len(), 6);
        assert_eq!(first["revision"], second["revision"]);
        assert!(second["nextCursor"].is_null());
        std::fs::remove_file(&routes.path).unwrap();
        std::fs::create_dir(&routes.path).unwrap();
        assert_eq!(
            routes
                .handle("/chats/register", "host.alice", r#"{"chat":"oc_failed"}"#)
                .status,
            500
        );
        assert_eq!(routes.page("{}").unwrap(), first);
    }
}
