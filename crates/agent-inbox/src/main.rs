use std::path::PathBuf;
use std::time::Duration;

use agent_inbox::{
    Result, claude, codex::Client, codex_inbox, http, hub, journal, proxy, token::Token,
    validate_message,
};

const USAGE: &str = "agent-inbox claude --listen <addr> --token-file <path> [--log <path>]\n\
agent-inbox claude --relay <local-relay-addr> [--name <name>] [--description <text>]\n\
agent-inbox hub --listen <loopback-addr> --config <toml> [--log <path>]\n\
agent-inbox relay --listen <loopback-addr> --config <toml>\n\
agent-inbox gui [--hub <local-relay>] [--token-file <sender-token>] [--listen <loopback-addr>]\n\
agent-inbox daemon --config <toml> [--check]\n\
agent-inbox feishu-watch --config <toml> [--once] [--retry <delivery-id>]\n\
agent-inbox chat-route <register|remove|list> --hub <relay> --token-file <sender-token> [--chat <chat-id>]\n\
agent-inbox codex --thread <id> --listen <addr> --token-file <path> [--log <path>]\n\
agent-inbox codex-watch --config <toml>\n\
agent-inbox grok-watch --config <toml>\n\
agent-inbox grok-self --session <id> [--state-dir <watcher-state>]\n\
agent-inbox codex-self [--thread <id>] [--state-dir <watcher-state>]\n\
agent-inbox send <http-address> --token-file <path> [--from <name>] [--log <path>] <text>\n\
agent-inbox send --hub <addr> --to <logical-name> --token-file <my-hub-token> [--log <path>] <text>\n\
agent-inbox steer --thread <id> [options] <text>\n\
agent-inbox codex-queue <addr> --token-file <path> [--resend <id> | --drop <id> | --release-hold <lease>]\n\
agent-inbox grok-queue <addr> --token-file <path> [--resend <id> | --drop <id>]\n\
agent-inbox peer-queue <local-relay> --token-file <sender-token> [--resend <id> | --drop <id>]\n\
agent-inbox who --hub <local-relay-address> --token-file <local-sender-token> [--json]\n\
agent-inbox upgrade-codex --config <toml> [--disable-auto-update]\n\
Options:\n\
  --from <name>           Sender label (default: teammate)\n\
  --log <path>            Durable Markdown journal\n\
  --codex-bin <path>      Codex executable (default: codex)\n\
  --sock <path>           Custom app-server control socket\n\
  --timeout-secs <n>      Response timeout (default: 10)\n\
  --                     Treat remaining arguments as message text\n\
Tested with Codex 0.162.1. Messages are coordination, not user approvals.";

