//! On-demand loopback UI. Relay credentials stay in this process, not in browser storage.

use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use subtle::ConstantTimeEq;

use crate::{MAX_MESSAGE_BYTES, Result, http, hub, state, token::Token, validate_message};

const PAGE: &str = include_str!("gui_page.html");
const MAX_HEADERS: usize = 16 * 1024;
const MAX_GUI_BODY: usize = MAX_MESSAGE_BYTES + 4096;
const MAX_WORKERS: usize = 16;

struct App {
    address: SocketAddr,
    relay: String,
    sender: Token,
    sender_name: String,
    bootstrap: Mutex<Option<String>>,
    cookie: String,
    csrf: String,
}

struct Request {
    method: String,
    path: String,
    host: String,
    origin: Option<String>,
    cookie: Option<String>,
    bootstrap: Option<String>,
    csrf: Option<String>,
    content_type: Option<String>,
    body: Vec<u8>,
}

struct Response {
    code: u16,
    content_type: &'static str,
    body: Vec<u8>,
    cookie: Option<String>,
}

impl Response {
    fn json(code: u16, body: Value) -> Self {
        Self {
            code,
            content_type: "application/json; charset=utf-8",
            body: body.to_string().into_bytes(),
            cookie: None,
        }
    }

    fn error(code: u16, message: &'static str) -> Self {
        Self::json(code, json!({"error":message}))
    }
}

fn same_secret(left: &str, right: &str) -> bool {
    bool::from(left.as_bytes().ct_eq(right.as_bytes()))
}

fn header(headers: &[httparse::Header<'_>], name: &str) -> Option<String> {
    let matches: Vec<_> = headers
        .iter()
        .filter(|header| header.name.eq_ignore_ascii_case(name))
        .collect();
    if matches.len() != 1 {
        return None;
    }
    std::str::from_utf8(matches[0].value)
        .ok()
        .map(str::to_owned)
}

fn read_request(stream: &mut TcpStream) -> Result<Request> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut buffer = Vec::new();
    let headers_end = loop {
        if let Some(index) = buffer.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
            break index + 4;
        }
        if buffer.len() >= MAX_HEADERS {
            return Err("GUI request headers exceed 16 KiB".into());
        }
        let mut bytes = [0; 4096];
        let count = stream.read(&mut bytes)?;
        if count == 0 {
            return Err("GUI request closed before headers".into());
        }
        buffer.extend_from_slice(&bytes[..count]);
        if buffer.len() > MAX_HEADERS + MAX_GUI_BODY {
            return Err("GUI request exceeds limit".into());
        }
    };
    if headers_end > MAX_HEADERS {
        return Err("GUI request headers exceed 16 KiB".into());
    }
    let mut headers = [httparse::EMPTY_HEADER; 48];
    let mut parsed = httparse::Request::new(&mut headers);
    if !parsed.parse(&buffer[..headers_end])?.is_complete() {
        return Err("incomplete GUI request".into());
    }
    let method = parsed.method.ok_or("missing GUI method")?.to_owned();
    let path = parsed.path.ok_or("missing GUI path")?.to_owned();
    let host = header(parsed.headers, "Host").ok_or("missing or repeated GUI Host")?;
    let origin = header(parsed.headers, "Origin");
    let cookie = header(parsed.headers, "Cookie");
    let bootstrap = header(parsed.headers, "X-Gui-Bootstrap");
    let csrf = header(parsed.headers, "X-Csrf");
    let content_type = header(parsed.headers, "Content-Type");
    if parsed
        .headers
        .iter()
        .any(|header| header.name.eq_ignore_ascii_case("Transfer-Encoding"))
    {
        return Err("GUI transfer encoding is unsupported".into());
    }
    let length = match header(parsed.headers, "Content-Length") {
        Some(value) => value.parse::<usize>()?,
        None if method == "GET" => 0,
        None => return Err("GUI POST requires Content-Length".into()),
    };
    if length > MAX_GUI_BODY {
        return Err("GUI request body exceeds limit".into());
    }
    while buffer.len() - headers_end < length {
        let mut bytes = [0; 4096];
        let count = stream.read(&mut bytes)?;
        if count == 0 {
            return Err("GUI request body was truncated".into());
        }
        buffer.extend_from_slice(&bytes[..count]);
        if buffer.len() > MAX_HEADERS + MAX_GUI_BODY {
            return Err("GUI request exceeds limit".into());
        }
    }
    Ok(Request {
        method,
        path,
        host,
        origin,
        cookie,
        bootstrap,
        csrf,
        content_type,
        body: buffer[headers_end..headers_end + length].to_vec(),
    })
}

