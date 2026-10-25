//! Ephemeral same-user registrations, scoped to their own session credentials.
use super::*;
use serde_json::{Value, json};

#[derive(Clone)]
pub(super) struct Session {
    id: String,
    pub expires: Instant,
}

#[derive(Deserialize)]
#[serde(
    tag = "op",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
enum Request {
    Directory {},
    Register {
        id: String,
        name: String,
        description: String,
        inbox: String,
        inbox_token: String,
        sender_token: String,
    },
    Heartbeat {
        id: String,
        sender_token: String,
    },
    Profile {
        id: String,
        sender_token: String,
        name: Option<String>,
        description: Option<String>,
    },
    Unregister {
        id: String,
        sender_token: String,
    },
}

fn credentials(router: &Router, token: &Token, sender: &Token) -> bool {
    token.bytes().len() >= 32
        && sender.bytes().len() >= 32
        && !bool::from(token.bytes().ct_eq(sender.bytes()))
        && ![token, sender].iter().any(|candidate| {
            router
                .host_token
                .as_ref()
                .is_some_and(|host_token| bool::from(candidate.bytes().ct_eq(host_token.bytes())))
                || router
                    .mesh
                    .as_ref()
                    .is_some_and(|mesh| mesh.token_conflicts(candidate))
                || router.agents.values().any(|agent| {
                    agent
                        .token
                        .as_ref()
                        .is_some_and(|token| bool::from(candidate.bytes().ct_eq(token.bytes())))
                        || bool::from(candidate.bytes().ct_eq(agent.sender.bytes()))
                })
        })
}

fn fits(router: &Router) -> bool {
    router.agents.len() <= 256
        && json!({"agents":router.agents.iter().map(|(name,a)|crate::directory::Report {
        name:format!("{}.{name}",router.host),description:a.description.clone(),online:false
    }).collect::<Vec<_>>()})
        .to_string()
        .len()
            <= crate::MAX_MESSAGE_BYTES
}

fn unique(router: &Router, wanted: &str, own: Option<&str>) -> Result<String> {
    hub::validate_component(wanted)?;
    let available =
        |candidate: &str| Some(candidate) == own || !router.agents.contains_key(candidate);
    if available(wanted) {
        return Ok(wanted.into());
    }
    for number in 2..=10000 {
        let suffix = format!("-{number}");
        let candidate = format!(
            "{}{}",
            &wanted[..wanted.len().min(63 - suffix.len())],
            suffix
        );
        if available(&candidate) {
            return Ok(candidate);
        }
    }
    Err("no free session name".into())
}

fn response(router: &Router, name: &str) -> Value {
    json!({"status":"ok","name":format!("{}.{name}",router.host),"description":router.agents[name].description,
        "ttlSecs":router.session_ttl.as_secs(),"heartbeatMs":(router.session_ttl.as_millis()/3).clamp(100,15000)})
}

fn prune(router: &mut Router) {
    let count = router.agents.len();
    router.agents.retain(|_, agent| agent.live());
    if count != router.agents.len() {
        router.generation += 1;
    }
}

pub(super) fn expire(router: &RwLock<Arc<Router>>) {
    let Ok(mut current) = router.write() else {
        return;
    };
    if current.agents.values().any(|agent| !agent.live()) {
        let mut next = (**current).clone();
        prune(&mut next);
        *current = Arc::new(next);
    }
}

pub(super) fn preserve(next: &mut Router, current: &Router) -> Result<()> {
    for (name, agent) in &current.agents {
        if agent.session.is_some() && agent.live() {
            if next.agents.contains_key(name) {
                return Err("static name collides with a live session".into());
            }
            if !credentials(next, agent.token.as_ref().unwrap(), &agent.sender) {
                return Err("static credentials collide with a live session".into());
            }
            next.agents.insert(name.clone(), agent.clone());
        }
    }
    if !fits(next) {
        return Err("relay capacity exceeded".into());
    }
    next.generation = current.generation + 1;
    Ok(())
}

