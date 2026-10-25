use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{
    Arc, Mutex, RwLock,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use crate::{Result, flytomesh, http, hub, journal, peer_chat_routes, state, token::Token};
#[cfg(feature = "dynamic-peers")]
use crate::{device_identity::DeviceIdentity, mesh_dynamic, mesh_pair::MeshRequest};
use serde::Deserialize;
use subtle::ConstantTimeEq;

#[path = "relay_mailbox.rs"]
mod mailbox;
#[path = "relay_sessions.rs"]
mod sessions;

#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
struct Config {
    host: String,
    hub: Option<String>,
    host_token_file: Option<PathBuf>,
    local_only: Option<bool>,
    chat_routes_seed_file: Option<PathBuf>,
    journal: Option<PathBuf>,
    state_dir: Option<PathBuf>,
    tunnel_script: Option<PathBuf>,
    tunnel_env: Option<PathBuf>,
    directory_report_secs: Option<u64>,
    control_socket: Option<PathBuf>,
    session_ttl_secs: Option<u64>,
    flytomesh: Option<flytomesh::Config>,
    #[cfg(feature = "dynamic-peers")]
    dynamic_mesh: Option<mesh_dynamic::Config>,
    #[cfg(feature = "dynamic-peers")]
    device_identity_file: Option<PathBuf>,
    #[cfg(feature = "dynamic-peers")]
    mesh_canary_only: Option<bool>,
    #[serde(default)]
    agents: Vec<AgentConfig>,
}
#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
struct AgentConfig {
    name: String,
    description: String,
    inbox: Option<String>,
    token_file: Option<PathBuf>,
    sender_token_file: PathBuf,
}
#[derive(Clone)]
struct Agent {
    inbox: Option<String>,
    description: String,
    token: Option<Token>,
    sender: Token,
    session: Option<sessions::Session>,
}

impl Agent {
    fn live(&self) -> bool {
        self.session
            .as_ref()
            .is_none_or(|s| s.expires > Instant::now())
    }
}

#[derive(Clone)]
struct Router {
    host: String,
    hub: Option<String>,
    host_token: Option<Token>,
    agents: HashMap<String, Agent>,
    log: PathBuf,
    generation: u64,
    session_ttl: Duration,
    mesh: Option<flytomesh::Plugin>,
    chat_routes: Option<Arc<peer_chat_routes::Routes>>,
    #[cfg(feature = "dynamic-peers")]
    dynamic: Option<Arc<mesh_dynamic::DynamicMesh>>,
    #[cfg(feature = "dynamic-peers")]
    device_identity: Option<Arc<DeviceIdentity>>,
    #[cfg(feature = "dynamic-peers")]
    mesh_canary_only: bool,
}

enum PeerRoute {
    Static(String, Token),
    #[cfg(feature = "dynamic-peers")]
    Dynamic(String, String),
}

impl PeerRoute {
    fn address(&self) -> &str {
        match self {
            Self::Static(address, _) => address,
            #[cfg(feature = "dynamic-peers")]
            Self::Dynamic(address, _) => address,
        }
    }

    fn send(&self, path: &str, from: &str, body: &str, timeout: Duration) -> Result<(u16, String)> {
        match self {
            Self::Static(address, token) => {
                http::post_diagnostic(address, path, token, Some(from), body, timeout)
            }
            #[cfg(feature = "dynamic-peers")]
            Self::Dynamic(address, authorization) => {
                http::post_mesh_diagnostic(address, path, authorization, from, body, timeout)
            }
        }
    }

    fn query(
        &self,
        path: &str,
        from: &str,
        body: &str,
        timeout: Duration,
    ) -> Result<(u16, String)> {
        match self {
            Self::Static(address, token) => {
                http::post_response_from(address, path, token, Some(from), body, timeout)
            }
            #[cfg(feature = "dynamic-peers")]
            Self::Dynamic(address, authorization) => {
                http::post_mesh_response(address, path, authorization, from, body, timeout)
            }
        }
    }
}

impl Router {
    fn has_mesh(&self) -> bool {
        self.mesh.is_some() || {
            #[cfg(feature = "dynamic-peers")]
            {
                self.dynamic.is_some()
            }
            #[cfg(not(feature = "dynamic-peers"))]
            {
                false
            }
        }
    }

    fn has_peer(&self, host: &str) -> bool {
        self.mesh.as_ref().is_some_and(|mesh| mesh.has_peer(host)) || {
            #[cfg(feature = "dynamic-peers")]
            {
                self.dynamic
                    .as_ref()
                    .is_some_and(|mesh| mesh.has_peer(host))
            }
            #[cfg(not(feature = "dynamic-peers"))]
            {
                false
            }
        }
    }

    fn configured_hosts(&self) -> Vec<String> {
        #[cfg(feature = "dynamic-peers")]
        if let Some(mesh) = &self.dynamic {
            return mesh.configured_hosts();
        }
        self.mesh
            .as_ref()
            .map(flytomesh::Plugin::configured_hosts)
            .unwrap_or_default()
    }

    fn peer_route(
        &self,
        host: &str,
        path: &str,
        from: &str,
        body: &str,
    ) -> Result<Option<PeerRoute>> {
        #[cfg(not(feature = "dynamic-peers"))]
        let _ = (path, from, body);
        #[cfg(feature = "dynamic-peers")]
        if let Some(mesh) = &self.dynamic {
            let request = MeshRequest {
                method: "POST",
                path,
                body: body.as_bytes(),
                from,
            };
            return Ok(mesh
                .target(host, request)?
                .map(|(address, auth)| PeerRoute::Dynamic(address, auth)));
        }
        Ok(self
            .mesh
            .as_ref()
            .and_then(|mesh| mesh.target(host))
            .map(|(address, token)| PeerRoute::Static(address, token)))
    }

    fn load(path: &Path) -> Result<(Self, Config)> {
        let config: Config =
            toml::from_str(&std::fs::read_to_string(path).map_err(|_| "cannot read relay config")?)
                .map_err(|_| "invalid relay TOML; check fields and syntax")?;
        hub::validate_component(&config.host)?;
        if !matches!(config.directory_report_secs.unwrap_or(15), 1..=3600) {
            return Err("directory report interval must be 1..3600 seconds".into());
        }
        if !matches!(config.session_ttl_secs.unwrap_or(45), 1..=3600) {
            return Err("session TTL must be 1..3600 seconds".into());
        }
        let (central, host_token) = match (&config.hub, &config.host_token_file) {
            (Some(address), Some(token_file)) => (
                Some(hub::loopback_endpoint(address)?),
                Some(Token::read(&hub::relative_to(path, token_file.clone()))?),
            ),
            (None, None) => (None, None),
            _ => return Err("hub and host_token_file must be supplied together".into()),
        };
        #[cfg(feature = "dynamic-peers")]
        let dynamic = config
            .dynamic_mesh
            .as_ref()
            .map(|mesh| mesh_dynamic::DynamicMesh::load(mesh, path, &config.host))
            .transpose()?;
        #[cfg(feature = "dynamic-peers")]
        if dynamic.is_some() && config.flytomesh.is_some() {
            return Err("configure static or dynamic FlytoMesh, not both".into());
        }
        #[cfg(feature = "dynamic-peers")]
        if config.mesh_canary_only.unwrap_or(false) && (central.is_none() || dynamic.is_none()) {
            return Err("mesh_canary_only requires both hub and dynamic_mesh".into());
        }
        if config.local_only.unwrap_or(false)
            && (central.is_some() || config.flytomesh.is_some() || {
                #[cfg(feature = "dynamic-peers")]
                {
                    dynamic.is_some()
                }
                #[cfg(not(feature = "dynamic-peers"))]
                {
                    false
                }
            })
        {
            return Err("local_only cannot be combined with a hub or mesh transport".into());
        }
        #[cfg(not(feature = "dynamic-peers"))]
        if central.is_none() && config.flytomesh.is_none() && !config.local_only.unwrap_or(false) {
            return Err("hubless relay requires mesh transport or local_only=true".into());
        }
        #[cfg(feature = "dynamic-peers")]
        if central.is_none()
            && config.flytomesh.is_none()
            && dynamic.is_none()
            && !config.local_only.unwrap_or(false)
        {
            return Err("hubless relay requires mesh transport or local_only=true".into());
        }
        let mesh = config
            .flytomesh
            .as_ref()
            .map(|mesh| flytomesh::Plugin::load(mesh, path, &config.host, host_token.as_ref()))
            .transpose()?;
        if config.agents.len() > 256 {
            return Err("relay supports at most 256 agents".into());
        }
        let log = match &config.journal {
            Some(log) => hub::relative_to(path, log.clone()),
            None => journal::default_path()?,
        };
        let mut agents: HashMap<String, Agent> = HashMap::new();
        for agent in &config.agents {
            hub::validate_component(&agent.name)?;
            crate::directory::description(&agent.description)?;
            let (inbox, token) = match (&agent.inbox, &agent.token_file) {
                (Some(inbox), Some(token)) => (
                    Some(hub::loopback_endpoint(inbox)?),
                    Some(Token::read(&hub::relative_to(path, token.clone()))?),
                ),
                (None, None) => (None, None),
                _ => return Err("relay inbox and token_file must be supplied together".into()),
            };
            let sender = Token::read(&hub::relative_to(path, agent.sender_token_file.clone()))?;
            if mesh.as_ref().is_some_and(|mesh| {
                mesh.token_conflicts(&sender)
                    || token
                        .as_ref()
                        .is_some_and(|token| mesh.token_conflicts(token))
            }) {
                return Err("FlytoMesh peer tokens must not match local agent tokens".into());
            }
            if agents.contains_key(&agent.name) {
                return Err("duplicate relay agent name".into());
            }
            if std::iter::once(&sender)
                .chain(token.iter())
                .any(|candidate| {
                    host_token.as_ref().is_some_and(|host_token| {
                        bool::from(candidate.bytes().ct_eq(host_token.bytes()))
                    }) || agents.values().any(|existing| {
                        std::iter::once(&existing.sender)
                            .chain(existing.token.iter())
                            .any(|other| bool::from(candidate.bytes().ct_eq(other.bytes())))
                    })
                })
                || token
                    .as_ref()
                    .is_some_and(|token| bool::from(token.bytes().ct_eq(sender.bytes())))
            {
                return Err(
                    "local sender tokens must be independent of the host and other senders".into(),
                );
            }
            agents.insert(
                agent.name.clone(),
                Agent {
                    inbox,
                    description: agent.description.clone(),
                    token,
                    sender,
                    session: None,
                },
            );
        }
        let worst = serde_json::json!({"agents":agents.iter().map(|(name,agent)|crate::directory::Report{name:format!("{}.{name}",config.host),description:agent.description.clone(),online:false}).collect::<Vec<_>>()});
        if worst.to_string().len() > crate::MAX_MESSAGE_BYTES {
            return Err("directory report exceeds 64 KiB; reduce agent descriptions/count".into());
        }
        Ok((
            Self {
                host: config.host.clone(),
                hub: central,
                host_token,
                agents,
                log,
                generation: 0,
                session_ttl: Duration::from_secs(config.session_ttl_secs.unwrap_or(45)),
                mesh,
                chat_routes: None,
                #[cfg(feature = "dynamic-peers")]
                dynamic,
                #[cfg(feature = "dynamic-peers")]
                device_identity: None,
                #[cfg(feature = "dynamic-peers")]
                mesh_canary_only: config.mesh_canary_only.unwrap_or(false),
            },
            config,
        ))
    }
    fn authenticate(&self, header: &[u8]) -> Option<http::SenderIdentity> {
        let mut identity = if self
            .host_token
            .as_ref()
            .is_some_and(|token| token.authorizes(header))
        {
            Some(http::SenderIdentity::Hub)
        } else {
            None
        };
        for (name, agent) in &self.agents {
            if agent.live() && agent.sender.authorizes(header) {
                identity = Some(http::SenderIdentity::Authenticated(format!(
                    "{}.{name}",
                    self.host
                )));
            }
        }
        identity
    }
    fn forward(
        &self,
        path: &str,
        text: &str,
        sender: Option<&str>,
        origin: http::Origin,
        mailbox: Option<&mailbox::Mailbox>,
    ) -> http::Response {
        #[cfg(feature = "dynamic-peers")]
        if path == "/mesh/status" {
            return if origin == http::Origin::Sender {
                self.dynamic
                    .as_ref()
                    .map(|mesh| http::Response::json(200, mesh.status()))
                    .unwrap_or_else(|| http::Response::new(404, "dynamic mesh is not configured"))
            } else {
                http::Response::new(403, "only local agents may inspect mesh status")
            };
        }
        #[cfg(feature = "dynamic-peers")]
        if path == "/device/identity" {
            return if origin == http::Origin::Sender {
                self.device_identity
                    .as_ref()
                    .map(|identity| {
                        http::Response::json(
                            200,
                            serde_json::json!({
                                "host":self.host,
                                "public_key":identity.public_key(),
                                "fingerprint":identity.fingerprint(),
                                "ownership":"not yet paired"
                            }),
                        )
                    })
                    .unwrap_or_else(|| {
                        http::Response::new(404, "device identity is not configured")
                    })
            } else {
                http::Response::new(403, "only local agents may inspect device identity")
            };
        }
        #[cfg(feature = "dynamic-peers")]
        if path == "/mesh/peer-check" {
            if origin != http::Origin::Sender {
                return http::Response::new(403, "only local agents may probe mesh peers");
            }
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Query {
                host: String,
            }
            let Ok(query) = serde_json::from_str::<Query>(text) else {
                return http::Response::new(400, "mesh peer check needs host");
            };
            if hub::validate_component(&query.host).is_err() || !self.has_peer(&query.host) {
                return http::Response::new(404, "unknown mesh peer");
            }
            let from = format!("{}.relay", self.host);
            let result = self
                .peer_route(&query.host, "/mesh/directory", &from, "{}")
                .and_then(|route| route.ok_or("mesh peer is offline".into()))
                .and_then(|route| {
                    route.query("/mesh/directory", &from, "{}", Duration::from_secs(3))
                });
            return match result {
                Ok((200, body))
                    if serde_json::from_str::<serde_json::Value>(&body)
                        .ok()
                        .is_some_and(|value| value["agents"].is_array()) =>
                {
                    http::Response::json(
                        200,
                        serde_json::json!({"host":query.host,"authenticated":true}),
                    )
                }
                _ => http::Response::new(503, "mesh peer authentication or directory query failed"),
            };
        }
        #[cfg(feature = "dynamic-peers")]
        if path == "/mesh/membership" {
            return if origin == http::Origin::Header {
                self.dynamic
                    .as_ref()
                    .and_then(|mesh| mesh.current_envelope())
                    .map(|envelope| http::Response::json(200, serde_json::json!(envelope)))
                    .unwrap_or_else(|| http::Response::new(503, "membership unavailable"))
            } else {
                http::Response::new(403, "membership is served only on the mesh listener")
            };
        }
        if path == "/whoami" {
            return if origin == http::Origin::Sender {
                http::Response::json(200, serde_json::json!({"name":sender}))
            } else {
                http::Response::new(
                    403,
                    "only local authenticated agents may identify themselves",
                )
            };
        }
        if path == "/mesh/directory" {
            if origin != http::Origin::Host {
                return http::Response::new(
                    403,
                    "only authenticated mesh hosts may query this directory",
                );
            }
            return http::Response::json(200, serde_json::json!({"agents":reports(self)}));
        }
        if path == "/mesh/chats/snapshot" {
            if origin != http::Origin::Host || self.hub.is_some() {
                return http::Response::new(
                    403,
                    "only authenticated peer hosts may query chat routes",
                );
            }
            return match self
                .chat_routes
                .as_ref()
                .and_then(|routes| routes.own_snapshot().ok())
            {
                Some(snapshot) => http::Response::json(200, serde_json::json!(snapshot)),
                None => http::Response::new(503, "peer chat routes unavailable"),
            };
        }
        if let Some((route, skip)) = path
            .strip_prefix("/mesh/receive/")
            .map(|route| (route, false))
            .or_else(|| path.strip_prefix("/mesh/skip/").map(|route| (route, true)))
        {
            if origin != http::Origin::Host {
                return http::Response::new(403, "only authenticated peers may submit messages");
            }
            let Some((id, rest)) = route.split_once('/') else {
                return http::Response::new(400, "peer message needs ID, sequence and target");
            };
            let Some((sequence, target)) = rest.split_once('/') else {
                return http::Response::new(400, "peer message needs sequence and target");
            };
            let Ok(sequence) = sequence.parse::<u64>() else {
                return http::Response::new(400, "invalid peer message sequence");
            };
            if !state::is_id(id) {
                return http::Response::new(400, "invalid peer message ID");
            }
            if hub::qualified(target).ok().map(|(host, _)| host) != Some(self.host.as_str()) {
                return http::Response::new(403, "peer message must target this host");
            }
            let (Some(sender), Some(mailbox)) = (sender, mailbox) else {
                return http::Response::new(503, "peer mailbox unavailable");
            };
            let result = if skip {
                mailbox.skip(id, sequence, sender, target, &self.log)
            } else {
                mailbox.receive(id, sequence, sender, target, text, &self.log)
            };
            return match result {
                Ok(status) => http::Response::new(status, "peer message saved"),
                Err(error) if error.to_string().contains("ID conflicts") => {
                    http::Response::new(409, "peer message ID conflicts with saved message")
                }
                Err(error) if error.to_string().contains("sequence gap") => {
                    http::Response::new(409, "peer message sequence gap")
                }
                Err(_) => http::Response::new(503, "peer message could not be saved"),
            };
        }
        if path == "/mesh/queue" {
            if origin != http::Origin::Sender {
                return http::Response::new(403, "only local agents may inspect the peer mailbox");
            }
            return match mailbox.and_then(|mailbox| mailbox.status().ok()) {
                Some(status) => http::Response::json(200, status),
                None => http::Response::new(503, "peer mailbox unavailable"),
            };
        }
        if let Some(route) = path.strip_prefix("/mesh/queue/") {
            if origin != http::Origin::Sender {
                return http::Response::new(403, "only local agents may resolve peer messages");
            }
            let Some((id, action)) = route.split_once('/') else {
                return http::Response::new(400, "peer queue action needs ID");
            };
            let (Some(sender), Some(mailbox)) = (sender, mailbox) else {
                return http::Response::new(503, "peer mailbox unavailable");
            };
            return match action {
                "resend" | "drop" => match mailbox.resolve(id, sender, action == "resend") {
                    Ok(()) => http::Response::new(200, "peer message resolution saved"),
                    Err(_) => http::Response::new(409, "peer message cannot be resolved"),
                },
                _ => http::Response::new(400, "unknown peer queue action"),
            };
        }
        if path == "/directory/list" || path.starts_with("/chats/") {
            if origin != http::Origin::Sender {
                return http::Response::new(
                    403,
                    "only local authenticated agents may query the directory",
                );
            }
            let (Some(hub), Some(token)) = (&self.hub, &self.host_token) else {
                return if path == "/directory/list" {
                    self.mesh_directory()
                } else {
                    let Some(routes) = &self.chat_routes else {
                        return http::Response::new(503, "peer chat routes unavailable");
                    };
                    if path == "/chats/list" {
                        let incomplete = self.refresh_chat_routes(routes);
                        let mut response = routes.handle_local(path, sender.unwrap_or(""), text);
                        if response.status == 200
                            && let Ok(mut value) =
                                serde_json::from_str::<serde_json::Value>(&response.text)
                        {
                            value["incompleteHosts"] = serde_json::json!(incomplete);
                            response = http::Response::json(200, value);
                        }
                        response
                    } else {
                        routes.handle_local(path, sender.unwrap_or(""), text)
                    }
                };
            };
            let response = http::post_response_from(
                hub,
                path,
                token,
                sender,
                text,
                Duration::from_secs(if self.has_mesh() && path == "/directory/list" {
                    3
                } else {
                    20
                }),
            );
            return match response {
                Ok((code @ (200 | 400 | 403 | 404 | 409 | 500), body)) => {
                    match serde_json::from_str(&body) {
                        Ok(value) => http::Response::json(code, value),
                        Err(_) if path.starts_with("/chats/") && code != 200 => {
                            http::Response::new(
                                code,
                                "chat route request rejected; inspect current registration",
                            )
                        }
                        Err(_) => http::Response::new(502, "invalid directory response"),
                    }
                }
                _ if path == "/directory/list" && self.has_mesh() => self.mesh_directory(),
                _ => http::Response::new(503, "hub directory unavailable"),
            };
        }
        let Some(target) = path.strip_prefix("/to/") else {
            return http::Response::new(404, "unknown relay endpoint");
        };
        let Some(sender) = sender else {
            return http::Response::new(400, "sender required");
        };
        if hub::validate_name(target).is_err() || hub::validate_name(sender).is_err() {
            return http::Response::new(400, "invalid logical name");
        }
        let local = hub::qualified(target)
            .ok()
            .filter(|(host, _)| *host == self.host)
            .map(|(_, agent)| agent);
        if matches!(origin, http::Origin::Hub | http::Origin::Host) && local.is_none() {
            return http::Response::new(403, "inbound host requests must target this host");
        }
        if !matches!(
            origin,
            http::Origin::Hub | http::Origin::Host | http::Origin::Sender
        ) {
            return http::Response::new(403, "untrusted relay source");
        }
        if self.hub.is_none() && origin == http::Origin::Sender {
            let Some(mailbox) = mailbox else {
                return http::Response::new(503, "peer mailbox unavailable");
            };
            if local.is_some() {
                let id = match state::id() {
                    Ok(id) => id,
                    Err(_) => return http::Response::new(500, "cannot create message ID"),
                };
                return match mailbox.receive(&id, 0, sender, target, text, &self.log) {
                    Ok(_) => http::Response::new(202, "message saved for local delivery"),
                    Err(_) => http::Response::new(503, "local message could not be saved"),
                };
            }
            if journal::append(&self.log, sender, target, text).is_err() {
                return http::Response::new(500, "relay journal failed; message not queued");
            }
            if hub::qualified(target)
                .ok()
                .is_none_or(|(host, _)| !self.has_peer(host))
            {
                return http::Response::new(404, "unknown peer; message saved");
            }
            return match mailbox.enqueue(sender, target, text) {
                Ok(_) => http::Response::new(202, "message saved for peer delivery"),
                Err(_) => http::Response::new(503, "peer message could not be queued"),
            };
        }
        if journal::append(&self.log, sender, target, text).is_err() {
            return http::Response::new(500, "relay journal failed; message not forwarded");
        }
        let outcome = if let Some(name) = local {
            let Some(agent) = self.agents.get(name).filter(|agent| agent.live()) else {
                return http::Response::new(404, "unknown local agent; message saved");
            };
            let (Some(inbox), Some(token)) = (&agent.inbox, &agent.token) else {
                return http::Response::new(405, "sender-only agent has no inbox; message saved");
            };
            http::post_diagnostic(
                inbox,
                "/",
                token,
                Some(sender),
                text,
                Duration::from_secs(5),
            )
        } else if let Some(route) = hub::qualified(target)
            .ok()
            .and_then(|(host, _)| self.peer_route(host, path, sender, text).ok().flatten())
            .filter(|_| {
                #[cfg(feature = "dynamic-peers")]
                {
                    !self.mesh_canary_only
                }
                #[cfg(not(feature = "dynamic-peers"))]
                {
                    true
                }
            })
            .filter(|route| flytomesh::reachable(route.address()))
        {
            // After a POST starts, an error may mean uncertain delivery. Never send the same
            // message through the hub as an automatic second attempt.
            route.send(path, sender, text, Duration::from_secs(20))
        } else if let (Some(hub), Some(token)) = (&self.hub, &self.host_token) {
            http::post_diagnostic(
                hub,
                path,
                token,
                Some(sender),
                text,
                Duration::from_secs(20),
            )
        } else {
            return http::Response::new(503, "peer unavailable; relay saved message");
        };
        match outcome {
            Ok((200, _)) => http::Response::new(200, "destination accepted message"),
            Ok((202, _)) => http::Response::new(202, "destination queued message"),
            Ok((status, reason)) if http::missing_route(status, &reason).is_some() => {
                http::Response::new(404, http::response_reason(&reason))
            }
            Ok(_) => http::Response::new(502, "destination rejected message; relay saved it"),
            Err(_) => http::Response::new(
                503,
                "destination unavailable or unconfirmed; relay saved it; do not blindly resend",
            ),
        }
    }

    fn mesh_directory(&self) -> http::Response {
        let mut rows: Vec<serde_json::Value> = reports(self).into_iter().map(report_row).collect();
        if !self.has_mesh() {
            return http::Response::json(200, serde_json::json!({"agents":rows,"nextCursor":null}));
        }
        let remote = Mutex::new(Vec::new());
        let answered = Mutex::new(std::collections::HashSet::new());
        let configured = self.configured_hosts();
        #[cfg(feature = "dynamic-peers")]
        if self
            .dynamic
            .as_ref()
            .is_some_and(|mesh| mesh.membership_expired())
        {
            for host in configured {
                rows.push(serde_json::json!({"name":format!("{host}.*"),
                    "description":"Signed membership expired; remote routing is paused",
                    "status":"offline","host_online":false,"inbox_reachable":false,
                    "last_seen":null,"host_marker":true,"membership_expired":true}));
            }
            rows.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
            return http::Response::json(200, serde_json::json!({"agents":rows,"nextCursor":null}));
        }
        let from = format!("{}.relay", self.host);
        let peers: Vec<_> = configured
            .iter()
            .filter_map(|host| {
                self.peer_route(host, "/mesh/directory", &from, "{}")
                    .ok()
                    .flatten()
                    .map(|route| (host.clone(), route))
            })
            .collect();
        std::thread::scope(|scope| {
            for (host, route) in peers {
                let remote = &remote;
                let answered = &answered;
                let from = &from;
                scope.spawn(move || {
                    if !flytomesh::reachable(route.address()) {
                        return;
                    }
                    let Ok((200, body)) =
                        route.query("/mesh/directory", from, "{}", Duration::from_secs(3))
                    else {
                        return;
                    };
                    let Ok(value) = serde_json::from_str::<serde_json::Value>(&body) else {
                        return;
                    };
                    let Some(entries) = value["agents"].as_array() else {
                        return;
                    };
                    if entries.len() > 256 {
                        return;
                    }
                    let mut checked = Vec::new();
                    for entry in entries {
                        let Ok(report) =
                            serde_json::from_value::<crate::directory::Report>(entry.clone())
                        else {
                            return;
                        };
                        if hub::qualified(&report.name).ok().map(|(name, _)| name)
                            != Some(host.as_str())
                            || crate::directory::description(&report.description).is_err()
                        {
                            return;
                        }
                        checked.push(report_row(report));
                    }
                    remote.lock().unwrap().extend(checked);
                    answered.lock().unwrap().insert(host);
                });
            }
        });
        rows.extend(remote.into_inner().unwrap());
        let answered = answered.into_inner().unwrap();
        for host in configured
            .into_iter()
            .filter(|host| !answered.contains(host))
        {
            rows.push(serde_json::json!({"name":format!("{host}.*"),
                "description":"Peer relay unreachable; agent roster unavailable",
                "status":"offline","host_online":false,"inbox_reachable":false,
                "last_seen":null,"host_marker":true}));
        }
        rows.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
        http::Response::json(200, serde_json::json!({"agents":rows,"nextCursor":null}))
    }

    fn refresh_chat_routes(&self, routes: &peer_chat_routes::Routes) -> Vec<String> {
        let hosts = self.configured_hosts();
        let allowed = hosts.iter().cloned().collect();
        if let Err(error) = routes.prune_remote(&allowed) {
            eprintln!("agent-inbox: peer chat route pruning failed: {error}");
        }
        let cursor = std::sync::atomic::AtomicUsize::new(0);
        let results = Mutex::new(Vec::new());
        std::thread::scope(|scope| {
            for _ in 0..hosts.len().min(8) {
                let results = &results;
                let cursor = &cursor;
                let hosts = &hosts;
                scope.spawn(move || {
                    loop {
                        let index = cursor.fetch_add(1, Ordering::Relaxed);
                        let Some(host) = hosts.get(index) else { break };
                        let from = format!("{}.relay", self.host);
                        let result = self
                            .peer_route(host, "/mesh/chats/snapshot", &from, "{}")
                            .and_then(|route| route.ok_or("peer is unavailable".into()))
                            .and_then(|route| {
                                route.query(
                                    "/mesh/chats/snapshot",
                                    &from,
                                    "{}",
                                    Duration::from_secs(2),
                                )
                            })
                            .and_then(|(status, body)| {
                                if status != 200 {
                                    return Err("peer chat route query failed".into());
                                }
                                Ok(serde_json::from_str::<peer_chat_routes::HostSnapshot>(
                                    &body,
                                )?)
                            });
                        results.lock().unwrap().push((host.clone(), result));
                    }
                });
            }
        });
        let mut incomplete = Vec::new();
        for (host, result) in results.into_inner().unwrap() {
            match result.and_then(|snapshot| routes.merge_remote(&host, snapshot).map(|_| ())) {
                Ok(()) => {}
                Err(error) => {
                    eprintln!("agent-inbox: peer chat routes from {host} unavailable: {error}");
                    incomplete.push(host);
                }
            }
        }
        incomplete.sort();
        incomplete
    }
}

fn report_row(report: crate::directory::Report) -> serde_json::Value {
    let seen = report
        .online
        .then(|| chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
    serde_json::json!({"name":report.name,"description":report.description,
        "status":if report.online {"online"} else {"offline"},
        "host_online":true,"inbox_reachable":report.online,"last_seen":seen})
}

fn reachable(address: &str) -> bool {
    let Ok(address) = address.parse() else {
        return false;
    };
    let Ok(mut stream) = TcpStream::connect_timeout(&address, Duration::from_millis(300)) else {
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
    let mut response = String::new();
    stream.take(512).read_to_string(&mut response).is_ok()
        && response.starts_with("HTTP/1.1 200 ")
        && response.contains("agent-inbox ok")
}

fn reports(router: &Router) -> Vec<crate::directory::Report> {
    let entries: Vec<_> = router
        .agents
        .iter()
        .filter(|(_, agent)| agent.live())
        .filter(|(_, agent)| agent.inbox.is_some())
        .collect();
    let result = Mutex::new(Vec::new());
    let cursor = std::sync::atomic::AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for _ in 0..entries.len().min(8) {
            scope.spawn(|| {
                loop {
                    let index = cursor.fetch_add(1, Ordering::Relaxed);
                    let Some((name, agent)) = entries.get(index) else {
                        break;
                    };
                    let row = crate::directory::Report {
                        name: format!("{}.{name}", router.host),
                        description: agent.description.clone(),
                        online: reachable(agent.inbox.as_deref().unwrap()),
                    };
                    result.lock().unwrap().push(row);
                }
            });
        }
    });
    result.into_inner().unwrap()
}

fn directory_loop(path: PathBuf, initial: Config, router: Arc<RwLock<Arc<Router>>>) {
    let mut previous = std::fs::read(&path).unwrap_or_default();
    let mut last = None;
    let mut waiting_for_tunnel = false;
    let mut reported_generation = None;
    while !STOP.load(Ordering::Relaxed) {
        let bytes = std::fs::read(&path).unwrap_or_default();
        let mut changed = false;
        if bytes != previous {
            match Router::load(&path) {
                Ok((mut next, config)) => {
                    // Merge and replacement are one mutation with registration/heartbeats.
                    let mut current = router.write().unwrap();
                    let same_host_token = match (&next.host_token, &current.host_token) {
                        (Some(next), Some(current)) => {
                            bool::from(next.bytes().ct_eq(current.bytes()))
                        }
                        (None, None) => true,
                        _ => false,
                    };
                    let same = next.host == current.host
                        && next.hub == current.hub
                        && same_host_token
                        && config.tunnel_script == initial.tunnel_script
                        && config.tunnel_env == initial.tunnel_env
                        && config.local_only == initial.local_only
                        && config.chat_routes_seed_file == initial.chat_routes_seed_file
                        && {
                            #[cfg(feature = "dynamic-peers")]
                            {
                                config.mesh_canary_only == initial.mesh_canary_only
                            }
                            #[cfg(not(feature = "dynamic-peers"))]
                            {
                                true
                            }
                        }
                        && config.state_dir == initial.state_dir;
                    let same = same
                        && config.control_socket == initial.control_socket
                        && config.session_ttl_secs == initial.session_ttl_secs
                        && {
                            #[cfg(feature = "dynamic-peers")]
                            {
                                config.device_identity_file == initial.device_identity_file
                            }
                            #[cfg(not(feature = "dynamic-peers"))]
                            {
                                true
                            }
                        }
                        && next.mesh.as_ref().map(|mesh| mesh.runtime_key())
                            == current.mesh.as_ref().map(|mesh| mesh.runtime_key());
                    let merge = sessions::preserve(&mut next, &current);
                    next.chat_routes = current.chat_routes.clone();
                    #[cfg(feature = "dynamic-peers")]
                    {
                        next.device_identity = current.device_identity.clone();
                    }
                    if !same {
                        eprintln!(
                            "agent-inbox: relay identity/tunnel changes require a service restart"
                        );
                    } else if merge.is_err() {
                        eprintln!(
                            "agent-inbox: static relay configuration conflicts with live sessions or capacity; retaining active routing"
                        );
                    } else {
                        *current = Arc::new(next);
                        changed = true;
                        eprintln!("agent-inbox: relay agent configuration reloaded");
                    }
                }
                Err(_) => eprintln!(
                    "agent-inbox: invalid relay configuration update; retaining active routing"
                ),
            }
            previous = bytes;
        }
        let needs_report = router.read().is_ok_and(|current| {
            current.hub.is_some()
                && (Some(current.generation) != reported_generation
                    || last.is_none_or(|when: Instant| {
                        when.elapsed()
                            >= Duration::from_secs(initial.directory_report_secs.unwrap_or(15))
                    }))
        });
        if changed || needs_report {
            let current = Arc::clone(&router.read().unwrap());
            if let (Some(hub), Some(host_token)) = (&current.hub, &current.host_token) {
                let body = serde_json::json!({"agents":reports(&current)}).to_string();
                let from = format!("{}.relay", current.host);
                match http::post_diagnostic(
                    hub,
                    "/directory/report",
                    host_token,
                    Some(&from),
                    &body,
                    Duration::from_secs(20),
                ) {
                    Ok((200, _)) => {
                        if waiting_for_tunnel {
                            eprintln!(
                                "agent-inbox: directory hub connection restored at {}; report accepted",
                                hub
                            );
                        }
                        waiting_for_tunnel = false;
                        last = Some(Instant::now());
                        reported_generation = Some(current.generation);
                    }
                    Ok((status, reason)) => {
                        waiting_for_tunnel = false;
                        eprintln!(
                            "agent-inbox: directory report unconfirmed: HTTP {status}: {}; will retry",
                            http::response_reason(&reason)
                        );
                        last = None;
                    }
                    Err(error)
                        if error.to_string()
                            == "cannot connect to inbox; message was not confirmed" =>
                    {
                        if !waiting_for_tunnel {
                            eprintln!(
                                "agent-inbox: directory waiting for tunnel: cannot connect to hub at {}",
                                hub
                            );
                        }
                        waiting_for_tunnel = true;
                        last = None;
                    }
                    Err(error) => {
                        waiting_for_tunnel = false;
                        eprintln!("agent-inbox: directory report unconfirmed: {error}; will retry");
                        last = None;
                    }
                }
            } else {
                last = Some(Instant::now());
                reported_generation = Some(current.generation);
            }
        }
        for _ in 0..10 {
            if STOP.load(Ordering::Relaxed) {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

fn mesh_loop(
    plugin: flytomesh::Plugin,
    running: Arc<AtomicBool>,
    authenticate: Arc<http::Authenticator>,
    handler: Arc<http::RouteHandler>,
) {
    let mut active: Option<(Ipv4Addr, Arc<AtomicBool>, std::thread::JoinHandle<()>)> = None;
    let mut bind_failed = false;
    while running.load(Ordering::Acquire) && !STOP.load(Ordering::Relaxed) {
        let desired = plugin.local_ip();
        if active
            .as_ref()
            .is_some_and(|(ip, _, thread)| desired != Some(*ip) || thread.is_finished())
        {
            let (_, control, thread) = active.take().unwrap();
            control.store(false, Ordering::Release);
            let _ = thread.join();
        }
        if active.is_none()
            && let Some(ip) = desired
        {
            let address = SocketAddrV4::new(ip, plugin.port());
            match TcpListener::bind(address) {
                Ok(listener) => {
                    let control = Arc::new(AtomicBool::new(true));
                    let serve_control = Arc::clone(&control);
                    let auth = Arc::clone(&authenticate);
                    let route = Arc::clone(&handler);
                    let thread = std::thread::spawn(move || {
                        if let Err(error) =
                            http::serve_authenticated(listener, serve_control, auth, route)
                        {
                            eprintln!("agent-inbox: FlytoMesh listener stopped: {error}");
                        }
                    });
                    eprintln!("agent-inbox: FlytoMesh listener ready on {address}");
                    bind_failed = false;
                    active = Some((ip, control, thread));
                }
                Err(error) if !bind_failed => {
                    eprintln!("agent-inbox: FlytoMesh listener waiting for {address}: {error}");
                    bind_failed = true;
                }
                Err(_) => {}
            }
        }
        for _ in 0..10 {
            if !running.load(Ordering::Acquire) || STOP.load(Ordering::Relaxed) {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    if let Some((_, control, thread)) = active {
        control.store(false, Ordering::Release);
        let _ = thread.join();
    }
}

#[cfg(feature = "dynamic-peers")]
fn mesh_interface(ip: Ipv4Addr) -> Result<(String, u32)> {
    use std::ffi::CStr;
    let mut addresses = std::ptr::null_mut();
    if unsafe { libc::getifaddrs(&mut addresses) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    struct List(*mut libc::ifaddrs);
    impl Drop for List {
        fn drop(&mut self) {
            unsafe { libc::freeifaddrs(self.0) };
        }
    }
    let _list = List(addresses);
    let mut current = addresses;
    while !current.is_null() {
        let entry = unsafe { &*current };
        if !entry.ifa_addr.is_null()
            && !entry.ifa_name.is_null()
            && unsafe { (*entry.ifa_addr).sa_family as i32 } == libc::AF_INET
        {
            let socket = unsafe { &*(entry.ifa_addr as *const libc::sockaddr_in) };
            if Ipv4Addr::from(u32::from_be(socket.sin_addr.s_addr)) == ip {
                let name = unsafe { CStr::from_ptr(entry.ifa_name) };
                let text = name.to_str()?.to_owned();
                #[cfg(target_os = "linux")]
                if text != "flytomesh" {
                    return Err("mesh IP is not on the flytomesh interface".into());
                }
                #[cfg(target_os = "macos")]
                if !text.starts_with("utun") {
                    return Err("mesh IP is not on a tunnel interface".into());
                }
                let index = unsafe { libc::if_nametoindex(name.as_ptr()) };
                if index == 0 {
                    return Err("mesh interface has no index".into());
                }
                return Ok((text, index));
            }
        }
        current = entry.ifa_next;
    }
    Err("FlytoMesh interface IP is unavailable".into())
}

#[cfg(feature = "dynamic-peers")]
fn bound_mesh_listener(
    ip: Ipv4Addr,
    port: u16,
    allow_port_fallback: bool,
    interface: &str,
    index: u32,
) -> Result<TcpListener> {
    use socket2::{Domain, Protocol, SockAddr, Socket, Type};
    use std::os::fd::AsRawFd;
    let socket = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP))?;
    socket.set_reuse_address(true)?;
    #[cfg(target_os = "macos")]
    {
        let value = index as libc::c_uint;
        if unsafe {
            libc::setsockopt(
                socket.as_raw_fd(),
                libc::IPPROTO_IP,
                libc::IP_BOUND_IF,
                &value as *const _ as *const libc::c_void,
                std::mem::size_of_val(&value) as libc::socklen_t,
            )
        } != 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    #[cfg(target_os = "linux")]
    {
        let name = std::ffi::CString::new(interface)?;
        if unsafe {
            libc::setsockopt(
                socket.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_BINDTODEVICE,
                name.as_ptr() as *const libc::c_void,
                name.as_bytes_with_nul().len() as libc::socklen_t,
            )
        } != 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    if let Err(error) = socket.bind(&SockAddr::from(std::net::SocketAddrV4::new(ip, port))) {
        if error.kind() != std::io::ErrorKind::AddrInUse || !allow_port_fallback {
            return Err(error.into());
        }
        socket.bind(&SockAddr::from(std::net::SocketAddrV4::new(ip, 0)))?;
    }
    socket.listen(128)?;
    #[cfg(not(target_os = "linux"))]
    let _ = interface;
    #[cfg(not(target_os = "macos"))]
    let _ = index;
    Ok(socket.into())
}

#[cfg(all(test, feature = "dynamic-peers"))]
mod dynamic_listener_tests {
    use super::*;

    #[test]
    fn occupied_preferred_port_falls_back_only_with_service_discovery() {
        let ip = Ipv4Addr::LOCALHOST;
        let held = TcpListener::bind((ip, 0)).unwrap();
        let port = held.local_addr().unwrap().port();
        #[cfg(target_os = "macos")]
        let interface = "lo0";
        #[cfg(target_os = "linux")]
        let interface = "lo";
        let name = std::ffi::CString::new(interface).unwrap();
        let index = unsafe { libc::if_nametoindex(name.as_ptr()) };
        assert_ne!(index, 0);
        assert!(bound_mesh_listener(ip, port, false, interface, index).is_err());
        let listener = bound_mesh_listener(ip, port, true, interface, index).unwrap();
        let actual = listener.local_addr().unwrap();
        assert_eq!(actual.ip(), ip);
        assert_ne!(actual.port(), port);
    }
}

#[cfg(feature = "dynamic-peers")]
type ActiveDynamicListener = (
    (Ipv4Addr, u32, u64),
    Arc<AtomicBool>,
    std::thread::JoinHandle<()>,
    Option<crate::mesh_services::Registration>,
);

#[cfg(feature = "dynamic-peers")]
fn dynamic_mesh_loop(
    plugin: Arc<mesh_dynamic::DynamicMesh>,
    identity: Arc<DeviceIdentity>,
    running: Arc<AtomicBool>,
    handler: Arc<http::RouteHandler>,
) {
    let mut active: Option<ActiveDynamicListener> = None;
    let mut bind_failed = false;
    while running.load(Ordering::Acquire) && !STOP.load(Ordering::Relaxed) {
        let registration_failed = if let Some((_, _, _, Some(registration))) = active.as_mut() {
            if let Err(error) = registration.heartbeat() {
                eprintln!("agent-inbox: FlytoMesh service registration lost: {error}");
                true
            } else {
                false
            }
        } else {
            false
        };
        let desired = plugin.local_binding().and_then(|(ip, revision)| {
            mesh_interface(ip)
                .ok()
                .map(|(name, index)| ((ip, index, revision), name))
        });
        if active.as_ref().is_some_and(|(bound, _, thread, _)| {
            registration_failed
                || desired.as_ref().map(|(key, _)| key) != Some(bound)
                || thread.is_finished()
        }) {
            let (_, control, thread, registration) = active.take().unwrap();
            plugin.set_listener_port(0);
            drop(registration);
            control.store(false, Ordering::Release);
            let _ = thread.join();
        }
        if active.is_none()
            && let Some(((ip, index, revision), name)) = desired
        {
            match bound_mesh_listener(ip, plugin.port(), plugin.service_discovery(), &name, index) {
                Ok(listener) => {
                    let actual_port = match listener.local_addr() {
                        Ok(address) => address.port(),
                        Err(error) => {
                            eprintln!(
                                "agent-inbox: dynamic mesh listener address unavailable: {error}"
                            );
                            std::thread::sleep(Duration::from_secs(1));
                            continue;
                        }
                    };
                    let registration = if plugin.service_discovery() {
                        match crate::mesh_services::Registration::create(
                            plugin.service_directory(),
                            actual_port,
                            &identity.fingerprint(),
                        ) {
                            Ok(registration) => Some(registration),
                            Err(error) => {
                                if !bind_failed {
                                    eprintln!(
                                        "agent-inbox: FlytoMesh service registration unavailable: {error}"
                                    );
                                }
                                bind_failed = true;
                                std::thread::sleep(Duration::from_secs(1));
                                continue;
                            }
                        }
                    } else {
                        None
                    };
                    let control = Arc::new(AtomicBool::new(true));
                    let serve_control = Arc::clone(&control);
                    let auth_plugin = Arc::clone(&plugin);
                    let authenticate: Arc<http::MeshAuthenticator> =
                        Arc::new(move |header, method, path, body, from, peer| {
                            auth_plugin.authenticate(
                                header,
                                MeshRequest {
                                    method,
                                    path,
                                    body,
                                    from,
                                },
                                peer,
                            )
                        });
                    let route = Arc::clone(&handler);
                    let thread = std::thread::spawn(move || {
                        if let Err(error) = http::serve_mesh_authenticated(
                            listener,
                            serve_control,
                            authenticate,
                            route,
                        ) {
                            eprintln!("agent-inbox: dynamic mesh listener stopped: {error}");
                        }
                    });
                    eprintln!("agent-inbox: dynamic mesh listener ready on {ip}:{actual_port}");
                    plugin.set_listener_port(actual_port);
                    bind_failed = false;
                    active = Some(((ip, index, revision), control, thread, registration));
                }
                Err(error) if !bind_failed => {
                    eprintln!("agent-inbox: dynamic mesh listener unavailable: {error}");
                    bind_failed = true;
                }
                Err(_) => {}
            }
        }
        for _ in 0..10 {
            if !running.load(Ordering::Acquire) || STOP.load(Ordering::Relaxed) {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    if let Some((_, control, thread, registration)) = active {
        plugin.set_listener_port(0);
        drop(registration);
        control.store(false, Ordering::Release);
        let _ = thread.join();
    }
}

#[cfg(feature = "dynamic-peers")]
fn membership_loop(plugin: Arc<mesh_dynamic::DynamicMesh>, running: Arc<AtomicBool>) {
    let mut cursor = 0usize;
    let mut update_error = None;
    let mut warned_revision = 0;
    let mut warned = std::collections::HashSet::new();
    while running.load(Ordering::Acquire) && !STOP.load(Ordering::Relaxed) {
        match plugin.poll_update_file() {
            Ok(true) => {
                eprintln!("agent-inbox: newer signed membership accepted from local file");
                update_error = None;
            }
            Ok(false) => update_error = None,
            Err(error) => {
                let reason = error.to_string();
                if update_error.as_deref() != Some(reason.as_str()) {
                    eprintln!("agent-inbox: membership update file rejected: {reason}");
                    update_error = Some(reason);
                }
            }
        }
        if let Some((revision, expires)) = plugin.membership_timing() {
            if revision != warned_revision {
                warned_revision = revision;
                warned.clear();
            }
            let remaining = expires.saturating_sub(chrono::Utc::now().timestamp());
            let warning = if remaining <= 0 {
                Some((0, "expired"))
            } else if remaining <= 24 * 3600 {
                Some((1, "1 day"))
            } else if remaining <= 7 * 24 * 3600 {
                Some((7, "7 days"))
            } else if remaining <= 14 * 24 * 3600 {
                Some((14, "14 days"))
            } else {
                None
            };
            if let Some((threshold, label)) = warning
                && warned.insert(threshold)
            {
                eprintln!(
                    "agent-inbox: signed membership revision {revision} expires within {label}; renew before remote routing pauses"
                );
            }
        }
        let targets = plugin.gossip_targets();
        if !targets.is_empty() {
            for offset in 0..targets.len().min(8) {
                let address = &targets[(cursor + offset) % targets.len()];
                let Ok((200, body)) =
                    http::get_mesh_response(address, "/mesh/membership", Duration::from_secs(3))
                else {
                    continue;
                };
                let Ok(envelope) = crate::mesh_membership::parse_envelope(body.trim().as_bytes())
                else {
                    continue;
                };
                if plugin
                    .accept_envelope(&envelope)
                    .is_ok_and(|accepted| accepted)
                {
                    eprintln!("agent-inbox: newer signed membership learned from mesh peer");
                }
            }
            cursor = (cursor + 8) % targets.len();
        }
        for _ in 0..50 {
            if !running.load(Ordering::Acquire) || STOP.load(Ordering::Relaxed) {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

static STOP: AtomicBool = AtomicBool::new(false);
extern "C" fn stop(_: libc::c_int) {
    STOP.store(true, Ordering::Relaxed);
}
struct Tunnel(Arc<Mutex<Child>>);
impl Drop for Tunnel {
    fn drop(&mut self) {
        // The supervisor owns its guard/SSH groups and handles TERM before we fall back to KILL.
        let Ok(mut child) = self.0.lock() else {
            return;
        };
        if child.try_wait().ok().flatten().is_some() {
            return;
        }
        unsafe {
            libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if child.try_wait().ok().flatten().is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = child.kill();
        let _ = child.wait();
    }
}

pub(crate) fn validate_for_daemon(path: &Path, no_tunnel: bool) -> Result<Option<PathBuf>> {
    let (_, config) = Router::load(path)?;
    if config.hub.is_none() && !no_tunnel {
        return Err("hubless relay requires no_tunnel=true".into());
    }
    if !no_tunnel {
        for file in [config.tunnel_script.as_ref(), config.tunnel_env.as_ref()] {
            if !file.is_some_and(|p| hub::relative_to(path, p.clone()).is_file()) {
                return Err(
                    "desktop relay requires readable tunnel_script/tunnel_env or no_tunnel=true"
                        .into(),
                );
            }
        }
    }
    Ok(config.control_socket.map(|p| hub::relative_to(path, p)))
}

pub fn run(mut args: impl Iterator<Item = String>) -> Result<()> {
    let mut listen = None;
    let mut path = None;
    let mut state_dir = None;
    let mut no_tunnel = false;
    let mut parent = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                println!(
                    "agent-inbox relay --listen <loopback-address> --config <toml> [--state-dir <path>] [--no-tunnel]"
                );
                return Ok(());
            }
            "--no-tunnel" => no_tunnel = true,
            "--parent" => parent = Some(args.next().ok_or("missing relay parent")?.parse::<u32>()?),
            "--listen" | "--config" | "--state-dir" => {
                let value = args.next().ok_or("missing relay option value")?;
                match arg.as_str() {
                    "--listen" => listen = Some(http::listen_address(&value)?),
                    "--config" => path = Some(PathBuf::from(value)),
                    _ => state_dir = Some(PathBuf::from(value)),
                }
            }
            _ => return Err("unknown relay option".into()),
        }
    }
    let listen = listen.ok_or("relay requires --listen")?;
    if !listen.ip().is_loopback() {
        return Err("relay must bind loopback".into());
    }
    let path = std::fs::canonicalize(path.ok_or("relay requires --config")?)?;
    let (mut router, config) = Router::load(&path)?;
    if router.hub.is_none() && !no_tunnel {
        return Err("hubless relay requires --no-tunnel".into());
    }
    let initial = config.clone();
    let root = match state_dir.or(config.state_dir.clone()) {
        Some(pathbuf) => hub::relative_to(&path, pathbuf),
        None => journal::default_path()?
            .parent()
            .ok_or("state directory unavailable")?
            .to_owned(),
    };
    if router.hub.is_none() {
        let seed = config
            .chat_routes_seed_file
            .as_ref()
            .map(|value| hub::relative_to(&path, value.clone()));
        router.chat_routes = Some(Arc::new(peer_chat_routes::Routes::open(
            root.join(format!("host-{}.chat-routes.json", router.host)),
            &router.host,
            seed.as_deref(),
        )?));
    } else if config.chat_routes_seed_file.is_some() {
        return Err("chat_routes_seed_file requires a hubless relay".into());
    }
    let lock_path = root.join(format!("host-{}.relay-lock", router.host));
    state::parent(&lock_path)?;
    let _instance = state::open_private(&lock_path, true)?;
    _instance
        .try_lock()
        .map_err(|_| "a relay for this host is already running")?;
    #[cfg(feature = "dynamic-peers")]
    {
        let identity_path = config
            .device_identity_file
            .as_ref()
            .map(|value| hub::relative_to(&path, value.clone()))
            .or_else(|| {
                router
                    .dynamic
                    .as_ref()
                    .map(|_| root.join(format!("host-{}.device.key", router.host)))
            });
        if let Some(identity_path) = identity_path {
            router.device_identity =
                Some(Arc::new(DeviceIdentity::open_or_create(&identity_path)?));
        }
    }
    let listener = TcpListener::bind(listen)?;
    let address = listener.local_addr()?;
    for signal in [libc::SIGHUP, libc::SIGINT, libc::SIGTERM] {
        if unsafe { libc::signal(signal, stop as *const () as libc::sighandler_t) } == libc::SIG_ERR
        {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    crate::daemon::watch_parent(parent, &STOP)?;
    let _tunnel = if no_tunnel {
        None
    } else {
        let script = hub::relative_to(
            &path,
            config
                .tunnel_script
                .ok_or("relay requires tunnel_script or explicit --no-tunnel")?,
        );
        let env = hub::relative_to(&path, config.tunnel_env.ok_or("relay requires tunnel_env")?);
        Some(Tunnel(Arc::new(Mutex::new(
            Command::new("sh")
                .arg(script)
                .args(["--config"])
                .arg(env)
                .env("INBOX_SUPERVISOR_PARENT", std::process::id().to_string())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .spawn()?,
        ))))
    };
    let mailbox = if router.has_mesh() || router.hub.is_none() {
        Some(mailbox::Mailbox::open(
            root.join(format!("host-{}.peer-mailbox.json", router.host)),
        )?)
    } else {
        None
    };
    let running = Arc::new(AtomicBool::new(true));
    let control = Arc::clone(&running);
    let monitor = _tunnel.as_ref().map(|tunnel| Arc::clone(&tunnel.0));
    std::thread::spawn(move || {
        while !STOP.load(Ordering::Relaxed) {
            if let Some(child) = &monitor
                && child
                    .lock()
                    .ok()
                    .and_then(|mut child| child.try_wait().ok().flatten())
                    .is_some()
            {
                eprintln!("agent-inbox: tunnel supervisor exited; relay service must restart");
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        control.store(false, Ordering::Release);
    });
    let router = Arc::new(RwLock::new(Arc::new(router)));
    let socket = match initial.control_socket.clone() {
        Some(socket) => hub::relative_to(&path, socket),
        None => crate::local_control::default_path(address.port())?,
    };
    let registrations = Arc::clone(&router);
    let expiry = Arc::clone(&router);
    let _local = crate::local_control::Server::start(
        socket,
        Arc::new(move |value| sessions::apply(&registrations, value)),
        Arc::new(move || sessions::expire(&expiry)),
    )?;
    let registry = Arc::clone(&router);
    std::thread::spawn(move || directory_loop(path, initial, registry));
    let mesh = router.read().ok().and_then(|router| router.mesh.clone());
    #[cfg(feature = "dynamic-peers")]
    let dynamic = router.read().ok().and_then(|router| router.dynamic.clone());
    let mesh_auth_state = Arc::clone(&router);
    let mesh_authenticate: Arc<http::Authenticator> = Arc::new(move |header: &[u8]| {
        mesh_auth_state
            .read()
            .ok()?
            .mesh
            .as_ref()?
            .authenticate(header)
    });
    let auth = Arc::clone(&router);
    let authenticate: Arc<http::Authenticator> =
        Arc::new(move |header: &[u8]| auth.read().ok()?.authenticate(header));
    let routes = Arc::clone(&router);
    let handler_mailbox = mailbox.clone();
    let handler: Arc<http::RouteHandler> = Arc::new(
        move |path: &str, text: &str, sender: Option<&str>, origin: http::Origin| {
            let snapshot = routes.read().map(|router| Arc::clone(&router));
            match snapshot {
                Ok(router) => {
                    router.forward(path, text, sender, origin, handler_mailbox.as_deref())
                }
                Err(_) => http::Response::new(500, "relay routing unavailable"),
            }
        },
    );
    let mailbox_threads = mailbox
        .as_ref()
        .map(|mailbox| mailbox.start(Arc::clone(&router), Arc::clone(&running)))
        .unwrap_or_default();
    let mesh_thread = mesh.map(|plugin| {
        let mesh_running = Arc::clone(&running);
        let mesh_handler = Arc::clone(&handler);
        std::thread::spawn(move || mesh_loop(plugin, mesh_running, mesh_authenticate, mesh_handler))
    });
    #[cfg(feature = "dynamic-peers")]
    let membership_thread = dynamic.as_ref().map(|plugin| {
        let mesh_running = Arc::clone(&running);
        let plugin = Arc::clone(plugin);
        std::thread::spawn(move || membership_loop(plugin, mesh_running))
    });
    #[cfg(feature = "dynamic-peers")]
    let dynamic_identity = if dynamic.is_some() {
        Some(
            router
                .read()
                .map_err(|_| "relay identity lock failed")?
                .device_identity
                .clone()
                .ok_or("dynamic mesh requires a device identity")?,
        )
    } else {
        None
    };
    #[cfg(feature = "dynamic-peers")]
    let dynamic_thread = dynamic.map(|plugin| {
        let mesh_running = Arc::clone(&running);
        let mesh_handler = Arc::clone(&handler);
        let identity = dynamic_identity.expect("dynamic identity checked");
        std::thread::spawn(move || dynamic_mesh_loop(plugin, identity, mesh_running, mesh_handler))
    });
    eprintln!("agent-inbox: listening on http://{address}/");
    let result = http::serve_authenticated(listener, Arc::clone(&running), authenticate, handler);
    running.store(false, Ordering::Release);
    for thread in mailbox_threads {
        let _ = thread.join();
    }
    if let Some(thread) = mesh_thread {
        let _ = thread.join();
    }
    #[cfg(feature = "dynamic-peers")]
    if let Some(thread) = dynamic_thread {
        let _ = thread.join();
    }
    #[cfg(feature = "dynamic-peers")]
    if let Some(thread) = membership_thread {
        let _ = thread.join();
    }
    result
}

#[cfg(test)]
mod local_only_tests {
    use super::*;

    #[test]
    fn local_only_relay_keeps_a_durable_inbox_without_mesh_or_hub() {
        let root = std::env::temp_dir().join(format!("local-only-{}", state::id().unwrap()));
        state::parent(&root.join("relay.toml")).unwrap();
        let config = root.join("relay.toml");
        std::fs::write(
            &config,
            format!(
                "host = 'local'\nlocal_only = true\njournal = '{}'\nstate_dir = '{}'\n",
                root.join("relay.md").display(),
                root.join("state").display()
            ),
        )
        .unwrap();
        let (mut router, _) = Router::load(&config).unwrap();
        router.chat_routes = Some(Arc::new(
            peer_chat_routes::Routes::open(root.join("chat-routes.json"), "local", None).unwrap(),
        ));
        assert!(!router.has_mesh());
        #[cfg(feature = "dynamic-peers")]
        {
            assert_eq!(
                router
                    .forward(
                        "/mesh/peer-check",
                        r#"{"host":"other"}"#,
                        Some("local.sender"),
                        http::Origin::Sender,
                        None
                    )
                    .status,
                404
            );
            assert_eq!(
                router
                    .forward(
                        "/mesh/peer-check",
                        r#"{"host":"other"}"#,
                        Some("other.relay"),
                        http::Origin::Host,
                        None
                    )
                    .status,
                403
            );
        }
        assert_eq!(
            router
                .forward(
                    "/chats/register",
                    r#"{"chat":"oc_test"}"#,
                    Some("local.sender"),
                    http::Origin::Sender,
                    None
                )
                .status,
            200
        );
        assert_eq!(
            router
                .forward(
                    "/mesh/chats/snapshot",
                    "{}",
                    Some("peer.relay"),
                    http::Origin::Sender,
                    None
                )
                .status,
            403
        );
        let remote = router.forward(
            "/mesh/chats/snapshot",
            "{}",
            Some("peer.relay"),
            http::Origin::Host,
            None,
        );
        assert_eq!(remote.status, 200);
        assert!(remote.text.contains("local.sender"));
        assert_eq!(
            router
                .forward(
                    "/directory/list",
                    "{}",
                    Some("local.sender"),
                    http::Origin::Sender,
                    None
                )
                .status,
            200
        );
        let mailbox = mailbox::Mailbox::open(root.join("mailbox.json")).unwrap();
        assert_eq!(
            router
                .forward(
                    "/to/local.receiver",
                    "hello",
                    Some("local.sender"),
                    http::Origin::Sender,
                    Some(&mailbox),
                )
                .status,
            202
        );
        assert_eq!(
            mailbox.status().unwrap()["incoming"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        drop(mailbox);
        #[cfg(feature = "dynamic-peers")]
        {
            let mut text = std::fs::read_to_string(&config).unwrap();
            text.push_str("mesh_canary_only = true\n");
            std::fs::write(&config, text).unwrap();
            assert!(Router::load(&config).is_err());
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