fn main() {
    if let Err(error) = run() {
        eprintln!("agent-inbox: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("--help" | "-h") | None => {
            println!("{USAGE}");
            #[cfg(feature = "dynamic-peers")]
            println!(
                "Experimental: agent-inbox mesh-identity <host-keygen|authority-keygen|manifest-sign|renew-single> ...\nagent-inbox mesh-status --hub <local-relay> --token-file <sender-token> [--peer <host>]"
            );
            return Ok(());
        }
        Some("--version") => {
            println!("agent-inbox {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Some("steer") => {}
        Some("claude") => return claude::run(args),
        Some("codex") => return codex_inbox::run(args),
        #[cfg(unix)]
        Some("codex-watch") => return agent_inbox::codex_watch::run(args),
        #[cfg(unix)]
        Some("grok-watch") => return agent_inbox::grok_watch::run(args),
        #[cfg(unix)]
        Some("grok-self") => return agent_inbox::grok_watch::own_status(args),
        #[cfg(unix)]
        Some("codex-worker") => return agent_inbox::codex_auto::run(args),
        #[cfg(unix)]
        Some("codex-self") => return agent_inbox::codex_watch::own_status(args),
        Some("send") => return send(args),
        Some("hub") => return hub::run(args),
        #[cfg(unix)]
        Some("relay") => return agent_inbox::relay::run(args),
        #[cfg(unix)]
        Some("gui") => return agent_inbox::gui::run(args),
        #[cfg(unix)]
        Some("daemon") => return agent_inbox::daemon::run(args),
        #[cfg(unix)]
        Some("feishu-watch") => return agent_inbox::feishu_watch::run(args),
        Some("codex-queue") => return queue_control(args, "codex-queue", true),
        Some("grok-queue") => return queue_control(args, "grok-queue", false),
        Some("peer-queue") => return peer_queue(args),
        #[cfg(feature = "dynamic-peers")]
        Some("mesh-identity") => return agent_inbox::mesh_identity_cli::run(args),
        #[cfg(feature = "dynamic-peers")]
        Some("mesh-status") => return mesh_status(args),
        Some("who") => return who(args),
        Some("chat-route") => return agent_inbox::chat_routes::run(args),
        Some("upgrade-codex") => return agent_inbox::upgrade::run(args),
        #[cfg(unix)]
        Some("tunnel-route") => return agent_inbox::tunnel::print_route(),
        #[cfg(unix)]
        Some("tunnel-supervise") => return agent_inbox::tunnel::supervise(),
        #[cfg(unix)]
        Some("tunnel-worker") => return agent_inbox::tunnel::worker(args),
        _ => return Err(format!("unknown command\n{USAGE}").into()),
    }
    let mut thread = None;
    let mut sender = "teammate".to_owned();
    let mut codex = PathBuf::from("codex");
    let mut socket = None;
    let mut log = None;
    let mut timeout = 10_u64;
    let mut text = Vec::new();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                println!("{USAGE}");
                return Ok(());
            }
            "--" => {
                text.extend(args);
                break;
            }
            "--thread" | "--from" | "--log" | "--codex-bin" | "--sock" | "--timeout-secs" => {
                let value = args
                    .next()
                    .ok_or_else(|| format!("missing value for {arg}"))?;
                match arg.as_str() {
                    "--thread" => thread = Some(value),
                    "--from" => sender = value,
                    "--log" => log = Some(PathBuf::from(value)),
                    "--codex-bin" => codex = PathBuf::from(value),
                    "--sock" => socket = Some(PathBuf::from(value)),
                    "--timeout-secs" => timeout = value.parse()?,
                    _ => unreachable!(),
                }
            }
            _ if arg.starts_with('-') => {
                return Err(
                    format!("unknown option {arg}; use -- before text starting with -").into(),
                );
            }
            _ => {
                text.push(arg);
                text.extend(args);
                break;
            }
        }
    }
    let thread = thread.ok_or("--thread is required")?;
    let text = text.join(" ");
    validate_message(&thread, &sender, &text)?;
    if !(1..=60).contains(&timeout) {
        return Err("--timeout-secs must be between 1 and 60".into());
    }
    let log = match log {
        Some(path) => path,
        None => journal::default_path()?,
    };
    journal::append(&log, &sender, &thread, &text)?;
    let outcome = (|| {
        let socket = proxy::checked_socket(&codex, socket.as_deref())?;
        let mut client = Client::connect(&codex, &socket, Duration::from_secs(timeout))?;
        client.deliver(&thread, &sender, &text)
    })();
    match outcome {
        Ok(delivery) => println!(
            "Accepted via {}: thread {}, turn {}. Logged to {}.",
            delivery.method,
            thread,
            delivery.turn,
            log.display()
        ),
        Err(error) => {
            return Err(format!(
                "{error}. Message saved to {}; no automatic resend after uncertain delivery",
                log.display()
            )
            .into());
        }
    }
    Ok(())
}

