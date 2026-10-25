use std::borrow::Cow;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use crate::{MAX_MESSAGE_BYTES, Result, token::Token};

const HEADER_LIMIT: usize = 16 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(5);
// Allow tunnel latency in addition to the hub's shorter forwarding deadline.
pub const SEND_TIMEOUT: Duration = Duration::from_secs(25);
const MAX_WORKERS: usize = 16;

pub fn listen_address(value: &str) -> Result<SocketAddr> {
    if let Ok(port) = value.parse::<u16>() {
        return Ok(SocketAddr::from(([127, 0, 0, 1], port)));
    }
    value
        .parse()
        .map_err(|_| "--listen must be a port or a numeric IP:port".into())
}

pub struct Response {
    pub status: u16,
    pub text: Cow<'static, str>,
}

impl Response {
    pub fn new(status: u16, text: &'static str) -> Self {
        Self {
            status,
            text: Cow::Borrowed(text),
        }
    }
    pub fn json(status: u16, value: serde_json::Value) -> Self {
        Self {
            status,
            text: Cow::Owned(value.to_string()),
        }
    }
}

type Handler = dyn Fn(&str, Option<&str>) -> Response + Send + Sync;
pub type Health = dyn Fn() -> bool + Send + Sync;
pub type RouteHandler = dyn Fn(&str, &str, Option<&str>, Origin) -> Response + Send + Sync;
pub type Authenticator = dyn Fn(&[u8]) -> Option<SenderIdentity> + Send + Sync;
#[cfg(feature = "dynamic-peers")]
pub type MeshAuthenticator =
    dyn Fn(&[u8], &str, &str, &[u8], &str, SocketAddr) -> Option<SenderIdentity> + Send + Sync;

pub enum SenderIdentity {
    Header,
    Authenticated(String),
    HostPrefix(String),
    Hub,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    Header,
    Sender,
    Host,
    Hub,
}

/// One HTTP request per connection, bounded headers/body, and no request logging.
pub fn serve(
    listener: TcpListener,
    token: Token,
    running: Arc<AtomicBool>,
    handler: Arc<Handler>,
) -> Result<()> {
    serve_with_health(listener, token, running, handler, Arc::new(|| true))
}

pub fn serve_with_health(
    listener: TcpListener,
    token: Token,
    running: Arc<AtomicBool>,
    handler: Arc<Handler>,
    health: Arc<Health>,
) -> Result<()> {
    let authenticate =
        Arc::new(move |header: &[u8]| token.authorizes(header).then_some(SenderIdentity::Header));
    let route = Arc::new(
        move |path: &str, text: &str, sender: Option<&str>, _: Origin| {
            if path != "/" {
                return Response::new(404, "unknown endpoint");
            }
            handler(text, sender)
        },
    );
    serve_authenticated_with_health(listener, running, authenticate, route, health)
}

pub fn serve_authenticated(
    listener: TcpListener,
    running: Arc<AtomicBool>,
    authenticate: Arc<Authenticator>,
    handler: Arc<RouteHandler>,
) -> Result<()> {
    serve_authenticated_with_health(listener, running, authenticate, handler, Arc::new(|| true))
}

