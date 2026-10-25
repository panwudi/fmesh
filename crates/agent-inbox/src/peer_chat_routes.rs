//! Replicated Feishu chat ownership. Each host publishes only its own claims.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{Result, chat_routes, http, hub, state};

const MAX_LOCAL_ROUTES: usize = 256;
const MAX_HOSTS: usize = 256;
const MAX_ALL_ROUTES: usize = 4096;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Claim {
    pub chat: String,
    pub agent: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostSnapshot {
    pub host: String,
    pub revision: u64,
    pub routes: Vec<Claim>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    version: u32,
    host: String,
    generation: u64,
    local_revision: u64,
    local: BTreeMap<String, String>,
    remote: BTreeMap<String, HostSnapshot>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Seed {
    routes: Vec<Claim>,
}

pub struct Routes {
    path: PathBuf,
    data: Mutex<Snapshot>,
    _lock: File,
}

fn validate_claim(claim: &Claim, host: &str) -> Result<()> {
    chat_routes::validate_chat(&claim.chat)?;
    if hub::qualified(&claim.agent)?.0 != host {
        return Err("chat route agent is outside its authenticated host".into());
    }
    Ok(())
}

fn validate_host_snapshot(value: &HostSnapshot) -> Result<()> {
    hub::validate_component(&value.host)?;
    if value.routes.len() > MAX_LOCAL_ROUTES {
        return Err("host chat route snapshot is too large".into());
    }
    let mut seen = BTreeSet::new();
    for route in &value.routes {
        validate_claim(route, &value.host)?;
        if !seen.insert(&route.chat) {
            return Err("duplicate chat in host snapshot".into());
        }
    }
    Ok(())
}

fn merged(snapshot: &Snapshot) -> BTreeMap<String, BTreeSet<String>> {
    let mut found: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (chat, agent) in &snapshot.local {
        found.entry(chat.clone()).or_default().insert(agent.clone());
    }
    for host in snapshot.remote.values() {
        for route in &host.routes {
            found
                .entry(route.chat.clone())
                .or_default()
                .insert(route.agent.clone());
        }
    }
    found
}

impl Routes {
    pub fn open(path: PathBuf, host: &str, seed_file: Option<&Path>) -> Result<Self> {
        hub::validate_component(host)?;
        state::parent(&path)?;
        let lock = state::open_private(&path.with_extension("chat-routes-lock"), true)?;
        lock.try_lock()
            .map_err(|_| "another relay owns this peer chat route state")?;
        let snapshot = if path.exists() {
            let saved: Snapshot = state::read_json(&path)?;
            if saved.version != 1
                || saved.host != host
                || saved.local.len() > MAX_LOCAL_ROUTES
                || saved.remote.len() > MAX_HOSTS
            {
                return Err("invalid peer chat route state".into());
            }
            for (chat, agent) in &saved.local {
                validate_claim(
                    &Claim {
                        chat: chat.clone(),
                        agent: agent.clone(),
                    },
                    host,
                )?;
            }
            for (remote_host, claims) in &saved.remote {
                if remote_host != &claims.host || remote_host == host {
                    return Err("invalid remote chat route host".into());
                }
                validate_host_snapshot(claims)?;
            }
            saved
        } else {
            let mut local = BTreeMap::new();
            if let Some(seed_file) = seed_file {
                let bytes = std::fs::read(seed_file)?;
                if bytes.len() > 64 * 1024 {
                    return Err("chat route seed exceeds 64 KiB".into());
                }
                let seed: Seed = serde_json::from_slice(&bytes)?;
                if seed.routes.len() > MAX_LOCAL_ROUTES {
                    return Err("chat route seed has too many routes".into());
                }
                for claim in seed.routes {
                    validate_claim(&claim, host)?;
                    if local.insert(claim.chat, claim.agent).is_some() {
                        return Err("chat route seed has a duplicate chat".into());
                    }
                }
            }
            let fresh = Snapshot {
                version: 1,
                host: host.to_owned(),
                generation: 1,
                local_revision: 1,
                local,
                remote: BTreeMap::new(),
            };
            state::write_json(&path, &fresh)?;
            fresh
        };
        if merged(&snapshot).len() > MAX_ALL_ROUTES {
            return Err("peer chat route state is too large".into());
        }
        Ok(Self {
            path,
            data: Mutex::new(snapshot),
            _lock: lock,
        })
    }

    fn edit<T>(&self, change: impl FnOnce(&mut Snapshot) -> Result<T>) -> Result<T> {
        let mut data = self.data.lock().map_err(|_| "chat route lock failed")?;
        let mut next = data.clone();
        let result = change(&mut next)?;
        if merged(&next).len() > MAX_ALL_ROUTES {
            return Err("peer chat route table is full".into());
        }
        if next.generation != data.generation {
            state::write_json(&self.path, &next)?;
            *data = next;
        }
        Ok(result)
    }

    pub fn register(&self, sender: &str, chat: &str) -> Result<Value> {
        chat_routes::validate_chat(chat)?;
        let host = hub::qualified(sender)?.0;
        self.edit(|data| {
            if host != data.host {
                return Err("only a local agent may register a chat".into());
            }
            if merged(data)
                .get(chat)
                .is_some_and(|owners| owners.iter().any(|owner| owner != sender))
            {
                return Err("chat is registered to another agent".into());
            }
            if data.local.get(chat).is_none_or(|owner| owner != sender) {
                if data.local.len() >= MAX_LOCAL_ROUTES {
                    return Err("local chat route table is full".into());
                }
                data.local.insert(chat.to_owned(), sender.to_owned());
                data.local_revision = data.local_revision.saturating_add(1);
                data.generation = data.generation.saturating_add(1);
            }
            Ok(json!({"status":"registered","chat":chat,"agent":sender,
                "convergence":"local; peer confirmation pending"}))
        })
    }

    pub fn remove(&self, sender: &str, chat: &str) -> Result<Value> {
        chat_routes::validate_chat(chat)?;
        let host = hub::qualified(sender)?.0;
        self.edit(|data| {
            if host != data.host || data.local.get(chat).is_some_and(|agent| agent != sender) {
                return Err("only the owning local agent may remove this chat".into());
            }
            if data.local.remove(chat).is_some() {
                data.local_revision = data.local_revision.saturating_add(1);
                data.generation = data.generation.saturating_add(1);
            }
            Ok(json!({"status":"removed","chat":chat,"agent":sender,
                "convergence":"local; peer confirmation pending"}))
        })
    }

    pub fn own_snapshot(&self) -> Result<HostSnapshot> {
        let data = self.data.lock().map_err(|_| "chat route lock failed")?;
        let snapshot = HostSnapshot {
            host: data.host.clone(),
            revision: data.local_revision,
            routes: data
                .local
                .iter()
                .map(|(chat, agent)| Claim {
                    chat: chat.clone(),
                    agent: agent.clone(),
                })
                .collect(),
        };
        if serde_json::to_vec(&snapshot)?.len() > 48 * 1024 {
            return Err("local chat route snapshot exceeds 48 KiB".into());
        }
        Ok(snapshot)
    }

    pub fn merge_remote(&self, source_host: &str, incoming: HostSnapshot) -> Result<bool> {
        validate_host_snapshot(&incoming)?;
        if source_host != incoming.host {
            return Err("remote chat routes must match the authenticated host".into());
        }
        self.edit(|data| {
            if source_host == data.host {
                return Err("cannot import this host as a remote chat route owner".into());
            }
            if let Some(current) = data.remote.get(source_host) {
                if incoming.revision < current.revision {
                    return Ok(false);
                }
                if incoming.revision == current.revision {
                    return if &incoming == current {
                        Ok(false)
                    } else {
                        Err("conflicting chat route snapshot revision".into())
                    };
                }
            }
            data.remote.insert(source_host.to_owned(), incoming);
            data.generation = data.generation.saturating_add(1);
            Ok(true)
        })
    }

    pub fn prune_remote(&self, allowed: &BTreeSet<String>) -> Result<bool> {
        self.edit(|data| {
            let before = data.remote.len();
            data.remote.retain(|host, _| allowed.contains(host));
            if data.remote.len() != before {
                data.generation = data.generation.saturating_add(1);
                Ok(true)
            } else {
                Ok(false)
            }
        })
    }

    pub fn list(&self, cursor: usize) -> Result<Value> {
        if cursor > MAX_ALL_ROUTES {
            return Err("invalid chat route cursor".into());
        }
        let data = self.data.lock().map_err(|_| "chat route lock failed")?;
        let merged = merged(&data);
        let routes: Vec<_> = merged
            .iter()
            .filter(|(_, owners)| owners.len() == 1)
            .map(|(chat, owners)| json!({"chat":chat,"agent":owners.iter().next().unwrap()}))
            .collect();
        let conflicts: Vec<_> = merged
            .iter()
            .filter(|(_, owners)| owners.len() > 1)
            .map(|(chat, owners)| json!({"chat":chat,"agents":owners}))
            .collect();
        if conflicts.len() > 64 {
            return Err("too many chat route conflicts; operator review required".into());
        }
        let page: Vec<_> = routes.iter().skip(cursor).take(64).cloned().collect();
        let next = (cursor + page.len() < routes.len()).then_some(cursor + page.len());
        let result = json!({"routes":page,"nextCursor":next,"revision":data.generation,
            "conflicts":conflicts});
        if serde_json::to_vec(&result)?.len() > crate::MAX_MESSAGE_BYTES {
            return Err("chat route page exceeds 64 KiB".into());
        }
        Ok(result)
    }

    pub fn handle_local(&self, path: &str, sender: &str, text: &str) -> http::Response {
        let action = match path {
            "/chats/register" => "register",
            "/chats/remove" => "remove",
            "/chats/list" => "list",
            _ => return http::Response::new(404, "unknown chat route endpoint"),
        };
        let result = if action == "list" {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Query {
                cursor: Option<usize>,
            }
            serde_json::from_str::<Query>(text)
                .map_err(|_| "invalid chat route query".into())
                .and_then(|query| self.list(query.cursor.unwrap_or(0)))
        } else {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Change {
                chat: String,
            }
            serde_json::from_str::<Change>(text)
                .map_err(|_| "chat route accepts only chat".into())
                .and_then(|change| {
                    if action == "register" {
                        self.register(sender, &change.chat)
                    } else {
                        self.remove(sender, &change.chat)
                    }
                })
        };
        match result {
            Ok(value) => http::Response::json(200, value),
            Err(_) => http::Response::new(409, "chat route rejected; inspect current registration"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            Self(std::env::temp_dir().join(format!("peer-chat-routes-{}", state::id().unwrap())))
        }
        fn path(&self) -> PathBuf {
            self.0.join("routes.json")
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn concurrent_claims_fail_closed_then_owner_removal_recovers() {
        let temp = Temp::new();
        let routes = Routes::open(temp.path(), "alpha", None).unwrap();
        routes.register("alpha.agent", "oc_team").unwrap();
        assert!(routes.register("alpha.other", "oc_team").is_err());
        assert!(routes.remove("alpha.other", "oc_team").is_err());
        let beta = HostSnapshot {
            host: "beta".into(),
            revision: 2,
            routes: vec![Claim {
                chat: "oc_team".into(),
                agent: "beta.agent".into(),
            }],
        };
        assert!(routes.merge_remote("beta", beta.clone()).unwrap());
        let page = routes.list(0).unwrap();
        assert!(page["routes"].as_array().unwrap().is_empty());
        assert_eq!(page["conflicts"][0]["chat"], "oc_team");
        assert!(routes.merge_remote("gamma", beta.clone()).is_err());
        assert!(!routes.merge_remote("beta", beta.clone()).unwrap());
        let mut fork = beta.clone();
        fork.routes.clear();
        assert!(routes.merge_remote("beta", fork.clone()).is_err());
        routes.remove("alpha.agent", "oc_team").unwrap();
        assert_eq!(routes.list(0).unwrap()["routes"][0]["agent"], "beta.agent");
        fork.revision = 3;
        assert!(routes.merge_remote("beta", fork).unwrap());
        assert!(
            routes.list(0).unwrap()["routes"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        drop(routes);
        let reopened = Routes::open(temp.path(), "alpha", None).unwrap();
        assert!(
            reopened.list(0).unwrap()["conflicts"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn seed_routes_keep_original_agent_self_removal_rights() {
        let temp = Temp::new();
        state::parent(&temp.path()).unwrap();
        let seed = temp.0.join("seed.json");
        std::fs::write(
            &seed,
            json!({"routes":[{"chat":"oc_owner","agent":"alpha.owner"}]}).to_string(),
        )
        .unwrap();
        let routes = Routes::open(temp.path(), "alpha", Some(&seed)).unwrap();
        assert_eq!(routes.list(0).unwrap()["routes"][0]["agent"], "alpha.owner");
        assert!(routes.remove("alpha.other", "oc_owner").is_err());
        routes.remove("alpha.owner", "oc_owner").unwrap();
        assert!(
            routes.list(0).unwrap()["routes"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }
}