fn perform(router: &mut Router, request: Request) -> Result<Value> {
    match request {
        Request::Register {
            id,
            name,
            description,
            inbox,
            inbox_token,
            sender_token,
        } => {
            if !state::is_id(&id) {
                return Err("invalid session identity".into());
            }
            hub::validate_component(&name)?;
            crate::directory::description(&description)?;
            let inbox = hub::loopback_endpoint(&inbox)?;
            let token = Token::from_text(&inbox_token)?;
            let sender = Token::from_text(&sender_token)?;
            if let Some((existing, agent)) = router
                .agents
                .iter_mut()
                .find(|(_, a)| a.session.as_ref().is_some_and(|s| s.id == id))
            {
                if !bool::from(agent.sender.bytes().ct_eq(sender.bytes()))
                    || !bool::from(agent.token.as_ref().unwrap().bytes().ct_eq(token.bytes()))
                    || agent.inbox.as_deref() != Some(&inbox)
                {
                    return Err("session credentials differ".into());
                }
                agent.session.as_mut().unwrap().expires = Instant::now() + router.session_ttl;
                let existing = existing.clone();
                return Ok(response(router, &existing));
            }
            if !credentials(router, &token, &sender) {
                return Err("session credentials must be independent".into());
            }
            let name = unique(router, &name, None)?;
            router.agents.insert(
                name.clone(),
                Agent {
                    inbox: Some(inbox),
                    description,
                    token: Some(token),
                    sender,
                    session: Some(Session {
                        id,
                        expires: Instant::now() + router.session_ttl,
                    }),
                },
            );
            if !fits(router) {
                return Err("relay capacity exceeded".into());
            }
            router.generation += 1;
            Ok(response(router, &name))
        }
        operation => {
            let (id, sender_token) = match &operation {
                Request::Heartbeat { id, sender_token }
                | Request::Profile {
                    id, sender_token, ..
                }
                | Request::Unregister { id, sender_token } => (id, sender_token),
                _ => unreachable!(),
            };
            let Some(name) = router.agents.iter().find_map(|(name, agent)| {
                agent
                    .session
                    .as_ref()
                    .filter(|s| &s.id == id)
                    .map(|_| name.clone())
            }) else {
                return Ok(json!({"status":"missing"}));
            };
            if !bool::from(
                router.agents[&name]
                    .sender
                    .bytes()
                    .ct_eq(sender_token.as_bytes()),
            ) {
                return Err("session credentials differ".into());
            }
            match operation {
                Request::Unregister { .. } => {
                    router.agents.remove(&name);
                    router.generation += 1;
                    Ok(json!({"status":"ok"}))
                }
                Request::Heartbeat { .. } => {
                    router
                        .agents
                        .get_mut(&name)
                        .unwrap()
                        .session
                        .as_mut()
                        .unwrap()
                        .expires = Instant::now() + router.session_ttl;
                    Ok(response(router, &name))
                }
                Request::Profile {
                    name: new_name,
                    description,
                    ..
                } => {
                    if let Some(description) = &description {
                        crate::directory::description(description)?;
                    }
                    let assigned = match new_name {
                        Some(wanted) => unique(router, &wanted, Some(&name))?,
                        None => name.clone(),
                    };
                    let mut agent = router.agents.remove(&name).unwrap();
                    if let Some(description) = description {
                        agent.description = description;
                    }
                    agent.session.as_mut().unwrap().expires = Instant::now() + router.session_ttl;
                    router.agents.insert(assigned.clone(), agent);
                    if !fits(router) {
                        return Err("relay capacity exceeded".into());
                    }
                    router.generation += 1;
                    Ok(response(router, &assigned))
                }
                _ => unreachable!(),
            }
        }
    }
}

pub(super) fn apply(router: &RwLock<Arc<Router>>, value: Value) -> Value {
    let Ok(request) = serde_json::from_value::<Request>(value) else {
        return json!({"status":"invalid","error":"invalid registration operation"});
    };
    if matches!(request, Request::Directory {}) {
        // The control server already verified the same OS user. Return only the
        // directory projection, never routing addresses or session credentials.
        let snapshot = match router.read() {
            Ok(current) => Arc::clone(&current),
            Err(_) => return json!({"status":"unavailable"}),
        };
        let response = snapshot.mesh_directory();
        return serde_json::from_str(&response.text)
            .unwrap_or_else(|_| json!({"status":"unavailable"}));
    }
    let Ok(mut current) = router.write() else {
        return json!({"status":"unavailable"});
    };
    let mut next = (**current).clone();
    prune(&mut next);
    match perform(&mut next, request) {
        Ok(value) => {
            *current = Arc::new(next);
            value
        }
        Err(_) => {
            json!({"status":"rejected","error":"invalid profile, credentials or relay capacity"})
        }
    }
}