#[cfg(feature = "dynamic-peers")]
fn mesh_status(mut args: impl Iterator<Item = String>) -> Result<()> {
    let mut relay = None;
    let mut token_file = None;
    let mut peer = None;
    while let Some(flag) = args.next() {
        let value = args.next().ok_or("missing mesh-status option value")?;
        match flag.as_str() {
            "--hub" => relay = Some(value),
            "--token-file" => token_file = Some(PathBuf::from(value)),
            "--peer" => peer = Some(value),
            _ => return Err("mesh-status accepts only --hub, --token-file and --peer".into()),
        }
    }
    let relay = http::listen_address(&relay.ok_or("mesh-status requires --hub")?)?;
    if !relay.ip().is_loopback() {
        return Err("mesh-status requires a loopback relay".into());
    }
    let token = Token::read(&token_file.ok_or("mesh-status requires --token-file")?)?;
    let (path, body) = match peer {
        Some(host) => {
            hub::validate_component(&host)?;
            (
                "/mesh/peer-check",
                serde_json::json!({"host":host}).to_string(),
            )
        }
        None => ("/mesh/status", "{}".to_owned()),
    };
    let (status, body) = http::post_response(&relay.to_string(), path, &token, &body)?;
    if status != 200 {
        return Err(format!("mesh-status rejected (HTTP {status})").into());
    }
    let value: serde_json::Value = serde_json::from_str(&body)?;
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

fn send(mut args: impl Iterator<Item = String>) -> Result<()> {
    let mut address = None;
    let mut hub_address = None;
    let mut target = None;
    let mut token_file = std::env::var_os("AGENT_INBOX_TOKEN_FILE").map(PathBuf::from);
    let mut sender = "teammate".to_owned();
    let mut log = None;
    let mut text = Vec::new();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                println!("{USAGE}");
                return Ok(());
            }
            "--token-file" | "--from" | "--log" | "--hub" | "--to" => {
                let value = args
                    .next()
                    .ok_or_else(|| format!("missing value for {arg}"))?;
                match arg.as_str() {
                    "--token-file" => token_file = Some(PathBuf::from(value)),
                    "--from" => sender = value,
                    "--log" => log = Some(PathBuf::from(value)),
                    "--hub" => hub_address = Some(value),
                    "--to" => target = Some(value),
                    _ => unreachable!(),
                }
            }
            "--" => {
                text.extend(args);
                break;
            }
            _ if arg.starts_with('-') => {
                return Err("unknown send option; use -- before text starting with -".into());
            }
            _ => {
                if address.is_none() && hub_address.is_none() {
                    address = Some(arg);
                    continue;
                }
                text.push(arg);
                text.extend(args);
                break;
            }
        }
    }
    let text = text.join(" ");
    let (addr, destination, hub_mode) = match (address, hub_address, target) {
        (None, Some(hub), Some(name)) => {
            hub::validate_name(&name)?;
            (hub, name, true)
        }
        (Some(addr), None, None) => (addr.clone(), addr, false),
        (Some(_), Some(_), _) => return Err("choose a direct address or --hub, not both".into()),
        (_, Some(_), None) => return Err("--to is required with --hub".into()),
        (_, None, Some(_)) => return Err("--to requires --hub".into()),
        _ => return Err("send requires a direct address or --hub and --to".into()),
    };
    let sender = if hub_mode {
        "hub-token-holder".to_owned()
    } else {
        sender
    };
    validate_message(&destination, &sender, &text)?;
    let token =
        Token::read(&token_file.ok_or("--token-file or AGENT_INBOX_TOKEN_FILE is required")?)?;
    let log = match log {
        Some(path) => path,
        None => journal::default_path()?,
    };
    journal::append(&log, &sender, &destination, &text)?;
    let outcome = if hub_mode {
        http::post_diagnostic(
            &addr,
            &format!("/to/{destination}"),
            &token,
            None,
            &text,
            http::SEND_TIMEOUT,
        )
        .and_then(|(status, body)| {
            http::accepted(status).map_err(|error| {
                let hint = if status == 404 {
                    "; check --to against the exact name from who (host.agent for registered sessions)"
                } else {
                    ""
                };
                format!("{error}: {}{hint}", http::response_reason(&body)).into()
            })
        })
    } else {
        http::send(&addr, &token, &sender, &text)
    };
    let status = outcome.map_err(|error| format!("{error}. Message saved to {}", log.display()))?;
    let recipient = if hub_mode {
        format!("{addr} for {destination}")
    } else {
        addr
    };
    if status == 202 {
        println!(
            "Message accepted by {recipient} (HTTP 202); delivery or session receipt is unconfirmed. Logged to {}.",
            log.display()
        );
    } else {
        println!(
            "Message accepted by {recipient} (HTTP {status}). Logged to {}.",
            log.display()
        );
    }
    Ok(())
}

fn queue_control(
    mut args: impl Iterator<Item = String>,
    kind: &str,
    allow_hold: bool,
) -> Result<()> {
    let address = args
        .next()
        .ok_or_else(|| format!("{kind} requires an inbox address"))?;
    let mut token_file = None;
    let mut action = "status";
    let mut id = None;
    while let Some(arg) = args.next() {
        let value = args.next().ok_or("missing queue option value")?;
        match arg.as_str() {
            "--token-file" => token_file = Some(PathBuf::from(value)),
            "--resend" | "--drop" if id.is_none() => {
                action = match arg.as_str() {
                    "--resend" => "resend",
                    "--drop" => "drop",
                    _ => "release",
                };
                id = Some(value);
            }
            "--release-hold" if allow_hold && id.is_none() => {
                action = "release";
                id = Some(value);
            }
            _ => return Err(format!("unknown or conflicting {kind} option").into()),
        }
    }
    let token = Token::read(&token_file.ok_or_else(|| format!("{kind} requires --token-file"))?)?;
    let body = if action == "release" {
        let (status, body) = http::post_response(&address, "/control/status", &token, "{}")?;
        http::accepted(status)?;
        let status: serde_json::Value = serde_json::from_str(&body)?;
        serde_json::json!({"instance":status["instance"],"lease":id})
    } else {
        serde_json::json!({"id":id})
    };
    let (status, body) = http::post_response(
        &address,
        &format!("/control/{action}"),
        &token,
        &body.to_string(),
    )?;
    http::accepted(status)?;
    println!("{}", body.trim_end());
    Ok(())
}

