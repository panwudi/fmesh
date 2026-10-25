use std::collections::HashMap;
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use serde::Deserialize;
use subtle::ConstantTimeEq;

use crate::{Result, http, journal, token::Token};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    #[serde(default)]
    peers: Vec<PeerConfig>,
    #[serde(default)]
    senders: Vec<SenderConfig>,
    #[serde(default)]
    hosts: Vec<HostConfig>,
    journal: Option<PathBuf>,
    directory_file: Option<PathBuf>,
    directory_ttl_secs: Option<u64>,
    chat_routes_file: Option<PathBuf>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PeerConfig {
    name: String,
    inbox: String,
    token_file: PathBuf,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SenderConfig {
    name: String,
    token_file: PathBuf,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HostConfig {
    name: String,
    relay: String,
    token_file: PathBuf,
}

struct Peer {
    inbox: String,
    token: Token,
}
struct Sender {
    name: String,
    token: Token,
}

pub struct Router {
    peers: HashMap<String, Peer>,
    senders: Vec<Sender>,
    hosts: HashMap<String, Peer>,
    log: PathBuf,
    directory: crate::directory::Directory,
    chat_routes: crate::chat_routes::Routes,
}

pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 128
        || !name.as_bytes()[0].is_ascii_alphanumeric()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_.-".contains(&byte))
    {
        return Err("logical names must be 1..128 ASCII letters, digits, dots, underscores or hyphens, starting with a letter or digit".into());
    }
    Ok(())
}

pub(crate) fn relative_to(config: &Path, path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        config.parent().unwrap_or(Path::new(".")).join(path)
    }
}

pub fn validate_component(name: &str) -> Result<()> {
    validate_name(name)?;
    if name.contains('.') || name.len() > 63 {
        return Err("host/agent components must be 1..63 ASCII characters without dots".into());
    }
    Ok(())
}

pub fn qualified(name: &str) -> Result<(&str, &str)> {
    let (host, agent) = name.split_once('.').ok_or("expected <host>.<agent>")?;
    validate_component(host)?;
    validate_component(agent)?;
    Ok((host, agent))
}

pub(crate) fn loopback_endpoint(value: &str) -> Result<String> {
    let endpoint = value
        .strip_prefix("http://")
        .unwrap_or(value)
        .trim_end_matches('/');
    let address: SocketAddr = endpoint
        .parse()
        .map_err(|_| "endpoint must be a numeric loopback IP:port")?;
    if !address.ip().is_loopback() || address.port() == 0 {
        return Err("endpoint must use a nonzero loopback port".into());
    }
    Ok(address.to_string())
}