fn serve_authenticated_with_health(
    listener: TcpListener,
    running: Arc<AtomicBool>,
    authenticate: Arc<Authenticator>,
    handler: Arc<RouteHandler>,
    health: Arc<Health>,
) -> Result<()> {
    listener.set_nonblocking(true)?;
    let workers = Arc::new(AtomicUsize::new(0));
    while running.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((mut stream, _)) => {
                // BSD/macOS: accepted sockets inherit O_NONBLOCK from the listener; requests that arrive a
                // moment after connect (SSH tunnels) would then read WouldBlock and get a false 408.
                if stream.set_nonblocking(false).is_err() {
                    continue;
                }
                if workers.load(Ordering::Acquire) >= MAX_WORKERS {
                    stream.set_write_timeout(Some(Duration::from_millis(100)))?;
                    let _ = respond(&mut stream, Response::new(503, "inbox busy; retry later"));
                    continue;
                }
                workers.fetch_add(1, Ordering::AcqRel);
                let workers = Arc::clone(&workers);
                let authenticate = Arc::clone(&authenticate);
                let handler = Arc::clone(&handler);
                let health = Arc::clone(&health);
                std::thread::spawn(move || {
                    let _guard = Worker(workers);
                    let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
                    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
                    let response = read_request(&mut stream, &authenticate, &handler, &health)
                        .unwrap_or_else(|response| response);
                    let _ = respond(&mut stream, response);
                    // Half-close, then boundedly drain unread bytes so a rejection survives TCP close.
                    let _ = stream.shutdown(std::net::Shutdown::Write);
                    let deadline = Instant::now() + Duration::from_millis(100);
                    let mut remaining = MAX_MESSAGE_BYTES + HEADER_LIMIT;
                    let mut buffer = [0; 4096];
                    while remaining > 0 {
                        let Some(timeout) = deadline
                            .checked_duration_since(Instant::now())
                            .filter(|duration| !duration.is_zero())
                        else {
                            break;
                        };
                        if stream.set_read_timeout(Some(timeout)).is_err() {
                            break;
                        }
                        match stream.read(&mut buffer[..remaining.min(4096)]) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => remaining -= n,
                        }
                    }
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

struct Worker(Arc<AtomicUsize>);
impl Drop for Worker {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

fn read_request(
    stream: &mut TcpStream,
    authenticate: &Arc<Authenticator>,
    handler: &Arc<RouteHandler>,
    health: &Arc<Health>,
) -> std::result::Result<Response, Response> {
    let deadline = Instant::now() + IO_TIMEOUT;
    let mut bytes = Vec::new();
    let header_end = loop {
        if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            if end + 4 > HEADER_LIMIT {
                return Err(Response::new(431, "headers too large"));
            }
            break end + 4;
        }
        if bytes.len() >= HEADER_LIMIT {
            return Err(Response::new(431, "headers too large"));
        }
        read_more(stream, &mut bytes, deadline, 4096)?;
    };
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut request = httparse::Request::new(&mut headers);
    if !matches!(
        request.parse(&bytes[..header_end]),
        Ok(httparse::Status::Complete(_))
    ) {
        return Err(Response::new(400, "invalid HTTP request"));
    }
    let path = request
        .path
        .filter(|path| path.starts_with('/'))
        .ok_or_else(|| Response::new(400, "invalid request target"))?
        .to_owned();
    match request.method {
        Some("GET") => {
            if path != "/" {
                return Err(Response::new(404, "unknown endpoint"));
            }
            if !health() {
                return Ok(Response::new(
                    503,
                    "agent-inbox session is not receiving; inspect inbox_profile",
                ));
            }
            return Ok(Response::new(
                200,
                "agent-inbox ok: POST text with Authorization: Bearer <token>",
            ));
        }
        Some("POST") => {}
        _ => return Err(Response::new(405, "use POST")),
    }
    let auth = unique_header(request.headers, "Authorization")?;
    let identity = auth
        .and_then(|header| authenticate(header))
        .ok_or_else(|| Response::new(401, "bad token"))?;
    if request
        .headers
        .iter()
        .any(|header| header.name.eq_ignore_ascii_case("Transfer-Encoding"))
    {
        return Err(Response::new(
            400,
            "use Content-Length; transfer encoding is unsupported",
        ));
    }
    let length = unique_header(request.headers, "Content-Length")?
        .and_then(|value| std::str::from_utf8(value).ok())
        .filter(|value| !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
        .and_then(|value| value.parse::<usize>().ok())
        .ok_or_else(|| Response::new(400, "body must be 1 to 65536 bytes"))?;
    if length == 0 || length > MAX_MESSAGE_BYTES {
        return Err(Response::new(400, "body must be 1 to 65536 bytes"));
    }
    let header_sender = || -> std::result::Result<Option<String>, Response> {
        unique_header(request.headers, "X-Agent-From")?
            .map(|value| std::str::from_utf8(value).map(str::to_owned))
            .transpose()
            .map_err(|_| Response::new(400, "invalid sender"))
    };
    let (sender, origin) = match identity {
        SenderIdentity::Authenticated(name) => (Some(name), Origin::Sender),
        SenderIdentity::Header => (header_sender()?, Origin::Header),
        SenderIdentity::Hub => (header_sender()?, Origin::Hub),
        SenderIdentity::HostPrefix(host) => {
            let sender =
                header_sender()?.ok_or_else(|| Response::new(400, "host sender is required"))?;
            if sender
                .strip_prefix(&format!("{host}."))
                .is_none_or(|agent| agent.is_empty())
            {
                return Err(Response::new(
                    403,
                    "sender is outside the authenticated host prefix",
                ));
            }
            (Some(sender), Origin::Host)
        }
    };
    if sender
        .as_ref()
        .is_some_and(|sender| sender.trim().is_empty() || sender.contains(['\r', '\n']))
    {
        return Err(Response::new(400, "invalid sender"));
    }
    while bytes.len() - header_end < length {
        let remaining = length - (bytes.len() - header_end);
        read_more(stream, &mut bytes, deadline, remaining.min(4096))?;
    }
    let body = String::from_utf8_lossy(&bytes[header_end..header_end + length]);
    let body = body.trim();
    if body.is_empty() || body.len() > MAX_MESSAGE_BYTES {
        return Err(Response::new(400, "empty or oversized message"));
    }
    Ok(handler(&path, body, sender.as_deref(), origin))
}

#[cfg(feature = "dynamic-peers")]
pub fn serve_mesh_authenticated(
    listener: TcpListener,
    running: Arc<AtomicBool>,
    authenticate: Arc<MeshAuthenticator>,
    handler: Arc<RouteHandler>,
) -> Result<()> {
    listener.set_nonblocking(true)?;
    let workers = Arc::new(AtomicUsize::new(0));
    while running.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((mut stream, peer)) => {
                if stream.set_nonblocking(false).is_err() {
                    continue;
                }
                if workers.load(Ordering::Acquire) >= MAX_WORKERS {
                    stream.set_write_timeout(Some(Duration::from_millis(100)))?;
                    let _ = respond(&mut stream, Response::new(503, "inbox busy; retry later"));
                    continue;
                }
                workers.fetch_add(1, Ordering::AcqRel);
                let workers = Arc::clone(&workers);
                let authenticate = Arc::clone(&authenticate);
                let handler = Arc::clone(&handler);
                std::thread::spawn(move || {
                    let _guard = Worker(workers);
                    let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
                    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
                    let response = read_mesh_request(&mut stream, peer, &authenticate, &handler)
                        .unwrap_or_else(|response| response);
                    let _ = respond(&mut stream, response);
                    let _ = stream.shutdown(std::net::Shutdown::Write);
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

#[cfg(feature = "dynamic-peers")]
fn read_mesh_request(
    stream: &mut TcpStream,
    peer: SocketAddr,
    authenticate: &Arc<MeshAuthenticator>,
    handler: &Arc<RouteHandler>,
) -> std::result::Result<Response, Response> {
    let deadline = Instant::now() + IO_TIMEOUT;
    let mut bytes = Vec::new();
    let header_end = loop {
        if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            if end + 4 > HEADER_LIMIT {
                return Err(Response::new(431, "headers too large"));
            }
            break end + 4;
        }
        if bytes.len() >= HEADER_LIMIT {
            return Err(Response::new(431, "headers too large"));
        }
        read_more(stream, &mut bytes, deadline, 4096)?;
    };
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut parsed = httparse::Request::new(&mut headers);
    if !matches!(
        parsed.parse(&bytes[..header_end]),
        Ok(httparse::Status::Complete(_))
    ) {
        return Err(Response::new(400, "invalid HTTP request"));
    }
    let path = parsed
        .path
        .filter(|path| path.starts_with('/'))
        .ok_or_else(|| Response::new(400, "invalid request target"))?
        .to_owned();
    if parsed.method == Some("GET") && path == "/" {
        return Ok(Response::new(
            200,
            "agent-inbox ok: authenticated mesh POST",
        ));
    }
    if parsed.method == Some("GET") && path == "/mesh/membership" {
        return Ok(handler(&path, "{}", None, Origin::Header));
    }
    if parsed.method != Some("POST") {
        return Err(Response::new(405, "use POST"));
    }
    if parsed
        .headers
        .iter()
        .any(|header| header.name.eq_ignore_ascii_case("Transfer-Encoding"))
    {
        return Err(Response::new(
            400,
            "use Content-Length; transfer encoding is unsupported",
        ));
    }
    let length = unique_header(parsed.headers, "Content-Length")?
        .and_then(|value| std::str::from_utf8(value).ok())
        .filter(|value| !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
        .and_then(|value| value.parse::<usize>().ok())
        .ok_or_else(|| Response::new(400, "body must be 1 to 65536 bytes"))?;
    if length == 0 || length > MAX_MESSAGE_BYTES {
        return Err(Response::new(400, "body must be 1 to 65536 bytes"));
    }
    let authorization = unique_header(parsed.headers, "Authorization")?
        .ok_or_else(|| Response::new(401, "mesh MAC required"))?
        .to_vec();
    let from = unique_header(parsed.headers, "X-Agent-From")?
        .and_then(|value| std::str::from_utf8(value).ok())
        .filter(|value| {
            value.is_ascii()
                && !value.is_empty()
                && !value.bytes().any(|byte| byte.is_ascii_control())
        })
        .ok_or_else(|| Response::new(400, "mesh sender required"))?
        .to_owned();
    while bytes.len() - header_end < length {
        let remaining = length - (bytes.len() - header_end);
        read_more(stream, &mut bytes, deadline, remaining.min(4096))?;
    }
    let body = &bytes[header_end..header_end + length];
    let Some(SenderIdentity::HostPrefix(host)) =
        authenticate(&authorization, "POST", &path, body, &from, peer)
    else {
        return Err(Response::new(401, "mesh MAC or membership rejected"));
    };
    if from
        .strip_prefix(&format!("{host}."))
        .is_none_or(str::is_empty)
    {
        return Err(Response::new(
            403,
            "sender is outside the authenticated host prefix",
        ));
    }
    let text = String::from_utf8_lossy(body);
    let text = text.trim();
    if text.is_empty() || text.len() > MAX_MESSAGE_BYTES {
        return Err(Response::new(400, "empty or oversized message"));
    }
    Ok(handler(&path, text, Some(&from), Origin::Host))
}

fn unique_header<'a>(
    headers: &[httparse::Header<'a>],
    name: &str,
) -> std::result::Result<Option<&'a [u8]>, Response> {
    let mut found = None;
    for header in headers
        .iter()
        .filter(|header| header.name.eq_ignore_ascii_case(name))
    {
        if found.is_some() {
            return Err(Response::new(400, "duplicate header"));
        }
        found = Some(header.value);
    }
    Ok(found)
}

fn read_more(
    stream: &mut TcpStream,
    bytes: &mut Vec<u8>,
    deadline: Instant,
    limit: usize,
) -> std::result::Result<(), Response> {
    let timeout = deadline
        .checked_duration_since(Instant::now())
        .filter(|duration| !duration.is_zero())
        .ok_or_else(|| Response::new(408, "request timed out"))?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|_| Response::new(400, "connection failed"))?;
    let mut buffer = vec![0; limit];
    let n = stream
        .read(&mut buffer)
        .map_err(|error| match error.kind() {
            std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => {
                Response::new(408, "request timed out")
            }
            _ => Response::new(400, "incomplete request"),
        })?;
    if n == 0 {
        return Err(Response::new(400, "incomplete request"));
    }
    bytes.extend_from_slice(&buffer[..n]);
    Ok(())
}

fn respond(stream: &mut TcpStream, response: Response) -> std::io::Result<()> {
    let reason = match response.status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        409 => "Conflict",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Error",
    };
    let body = format!("{}\n", response.text);
    write!(
        stream,
        "HTTP/1.1 {} {}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        response.status,
        reason,
        body.len(),
        body
    )?;
    stream.flush()
}

pub fn send(addr: &str, token: &Token, from: &str, text: &str) -> Result<u16> {
    let status = post(addr, "/", token, Some(from), text, SEND_TIMEOUT)?;
    accepted(status)
}

pub fn accepted(status: u16) -> Result<u16> {
    if !(200..300).contains(&status) {
        return Err(
            format!("inbox rejected message (HTTP {status}); message was not confirmed").into(),
        );
    }
    Ok(status)
}

pub fn post(
    addr: &str,
    path: &str,
    token: &Token,
    from: Option<&str>,
    text: &str,
    timeout: Duration,
) -> Result<u16> {
    Ok(post_inner(addr, path, token, from, text, timeout, Body::Skip)?.0)
}

pub fn post_response(addr: &str, path: &str, token: &Token, text: &str) -> Result<(u16, String)> {
    post_inner(addr, path, token, None, text, SEND_TIMEOUT, Body::Read)
}

/// Only known static diagnostics may enter logs. Arbitrary response bodies can
/// echo message contents or credentials and must remain private.
pub fn response_reason(body: &str) -> &'static str {
    match body.trim() {
        "invalid or out-of-prefix directory report" => "invalid or out-of-prefix directory report",
        "sender is outside the authenticated host prefix" => {
            "sender is outside the authenticated host prefix"
        }
        "invalid host-qualified sender" => "invalid host-qualified sender",
        "only registered hosts may report agents" => "only registered hosts may report agents",
        "bad token" => "bad token",
        "unknown endpoint" => "unknown endpoint",
        "unknown relay endpoint" => "unknown relay endpoint",
        "unknown local agent; message saved" => "unknown local agent; message saved",
        "unknown host; message saved" => "unknown host; message saved",
        "unknown peer; message saved" => "unknown peer; message saved",
        "sender required" => "sender required",
        "invalid logical name" => "invalid logical name",
        "inbound hub requests must target this host" => {
            "inbound hub requests must target this host"
        }
        "untrusted relay source" => "untrusted relay source",
        "destination rejected message; relay saved it" => {
            "destination rejected message; relay saved it"
        }
        "destination unavailable or unconfirmed; relay saved it; do not blindly resend" => {
            "destination unavailable or unconfirmed; relay saved it; do not blindly resend"
        }
        "relay journal failed; message not forwarded" => {
            "relay journal failed; message not forwarded"
        }
        "request timed out" => "request timed out",
        "inbox busy; retry later" => "inbox busy; retry later",
        _ => "unrecognized response (body omitted)",
    }
}

/// Preserve a routing failure across relays without exposing arbitrary bodies.
pub fn missing_route(status: u16, body: &str) -> Option<&'static str> {
    if status == 404
        && matches!(
            body.trim(),
            "unknown local agent; message saved"
                | "unknown host; message saved"
                | "unknown peer; message saved"
        )
    {
        Some(response_reason(body))
    } else {
        None
    }
}

pub fn post_response_from(
    addr: &str,
    path: &str,
    token: &Token,
    from: Option<&str>,
    text: &str,
    timeout: Duration,
) -> Result<(u16, String)> {
    post_inner(addr, path, token, from, text, timeout, Body::Read)
}

pub fn post_diagnostic(
    addr: &str,
    path: &str,
    token: &Token,
    from: Option<&str>,
    text: &str,
    timeout: Duration,
) -> Result<(u16, String)> {
    post_inner(addr, path, token, from, text, timeout, Body::ErrorsOnly)
}

enum Body {
    Skip,
    Read,
    ErrorsOnly,
}

fn post_inner(
    addr: &str,
    path: &str,
    token: &Token,
    from: Option<&str>,
    text: &str,
    timeout: Duration,
    read_body: Body,
) -> Result<(u16, String)> {
    let mut authorization = b"Bearer ".to_vec();
    authorization.extend_from_slice(token.bytes());
    post_inner_auth(addr, path, &authorization, from, text, timeout, read_body)
}

#[cfg(feature = "dynamic-peers")]
pub fn post_mesh_diagnostic(
    addr: &str,
    path: &str,
    authorization: &str,
    from: &str,
    text: &str,
    timeout: Duration,
) -> Result<(u16, String)> {
    if !authorization.is_ascii() || authorization.bytes().any(|byte| byte.is_ascii_control()) {
        return Err("invalid mesh authorization header".into());
    }
    post_inner_auth(
        addr,
        path,
        authorization.as_bytes(),
        Some(from),
        text,
        timeout,
        Body::ErrorsOnly,
    )
}

#[cfg(feature = "dynamic-peers")]
pub fn post_mesh_response(
    addr: &str,
    path: &str,
    authorization: &str,
    from: &str,
    text: &str,
    timeout: Duration,
) -> Result<(u16, String)> {
    if !authorization.is_ascii() || authorization.bytes().any(|byte| byte.is_ascii_control()) {
        return Err("invalid mesh authorization header".into());
    }
    post_inner_auth(
        addr,
        path,
        authorization.as_bytes(),
        Some(from),
        text,
        timeout,
        Body::Read,
    )
}

#[cfg(feature = "dynamic-peers")]
pub fn get_mesh_response(addr: &str, path: &str, timeout: Duration) -> Result<(u16, String)> {
    let address = addr.strip_prefix("http://").unwrap_or(addr);
    if address.contains("://") || address.contains(['\r', '\n', ' ', '@', '#', '?', '/']) {
        return Err("mesh address must be a host:port".into());
    }
    if !path.starts_with('/')
        || !path.bytes().all(|byte| byte.is_ascii_graphic())
        || path.contains(['?', '#'])
    {
        return Err("invalid mesh request path".into());
    }
    let deadline = Instant::now() + timeout;
    let address: SocketAddr = address.parse()?;
    let mut stream = TcpStream::connect_timeout(&address, timeout)?;
    stream.set_write_timeout(Some(timeout))?;
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n"
    )?;
    stream.flush()?;
    let mut response = Vec::new();
    let header_end = loop {
        if let Some(end) = response.windows(4).position(|part| part == b"\r\n\r\n") {
            if end + 4 > HEADER_LIMIT {
                return Err("mesh response headers exceed 16 KiB".into());
            }
            break end + 4;
        }
        if response.len() >= HEADER_LIMIT {
            return Err("mesh response headers exceed 16 KiB".into());
        }
        read_more(&mut stream, &mut response, deadline, 4096)
            .map_err(|_| "mesh response was incomplete or timed out")?;
    };
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut parsed = httparse::Response::new(&mut headers);
    if !matches!(
        parsed.parse(&response[..header_end]),
        Ok(httparse::Status::Complete(_))
    ) {
        return Err("invalid mesh HTTP response".into());
    }
    let status = parsed.code.ok_or("mesh response has no status")?;
    let length = unique_header(parsed.headers, "Content-Length")
        .map_err(|_| "duplicate mesh response length")?
        .and_then(|value| std::str::from_utf8(value).ok())
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|length| *length <= MAX_MESSAGE_BYTES)
        .ok_or("mesh response needs bounded Content-Length")?;
    while response.len() - header_end < length {
        let remaining = length - (response.len() - header_end);
        read_more(&mut stream, &mut response, deadline, remaining.min(4096))
            .map_err(|_| "mesh response body incomplete or timed out")?;
    }
    Ok((
        status,
        String::from_utf8(response[header_end..header_end + length].to_vec())?,
    ))
}

fn post_inner_auth(
    addr: &str,
    path: &str,
    authorization: &[u8],
    from: Option<&str>,
    text: &str,
    timeout: Duration,
    read_body: Body,
) -> Result<(u16, String)> {
    let address = addr.strip_prefix("http://").unwrap_or(addr);
    if address.contains("://") || address.contains(['\r', '\n', ' ', '@', '#', '?']) {
        return Err("address must be an HTTP host:port, optionally followed by /".into());
    }
    let authority = match address.split_once('/') {
        Some((authority, "")) => authority,
        Some(_) => return Err("the inbox endpoint is /".into()),
        None => address,
    };
    if !path.starts_with('/')
        || !path.bytes().all(|byte| byte.is_ascii_graphic())
        || path.contains(['?', '#'])
    {
        return Err("invalid HTTP request path".into());
    }
    if from.is_some_and(|from| !from.is_ascii() || from.bytes().any(|byte| byte.is_ascii_control()))
    {
        return Err("HTTP sender labels must be printable ASCII".into());
    }
    let deadline = Instant::now() + timeout;
    let addresses: Vec<SocketAddr> = authority.to_socket_addrs()?.collect();
    let mut stream = addresses
        .iter()
        .find_map(|address| {
            deadline
                .checked_duration_since(Instant::now())
                .filter(|timeout| !timeout.is_zero())
                .and_then(|timeout| TcpStream::connect_timeout(address, timeout).ok())
        })
        .ok_or("cannot connect to inbox; message was not confirmed")?;
    let mut request =
        format!("POST {path} HTTP/1.1\r\nHost: {authority}\r\nAuthorization: ").into_bytes();
    request.extend_from_slice(authorization);
    request.extend_from_slice(b"\r\n");
    if let Some(from) = from {
        request.extend_from_slice(format!("X-Agent-From: {from}\r\n").as_bytes());
    }
    request.extend_from_slice(format!("Content-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}", text.len()).as_bytes());
    let mut remaining = request.as_slice();
    while !remaining.is_empty() {
        let timeout = deadline
            .checked_duration_since(Instant::now())
            .filter(|timeout| !timeout.is_zero())
            .ok_or("inbox request timed out")?;
        stream.set_write_timeout(Some(timeout))?;
        let n = stream.write(remaining)?;
        if n == 0 {
            return Err("inbox connection closed during write".into());
        }
        remaining = &remaining[n..];
    }
    stream.flush()?;
    let mut response = Vec::new();
    let header_end = loop {
        if let Some(end) = response.windows(4).position(|part| part == b"\r\n\r\n") {
            if end + 4 > HEADER_LIMIT {
                return Err("inbox response headers exceed 16 KiB".into());
            }
            break end + 4;
        }
        if response.len() >= HEADER_LIMIT {
            return Err("inbox response headers exceed 16 KiB".into());
        }
        read_more(&mut stream, &mut response, deadline, 4096)
            .map_err(|_| "inbox response was incomplete or timed out")?;
    };
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut parsed = httparse::Response::new(&mut headers);
    if !matches!(
        parsed.parse(&response[..header_end]),
        Ok(httparse::Status::Complete(_))
    ) {
        return Err("inbox returned an invalid HTTP response".into());
    }
    let status = parsed.code.ok_or("inbox response has no status")?;
    if matches!(read_body, Body::Skip)
        || matches!(read_body, Body::ErrorsOnly) && (200..300).contains(&status)
    {
        return Ok((status, String::new()));
    }
    let length = unique_header(parsed.headers, "Content-Length")
        .map_err(|_| "duplicate response length")?
        .and_then(|value| std::str::from_utf8(value).ok())
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|length| *length <= MAX_MESSAGE_BYTES)
        .ok_or("response needs a bounded Content-Length")?;
    while response.len() - header_end < length {
        let remaining = length - (response.len() - header_end);
        read_more(&mut stream, &mut response, deadline, remaining.min(4096))
            .map_err(|_| "inbox response body incomplete or timed out")?;
    }
    Ok((
        status,
        String::from_utf8(response[header_end..header_end + length].to_vec())?,
    ))
}

#[cfg(test)]
mod routing_diagnostics_tests {
    use super::{missing_route, response_reason};

    #[test]
    fn only_static_missing_route_errors_are_forwarded_or_shown() {
        for reason in [
            "unknown peer; message saved",
            "unknown host; message saved",
            "unknown local agent; message saved",
        ] {
            assert_eq!(missing_route(404, reason), Some(reason));
            assert_eq!(missing_route(401, reason), None);
            assert_eq!(missing_route(502, reason), None);
        }
        let private = "unknown peer; message saved\nBearer private-credential";
        assert_eq!(missing_route(404, private), None);
        assert_eq!(
            response_reason(private),
            "unrecognized response (body omitted)"
        );
        assert_eq!(missing_route(404, "unknown endpoint"), None);
    }
}