fn peer_queue(mut args: impl Iterator<Item = String>) -> Result<()> {
    let address = args
        .next()
        .ok_or("peer-queue requires a local relay address")?;
    let mut token_file = None;
    let mut action = None;
    while let Some(arg) = args.next() {
        let value = args.next().ok_or("missing peer-queue option value")?;
        match arg.as_str() {
            "--token-file" => token_file = Some(PathBuf::from(value)),
            "--resend" | "--drop" if action.is_none() && agent_inbox::state::is_id(&value) => {
                action = Some((arg.trim_start_matches("--").to_owned(), value));
            }
            _ => return Err("unknown or conflicting peer-queue option".into()),
        }
    }
    let token = Token::read(&token_file.ok_or("peer-queue requires --token-file")?)?;
    let path = action.as_ref().map_or_else(
        || "/mesh/queue".to_owned(),
        |(action, id)| format!("/mesh/queue/{id}/{action}"),
    );
    let (status, body) = http::post_response(&address, &path, &token, "{}")?;
    http::accepted(status)?;
    if action.is_none() {
        let value: serde_json::Value = serde_json::from_str(&body)?;
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        println!("{}", body.trim_end());
    }
    Ok(())
}

fn who(mut args: impl Iterator<Item = String>) -> Result<()> {
    let mut address = std::env::var("AGENT_INBOX_RELAY").ok();
    let mut token_file = std::env::var_os("AGENT_INBOX_TOKEN_FILE").map(PathBuf::from);
    let mut json_output = false;
    let mut local = false;
    let mut control_socket = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--json" => json_output = true,
            "--local" => local = true,
            "--control-socket" => {
                control_socket = Some(PathBuf::from(args.next().ok_or("missing control socket")?));
                local = true;
            }
            "--hub" | "--token-file" => {
                let value = args.next().ok_or("missing who option value")?;
                if arg == "--hub" {
                    address = Some(value);
                } else {
                    token_file = Some(PathBuf::from(value));
                }
            }
            "--help" | "-h" => {
                println!(
                    "agent-inbox who --hub <local-relay-address> --token-file <local-sender-token> [--json]\nagent-inbox who --local [--hub <local-relay-address>] [--control-socket <path>] [--json]"
                );
                return Ok(());
            }
            _ => return Err("unknown who option".into()),
        }
    }
    let mut rows = Vec::new();
    if local {
        #[cfg(unix)]
        {
            let address = address.unwrap_or_else(|| "127.0.0.1:8831".into());
            let address = http::listen_address(&address)?;
            if !address.ip().is_loopback() {
                return Err("local directory requires a loopback relay".into());
            }
            let socket = control_socket
                .map(Ok)
                .unwrap_or_else(|| agent_inbox::local_control::default_path(address.port()))?;
            let page = agent_inbox::local_control::request_with_timeout(
                &socket,
                &serde_json::json!({"op":"directory"}),
                Duration::from_secs(5),
            )?;
            rows = page["agents"]
                .as_array()
                .ok_or("local directory unavailable; upgrade the relay")?
                .clone();
        }
        #[cfg(not(unix))]
        return Err("local directory requires a Unix local relay".into());
    } else {
        let address = address.ok_or("who requires --hub or AGENT_INBOX_RELAY")?;
        let token =
            Token::read(&token_file.ok_or("who requires --token-file or AGENT_INBOX_TOKEN_FILE")?)?;
        let mut cursor = None;
        loop {
            let (status, body) = http::post_response(
                &address,
                "/directory/list",
                &token,
                &serde_json::json!({"cursor":cursor}).to_string(),
            )?;
            http::accepted(status)?;
            let page: serde_json::Value = serde_json::from_str(&body)?;
            rows.extend(
                page["agents"]
                    .as_array()
                    .ok_or("invalid directory page")?
                    .iter()
                    .cloned(),
            );
            match page.get("nextCursor") {
                Some(serde_json::Value::Null) | None => break,
                Some(next) if next.as_u64().is_some_and(|next| Some(next) > cursor) => {
                    cursor = next.as_u64()
                }
                _ => return Err("invalid directory continuation".into()),
            }
        }
    }
    if json_output {
        println!("{}", serde_json::to_string_pretty(&rows)?);
    } else {
        println!("NAME\tSTATUS\tLAST SEEN\tDESCRIPTION");
        for row in rows {
            let status = if row["membership_expired"] == true {
                "unavailable(membership expired)"
            } else if row["host_online"] == false {
                "offline(host)"
            } else if row["inbox_reachable"] == false {
                "offline(inbox)"
            } else {
                row["status"].as_str().unwrap_or("unknown")
            };
            println!(
                "{}\t{}\t{}\t{}",
                row["name"].as_str().unwrap_or("?"),
                status,
                row["last_seen"].as_str().unwrap_or("never"),
                row["description"].as_str().unwrap_or("")
            );
        }
    }
    Ok(())
}