fn authenticated(request: &Request, app: &App) -> bool {
    request
        .cookie
        .as_deref()
        .unwrap_or_default()
        .split(';')
        .filter_map(|part| part.trim().split_once('='))
        .any(|(name, value)| name == "inbox_gui" && same_secret(value, &app.cookie))
}

fn agents(app: &App) -> Result<Value> {
    let mut rows = Vec::new();
    let mut cursor = None;
    loop {
        let (code, body) = http::post_response(
            &app.relay,
            "/directory/list",
            &app.sender,
            &json!({"cursor":cursor}).to_string(),
        )?;
        http::accepted(code)?;
        let page: Value = serde_json::from_str(&body)?;
        rows.extend(
            page["agents"]
                .as_array()
                .ok_or("invalid directory page")?
                .iter()
                .cloned(),
        );
        match page.get("nextCursor") {
            Some(Value::Null) | None => break,
            Some(next) if next.as_u64().is_some_and(|next| Some(next) > cursor) => {
                cursor = next.as_u64();
            }
            _ => return Err("invalid directory continuation".into()),
        }
    }
    Ok(Value::Array(rows))
}

fn handle(app: &App, request: Request) -> Response {
    if request.host != app.address.to_string() {
        return Response::error(403, "invalid local Host");
    }
    if request.method == "POST"
        && request.origin.as_deref() != Some(&format!("http://{}", app.address))
    {
        return Response::error(403, "invalid request origin");
    }
    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/") => Response {
            code: 200,
            content_type: "text/html; charset=utf-8",
            body: PAGE.as_bytes().to_vec(),
            cookie: None,
        },
        ("POST", "/api/bootstrap") => {
            let valid = app.bootstrap.lock().ok().is_some_and(|mut secret| {
                if request
                    .bootstrap
                    .as_deref()
                    .zip(secret.as_deref())
                    .is_some_and(|(supplied, expected)| same_secret(supplied, expected))
                {
                    secret.take();
                    true
                } else {
                    false
                }
            });
            if !valid {
                return Response::error(401, "open the GUI through agent-inbox gui");
            }
            let mut response =
                Response::json(200, json!({"csrf":app.csrf,"sender":app.sender_name}));
            response.cookie = Some(format!(
                "inbox_gui={}; HttpOnly; SameSite=Strict; Path=/",
                app.cookie
            ));
            response
        }
        ("GET", "/api/session") => {
            if !authenticated(&request, app) {
                return Response::error(401, "GUI session is not open");
            }
            Response::json(200, json!({"csrf":app.csrf,"sender":app.sender_name}))
        }
        ("GET", "/api/agents") => {
            if !authenticated(&request, app) {
                return Response::error(401, "GUI session is not open");
            }
            match agents(app) {
                Ok(agents) => Response::json(200, json!({"agents":agents})),
                Err(_) => Response::error(503, "agent directory is unavailable; retry refresh"),
            }
        }
        ("POST", "/api/send") => {
            if !authenticated(&request, app)
                || !request
                    .csrf
                    .as_deref()
                    .is_some_and(|value| same_secret(value, &app.csrf))
            {
                return Response::error(403, "GUI session or action token is invalid");
            }
            if request.content_type.as_deref() != Some("application/json") {
                return Response::error(415, "send requires JSON");
            }
            let Ok(body) = serde_json::from_slice::<Value>(&request.body) else {
                return Response::error(400, "invalid send request");
            };
            let (Some(to), Some(message)) = (body["to"].as_str(), body["message"].as_str()) else {
                return Response::error(400, "recipient and message are required");
            };
            if hub::qualified(to).is_err() || validate_message(to, "GUI", message).is_err() {
                return Response::error(400, "enter a valid agent name and message under 64 KiB");
            }
            match http::post_diagnostic(
                &app.relay,
                &format!("/to/{to}"),
                &app.sender,
                None,
                message,
                http::SEND_TIMEOUT,
            ) {
                Ok((200, _)) => Response::json(200, json!({"state":"accepted"})),
                Ok((202, _)) => Response::json(202, json!({"state":"queued"})),
                Ok((code, body)) => Response::json(
                    502,
                    json!({"state":"unconfirmed","code":code,"reason":http::response_reason(&body)}),
                ),
                Err(_) => Response::json(
                    503,
                    json!({"state":"unconfirmed","reason":"delivery was not confirmed; inspect the receiver before retrying"}),
                ),
            }
        }
        _ => Response::error(404, "unknown GUI endpoint"),
    }
}