impl Router {
    pub fn load(path: &Path, log_override: Option<PathBuf>) -> Result<Self> {
        let text = std::fs::read_to_string(path).map_err(|_| "cannot read hub config")?;
        // Do not print parser diagnostics: malformed input might contain pasted credentials.
        let config: Config =
            toml::from_str(&text).map_err(|_| "invalid hub TOML; check syntax and field names")?;
        if config.hosts.is_empty() && (config.peers.is_empty() || config.senders.is_empty()) {
            return Err("hub config requires hosts or legacy peers/senders".into());
        }
        if config.peers.len() > 256 || config.senders.len() > 256 || config.hosts.len() > 256 {
            return Err("hub config supports at most 256 peers and 256 senders".into());
        }
        let log = match log_override {
            Some(log) => log,
            None => match config.journal {
                Some(log) => relative_to(path, log),
                None => journal::default_path()?,
            },
        };
        let mut peers = HashMap::new();
        for peer in config.peers {
            validate_name(&peer.name)?;
            let endpoint = peer
                .inbox
                .strip_prefix("http://")
                .unwrap_or(&peer.inbox)
                .trim_end_matches('/');
            let address: SocketAddr = endpoint
                .parse()
                .map_err(|_| "peer inbox must be a numeric loopback IP:port")?;
            if !address.ip().is_loopback() || address.port() == 0 {
                return Err("peer inbox must use a nonzero port on loopback".into());
            }
            let token = Token::read(&relative_to(path, peer.token_file))
                .map_err(|error| format!("peer {}: {error}", peer.name))?;
            if peers
                .insert(
                    peer.name,
                    Peer {
                        inbox: address.to_string(),
                        token,
                    },
                )
                .is_some()
            {
                return Err("duplicate peer name in hub config".into());
            }
        }
        let mut senders: Vec<Sender> = Vec::new();
        for sender in config.senders {
            validate_name(&sender.name)?;
            let token = Token::read(&relative_to(path, sender.token_file))
                .map_err(|error| format!("sender {}: {error}", sender.name))?;
            if senders.iter().any(|existing| existing.name == sender.name) {
                return Err("duplicate sender name in hub config".into());
            }
            if senders
                .iter()
                .any(|existing| bool::from(existing.token.bytes().ct_eq(token.bytes())))
            {
                return Err(
                    "hub token is shared by more than one sender; use independent tokens".into(),
                );
            }
            senders.push(Sender {
                name: sender.name,
                token,
            });
        }
        let mut hosts: HashMap<String, Peer> = HashMap::new();
        for host in config.hosts {
            validate_component(&host.name)?;
            let inbox = loopback_endpoint(&host.relay)?;
            let token = Token::read(&relative_to(path, host.token_file))?;
            if hosts.contains_key(&host.name) {
                return Err("duplicate host name".into());
            }
            if hosts
                .values()
                .any(|peer| bool::from(peer.token.bytes().ct_eq(token.bytes())))
                || senders
                    .iter()
                    .any(|sender| bool::from(sender.token.bytes().ct_eq(token.bytes())))
                || peers
                    .values()
                    .any(|peer| bool::from(peer.token.bytes().ct_eq(token.bytes())))
            {
                return Err("host tokens must be independent of other host/sender tokens".into());
            }
            let prefix = format!("{}.", host.name);
            if peers.keys().any(|name| name.starts_with(&prefix))
                || senders
                    .iter()
                    .any(|sender| sender.name.starts_with(&prefix))
            {
                return Err("legacy name overlaps a registered host prefix".into());
            }
            hosts.insert(host.name, Peer { inbox, token });
        }
        let directory_path = config
            .directory_file
            .map(|pathbuf| relative_to(path, pathbuf))
            .unwrap_or_else(|| {
                if !log.is_dir() && log.file_name().is_some() {
                    log.with_extension("directory.json")
                } else {
                    path.with_extension("directory.json")
                }
            });
        let chat_routes = crate::chat_routes::Routes::open(
            config
                .chat_routes_file
                .map(|p| relative_to(path, p))
                .unwrap_or_else(|| directory_path.with_extension("chat-routes.json")),
        )?;
        let directory = crate::directory::Directory::open(
            directory_path,
            &hosts.keys().cloned().collect(),
            config.directory_ttl_secs.unwrap_or(45),
        )?;
        Ok(Self {
            peers,
            senders,
            hosts,
            log,
            directory,
            chat_routes,
        })
    }

    pub fn authenticate(&self, header: &[u8]) -> Option<http::SenderIdentity> {
        let mut matched = None;
        // Compare against every sender instead of returning at the first token match.
        for sender in &self.senders {
            if sender.token.authorizes(header) {
                matched = Some(&sender.name);
            }
        }
        let mut identity = matched.map(|name| http::SenderIdentity::Authenticated(name.clone()));
        for (host, peer) in &self.hosts {
            if peer.token.authorizes(header) {
                identity = Some(http::SenderIdentity::HostPrefix(host.clone()));
            }
        }
        identity
    }

    pub fn forward(&self, path: &str, text: &str, sender: &str) -> http::Response {
        let Some(name) = path.strip_prefix("/to/") else {
            return http::Response::new(404, "unknown endpoint");
        };
        if validate_name(name).is_err() {
            return http::Response::new(400, "invalid peer name");
        }
        if journal::append(&self.log, sender, name, text).is_err() {
            eprintln!("agent-inbox: hub journal write failed; forwarding skipped");
            return http::Response::new(500, "journal write failed; message was not forwarded");
        }
        let (peer, forward_path) = if let Some(peer) = self.peers.get(name) {
            (peer, "/".to_owned())
        } else if let Ok((host, _)) = qualified(name) {
            match self.hosts.get(host) {
                Some(peer) => (peer, format!("/to/{name}")),
                None => return http::Response::new(404, "unknown host; message saved"),
            }
        } else {
            return http::Response::new(404, "unknown peer; message saved");
        };
        match http::post_diagnostic(
            &peer.inbox,
            &forward_path,
            &peer.token,
            Some(sender),
            text,
            Duration::from_secs(10),
        ) {
            Ok((200, _)) => http::Response::new(200, "peer accepted message"),
            Ok((202, _)) => http::Response::new(202, "peer queued message"),
            Ok((status, reason)) if http::missing_route(status, &reason).is_some() => {
                http::Response::new(404, http::response_reason(&reason))
            }
            Ok((status, reason)) => {
                eprintln!(
                    "agent-inbox: peer {name} rejected message: HTTP {status}: {}",
                    http::response_reason(&reason)
                );
                http::Response::new(502, "peer rejected message; message saved")
            }
            Err(_) => {
                http::Response::new(503, "peer offline; message saved; do not blindly resend")
            }
        }
    }
}