fn write_response(stream: &mut TcpStream, response: Response) -> Result<()> {
    let reason = match response.code {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        415 => "Unsupported Media Type",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Error",
    };
    let mut headers = format!(
        "HTTP/1.1 {} {reason}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nX-Frame-Options: DENY\r\nContent-Security-Policy: default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline'; connect-src 'self'; base-uri 'none'; frame-ancestors 'none'\r\nConnection: close\r\n",
        response.code,
        response.content_type,
        response.body.len()
    );
    if let Some(cookie) = response.cookie {
        headers.push_str(&format!("Set-Cookie: {cookie}\r\n"));
    }
    headers.push_str("\r\n");
    stream.write_all(headers.as_bytes())?;
    stream.write_all(&response.body)?;
    Ok(())
}

fn serve_connection(app: Arc<App>, mut stream: TcpStream) {
    let response = match read_request(&mut stream) {
        Ok(request) => handle(&app, request),
        Err(_) => Response::error(400, "invalid GUI request"),
    };
    let _ = write_response(&mut stream, response);
}

fn open_browser(url: &str) -> Result<()> {
    let program = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let status = Command::new(program)
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if !status.success() {
        return Err("could not open the local GUI browser".into());
    }
    Ok(())
}

pub fn run(mut args: impl Iterator<Item = String>) -> Result<()> {
    let mut relay = std::env::var("AGENT_INBOX_RELAY").ok();
    let mut token_file = std::env::var_os("AGENT_INBOX_TOKEN_FILE").map(PathBuf::from);
    let mut listen = SocketAddr::from(([127, 0, 0, 1], 0));
    let mut no_open = false;
    let mut bootstrap_file = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--hub" | "--token-file" | "--listen" | "--bootstrap-file" => {
                let value = args.next().ok_or("missing GUI option value")?;
                match arg.as_str() {
                    "--hub" => relay = Some(value),
                    "--token-file" => token_file = Some(PathBuf::from(value)),
                    "--listen" => listen = http::listen_address(&value)?,
                    _ => bootstrap_file = Some(PathBuf::from(value)),
                }
            }
            "--no-open" => no_open = true,
            "--help" | "-h" => {
                println!(
                    "agent-inbox gui --hub <local-relay> --token-file <sender-token> [--listen 127.0.0.1:0] [--no-open --bootstrap-file <private-path>]"
                );
                return Ok(());
            }
            _ => return Err("unknown GUI option".into()),
        }
    }
    if !listen.ip().is_loopback() {
        return Err("GUI must bind a loopback address".into());
    }
    if no_open && bootstrap_file.is_none() {
        return Err("--no-open requires --bootstrap-file".into());
    }
    let relay = hub::loopback_endpoint(&relay.ok_or("GUI requires --hub or AGENT_INBOX_RELAY")?)?;
    let sender = Token::read(&token_file.ok_or("GUI requires a sender token file")?)?;
    let sender_name = match http::post_response(&relay, "/whoami", &sender, "{}")? {
        (200, identity) => {
            let identity: Value = serde_json::from_str(&identity)?;
            identity["name"]
                .as_str()
                .filter(|name| hub::qualified(name).is_ok())
                .ok_or("GUI relay returned an invalid sender identity")?
                .to_owned()
        }
        (404, _) => "旧版 relay 未提供名称".to_owned(),
        _ => return Err("GUI sender token was not accepted by the local relay".into()),
    };
    let listener = TcpListener::bind(listen)?;
    let address = listener.local_addr()?;
    let bootstrap = state::id()?;
    let app = Arc::new(App {
        address,
        relay,
        sender,
        sender_name,
        bootstrap: Mutex::new(Some(bootstrap.clone())),
        cookie: state::id()?,
        csrf: state::id()?,
    });
    if let Some(path) = bootstrap_file {
        state::parent(&path)?;
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
        let mut file = options.open(path)?;
        file.write_all(bootstrap.as_bytes())?;
    }
    println!("agent-inbox GUI ready at http://{address}/");
    if !no_open {
        open_browser(&format!("http://{address}/#{bootstrap}"))?;
    }
    let workers = Arc::new(AtomicUsize::new(0));
    for connection in listener.incoming() {
        let stream = connection?;
        if workers.load(Ordering::Acquire) >= MAX_WORKERS {
            continue;
        }
        workers.fetch_add(1, Ordering::AcqRel);
        let current = Arc::clone(&workers);
        let app = Arc::clone(&app);
        std::thread::spawn(move || {
            serve_connection(app, stream);
            current.fetch_sub(1, Ordering::AcqRel);
        });
    }
    Ok(())
}