pub fn run(mut args: impl Iterator<Item = String>) -> Result<()> {
    let mut listen = None;
    let mut config = None;
    let mut log = None;
    while let Some(flag) = args.next() {
        if flag == "--help" || flag == "-h" {
            println!("agent-inbox hub --listen <loopback-addr> --config <toml> [--log <path>]");
            return Ok(());
        }
        if !matches!(flag.as_str(), "--listen" | "--config" | "--log") {
            return Err(format!("unknown hub option {flag}").into());
        }
        let value = args
            .next()
            .ok_or_else(|| format!("missing value for {flag}"))?;
        match flag.as_str() {
            "--listen" => listen = Some(http::listen_address(&value)?),
            "--config" => config = Some(PathBuf::from(value)),
            "--log" => log = Some(PathBuf::from(value)),
            _ => unreachable!(),
        }
    }
    let listen = listen.ok_or("--listen is required")?;
    if !listen.ip().is_loopback() {
        return Err("hub must bind a loopback address".into());
    }
    let router = Arc::new(Router::load(&config.ok_or("--config is required")?, log)?);
    let listener = TcpListener::bind(listen)?;
    let address = listener.local_addr()?;
    let auth_router = Arc::clone(&router);
    let authenticate = Arc::new(move |header: &[u8]| auth_router.authenticate(header));
    let handler = Arc::new(
        move |path: &str, text: &str, sender: Option<&str>, origin: http::Origin| match sender {
            Some(sender) if origin == http::Origin::Host && qualified(sender).is_err() => {
                http::Response::new(400, "invalid host-qualified sender")
            }
            Some(sender) if path == "/directory/report" => {
                if origin != http::Origin::Host {
                    return http::Response::new(403, "only registered hosts may report agents");
                }
                let host = qualified(sender).unwrap().0;
                match router.directory.report(host, text) {
                    Ok(()) => http::Response::new(200, "directory report saved"),
                    Err(_) => http::Response::new(400, "invalid or out-of-prefix directory report"),
                }
            }
            Some(_) if path == "/directory/list" => match router.directory.page(text) {
                Ok(page) => http::Response::json(200, page),
                Err(_) => http::Response::new(400, "invalid directory query"),
            },
            Some(sender) if path.starts_with("/chats/") => {
                router.chat_routes.handle(path, sender, text)
            }
            Some(sender) => router.forward(path, text, sender),
            None => http::Response::new(401, "bad token"),
        },
    );
    eprintln!("agent-inbox: listening on http://{address}/");
    http::serve_authenticated(
        listener,
        Arc::new(AtomicBool::new(true)),
        authenticate,
        handler,
    )
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct Fixture {
        root: PathBuf,
        listeners: Vec<TcpListener>,
    }
    impl Fixture {
        fn new() -> Self {
            let root = PathBuf::from(format!(
                "/tmp/agent-inbox-hub-unit-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&root).unwrap();
            std::fs::write(root.join("token"), "unit-test-private-token").unwrap();
            std::fs::set_permissions(root.join("token"), std::fs::Permissions::from_mode(0o600))
                .unwrap();
            let listeners = vec![
                TcpListener::bind("127.0.0.1:0").unwrap(),
                TcpListener::bind("127.0.0.1:0").unwrap(),
            ];
            let config = format!(
                "journal='hub.md'\n[[senders]]\nname='sender'\ntoken_file='token'\n[[peers]]\nname='alpha'\ninbox='{}'\ntoken_file='token'\n[[peers]]\nname='beta'\ninbox='{}'\ntoken_file='token'\n",
                listeners[0].local_addr().unwrap(),
                listeners[1].local_addr().unwrap()
            );
            std::fs::write(root.join("hub.toml"), config).unwrap();
            Self { root, listeners }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn router_selects_the_configured_peer_and_rejects_an_unknown_name() {
        let fixture = Fixture::new();
        let router = Router::load(&fixture.root.join("hub.toml"), None).unwrap();
        assert_eq!(
            router.peers["alpha"].inbox,
            fixture.listeners[0].local_addr().unwrap().to_string()
        );
        assert_eq!(
            router.peers["beta"].inbox,
            fixture.listeners[1].local_addr().unwrap().to_string()
        );
        assert_eq!(
            router.forward("/to/unknown", "message", "sender").status,
            404
        );
        assert!(
            std::fs::read_to_string(fixture.root.join("hub.md"))
                .unwrap()
                .contains("To: unknown")
        );
    }

    #[test]
    fn logical_names_cannot_escape_a_route_or_add_headers() {
        for name in [
            "",
            ".",
            "..",
            "alpha/beta",
            "alpha?x=1",
            "name\r\nInjected: yes",
            "%2f",
            "会话",
        ] {
            assert!(validate_name(name).is_err());
        }
        for name in ["main-review", "netkit-codex", "peer_2", "peer.v1"] {
            assert!(validate_name(name).is_ok());
        }
    }
}
