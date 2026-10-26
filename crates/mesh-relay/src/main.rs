//! mesh-relay: rendezvous, STUN, encrypted packet forwarding, and node liveness events.
//!
//! One thread, one UDP socket, blocking I/O. Nodes register with HELLO (sealed with the
//! node/relay pair key, so the relay knows who it is); the relay remembers each node's public
//! address and forwards FWD payloads between registered nodes. Payloads are WireGuard packets
//! or node-to-node sealed messages: the relay can read neither.

use meshproto::crypto::PairKey;
use meshproto::release::{self, Release};
use meshproto::roster::Roster;
use meshproto::{wire, x25519};
use std::collections::HashMap;
use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::path::PathBuf;
use std::process::{exit, Command};
use std::time::{Duration, Instant, SystemTime};

/// A node that has not sent HELLO for this long is offline: no forwarding to it, and an event.
const OFFLINE_AFTER: Duration = Duration::from_secs(60);
/// HELLO timestamps further than this from the relay's clock are refused (replay window).
const MAX_SKEW_NANOS: u64 = 60_000_000_000;
const TICK: Duration = Duration::from_secs(1);
const ROSTER_CHECK: Duration = Duration::from_secs(5);
/// Roster and release bytes a node may pull in a burst, at least. A meshd update (manifest +
/// one binary) is about 1 MB, so this allows several. With module files published the burst
/// grows to twice the largest single-platform release (see `Published::burst`), so one node can
/// fetch everything for its platform once, re-asks included. A spoofed source address turns a
/// relay into an amplifier aimed at a registered node for at most one burst, refilled hourly.
const FETCH_BURST: f64 = 8.0 * 1024.0 * 1024.0;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let res = match args.first().map(String::as_str) {
        Some("run") => run(&args[1..]),
        Some("version" | "--version") => {
            println!("mesh-relay {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        _ => {
            eprintln!(
                "usage:\n  mesh-relay run [--dir D] [--listen PORT] [--event-url URL] [--event-cmd CMD]\n\n\
                 D (default /etc/flytomesh) holds `key` (relay X25519 private key, from `meshd genkey`),\n\
                 `admin.pub` (roster signing public key), `roster` (re-read when it changes) and\n\
                 optionally `release/` (signed manifest + binaries nodes update themselves from).\n\
                 Node events (offline / back online) are optional and go wherever you point them:\n\
                 POSTed as JSON to URL (with `Authorization: Bearer <D/event.token>` when that file\n\
                 exists), and / or piped as JSON to `sh -c CMD` on stdin. Neither set: no events."
            );
            exit(2);
        }
    };
    if let Err(e) = res {
        eprintln!("mesh-relay: {e}");
        exit(1);
    }
}

struct Args {
    dir: PathBuf,
    listen: u16,
    event_url: Option<String>,
    event_cmd: Option<String>,
}

fn parse(args: &[String]) -> io::Result<Args> {
    let bad = |m: String| io::Error::new(io::ErrorKind::InvalidInput, m);
    let mut a = Args { dir: "/etc/flytomesh".into(), listen: meshproto::RELAY_PORT, event_url: None, event_cmd: None };
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        let v = it.next().ok_or_else(|| bad(format!("{flag} needs a value")))?;
        match flag.as_str() {
            "--dir" => a.dir = PathBuf::from(v),
            "--listen" => a.listen = v.parse().map_err(|_| bad("bad --listen".into()))?,
            "--event-url" => a.event_url = Some(v.clone()),
            "--event-cmd" => a.event_cmd = Some(v.clone()),
            _ => return Err(bad(format!("unknown flag {flag}"))),
        }
    }
    Ok(a)
}

struct Node {
    name: String,
    key: PairKey,
    addr: Option<SocketAddr>,
    /// Largest HELLO timestamp accepted; a HELLO must be strictly newer (replay protection).
    last_ts: u64,
    last_seen: Option<Instant>,
    /// An offline alert went out and no back-online alert yet.
    alerted: bool,
    /// Bytes left for ROSTER_PART / BLOB_PART replies, and when it was last refilled.
    budget: f64,
    budget_at: Instant,
}

impl Node {
    fn online(&self, now: Instant) -> bool {
        self.last_seen.is_some_and(|t| now.duration_since(t) < OFFLINE_AFTER)
    }

    /// Takes `bytes` from the fetch budget (at most `burst`, refilled over an hour); false (and
    /// nothing taken) when it is used up.
    fn spend(&mut self, bytes: usize, now: Instant, burst: f64) -> bool {
        let refill = now.duration_since(self.budget_at).as_secs_f64() * burst / 3600.0;
        self.budget = (self.budget + refill).min(burst);
        self.budget_at = now;
        if self.budget < bytes as f64 {
            return false;
        }
        self.budget -= bytes as f64;
        true
    }
}

struct Relay {
    sock: UdpSocket,
    secret: x25519::StaticSecret,
    name: String,
    roster_path: String,
    roster_mtime: Option<SystemTime>,
    admin_pub: [u8; 32],
    roster: Roster,
    nodes: HashMap<u16, Node>,
    by_addr: HashMap<SocketAddr, u16>,
    events: Events,
    /// `<dir>/release`: a signed manifest plus the files it lists, named by SHA-256 hex.
    release_dir: PathBuf,
    release_mtime: Option<SystemTime>,
    release: Option<Published>,
}

/// The release this relay hands out: version, the manifest itself, and the size of every file
/// it lists by SHA-256. Files are read from `release_dir` when asked for, not kept in memory;
/// nodes check the hash of what they receive.
struct Published {
    version: u64,
    manifest: [u8; 32],
    raw: Vec<u8>,
    files: HashMap<[u8; 32], u64>,
    /// Per-node fetch budget while this release is out.
    burst: f64,
}

impl Published {
    fn new(rel: &Release, raw: Vec<u8>) -> Published {
        let mut per_platform: HashMap<&str, u64> = HashMap::new();
        for f in rel.files() {
            *per_platform.entry(&f.platform).or_default() += f.size;
        }
        let largest = per_platform.values().max().copied().unwrap_or(0) + raw.len() as u64;
        Published {
            version: rel.version,
            manifest: release::sha256(&raw),
            files: rel.files().map(|f| (f.sha256, f.size)).collect(),
            burst: FETCH_BURST.max(2.0 * largest as f64),
            raw,
        }
    }
}

fn run(args: &[String]) -> io::Result<()> {
    let a = parse(args)?;
    let secret = meshproto::read_secret(&a.dir.join("key").to_string_lossy())?;
    let admin_pub = meshproto::decode_key(&std::fs::read_to_string(a.dir.join("admin.pub"))?)?;
    let roster_path = a.dir.join("roster").to_string_lossy().into_owned();
    let roster = Roster::parse_verified(&std::fs::read(&roster_path)?, &admin_pub)?;
    let me = x25519::PublicKey::from(&secret);
    let name = roster
        .relay_by_key(me.as_bytes())
        .ok_or_else(|| meshproto::invalid("this relay's key is not in the roster"))?
        .name
        .clone();
    let sock = UdpSocket::bind(("0.0.0.0", a.listen))?;
    sock.set_read_timeout(Some(TICK))?;
    let mut r = Relay {
        sock,
        secret,
        name,
        roster_mtime: mtime(&roster_path),
        release_dir: a.dir.join("release"),
        release_mtime: None,
        release: None,
        roster_path,
        admin_pub,
        roster: Roster { version: 0, relays: vec![], nodes: vec![], raw: vec![] },
        nodes: HashMap::new(),
        by_addr: HashMap::new(),
        events: Events {
            url: a.event_url,
            cmd: a.event_cmd,
            token: std::fs::read_to_string(a.dir.join("event.token")).ok().map(|t| t.trim().to_string()),
            relay: String::new(),
        },
    };
    r.events.relay = r.name.clone();
    r.load(roster);
    eprintln!(
        "mesh-relay: {} on udp :{}, roster v{} with {} nodes",
        r.name,
        a.listen,
        r.roster.version,
        r.nodes.len()
    );

    let mut buf = vec![0u8; 65536];
    let mut next_tick = Instant::now() + TICK;
    let mut next_roster_check = Instant::now() + ROSTER_CHECK;
    loop {
        match r.sock.recv_from(&mut buf) {
            Ok((n, src)) => r.packet(&mut buf[..n], src),
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {}
            Err(e) => eprintln!("mesh-relay: recv: {e}"),
        }
        let now = Instant::now();
        if now >= next_tick {
            next_tick = now + TICK;
            r.check_liveness(now);
        }
        if now >= next_roster_check {
            next_roster_check = now + ROSTER_CHECK;
            r.reload_if_changed();
            r.reload_release_if_changed();
        }
    }
}

/// Up to `len` bytes of `path` from `off`; shorter at the end of the file.
fn read_range(path: &std::path::Path, off: u64, len: usize) -> io::Result<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    f.seek(SeekFrom::Start(off))?;
    let mut out = Vec::with_capacity(len);
    f.take(len as u64).read_to_end(&mut out)?;
    Ok(out)
}

fn mtime(path: &str) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

impl Relay {
    /// Installs a roster, keeping the registration of nodes that are still in it.
    fn load(&mut self, roster: Roster) {
        let mut old = std::mem::take(&mut self.nodes);
        for n in &roster.nodes {
            let prev = old.remove(&n.id).filter(|p| p.name == n.name);
            let key = PairKey::new(&self.secret, &n.key);
            let node = match prev {
                // Same id and name: keep the address only if the key is unchanged.
                Some(p) if self.roster.nodes.iter().any(|o| o.id == n.id && o.key == n.key) => Node { key, ..p },
                _ => Node {
                    name: n.name.clone(),
                    key,
                    addr: None,
                    last_ts: 0,
                    last_seen: None,
                    alerted: false,
                    budget: self.burst(),
                    budget_at: Instant::now(),
                },
            };
            self.nodes.insert(n.id, node);
        }
        self.by_addr = self.nodes.iter().filter_map(|(id, n)| n.addr.map(|a| (a, *id))).collect();
        self.roster = roster;
    }

    fn burst(&self) -> f64 {
        self.release.as_ref().map_or(FETCH_BURST, |p| p.burst)
    }

    fn reload_if_changed(&mut self) {
        let m = mtime(&self.roster_path);
        if m == self.roster_mtime {
            return;
        }
        self.roster_mtime = m;
        let res = std::fs::read(&self.roster_path).and_then(|raw| Roster::parse_verified(&raw, &self.admin_pub));
        match res {
            Ok(r) if r.version > self.roster.version => {
                eprintln!("mesh-relay: roster v{} -> v{}", self.roster.version, r.version);
                self.load(r);
            }
            Ok(r) => eprintln!("mesh-relay: roster file v{} is not newer than v{}, ignored", r.version, self.roster.version),
            Err(e) => eprintln!("mesh-relay: roster file rejected: {e}"),
        }
    }

    fn packet(&mut self, p: &mut [u8], src: SocketAddr) {
        let Some((kind, field)) = wire::control(p) else { return };
        match kind {
            wire::HELLO => self.hello(p, field, src),
            wire::FWD => self.forward(p, field, src),
            wire::ROSTER_GET => self.roster_part(p, src),
            wire::BLOB_GET => self.blob_parts(p, src),
            _ => {}
        }
    }

    fn hello(&mut self, p: &mut [u8], id: u16, src: SocketAddr) {
        let Some(node) = self.nodes.get_mut(&id) else { return };
        let Some(plain) = wire::open(&node.key, p) else { return };
        let Some(ts) = wire::get_u64(plain) else { return };
        let now_ns = meshproto::now_nanos();
        if ts <= node.last_ts || ts.abs_diff(now_ns) > MAX_SKEW_NANOS {
            return;
        }
        node.last_ts = ts;
        node.last_seen = Some(Instant::now());
        if node.addr != Some(src) {
            if let Some(old) = node.addr.replace(src) {
                self.by_addr.remove(&old);
            }
            eprintln!("mesh-relay: {} ({id}) at {src}", node.name);
            self.by_addr.insert(src, id);
        }
        if node.alerted {
            node.alerted = false;
            eprintln!("mesh-relay: {} back online", node.name);
            self.events.send(&node.name, "online", "恢复在线");
        }

        let SocketAddr::V4(src4) = src else { return };
        let mut ack = Vec::with_capacity(32);
        ack.extend_from_slice(&ts.to_be_bytes());
        wire::put_addr(&mut ack, src4);
        ack.extend_from_slice(&self.roster.version.to_be_bytes());
        if let Some(p) = &self.release {
            ack.extend_from_slice(&p.version.to_be_bytes());
            ack.extend_from_slice(&p.manifest);
        }
        let msg = wire::sealed(wire::HELLO_ACK, 0, &node.key, &ack);
        self.sock.send_to(&msg, src).ok();
    }

    /// FWD from a registered node: rewrite the header in place to FWD_FROM and send it on.
    /// The sender is whoever registered `src`, never what the packet claims.
    fn forward(&mut self, p: &mut [u8], dst: u16, src: SocketAddr) {
        let Some(&from) = self.by_addr.get(&src) else { return };
        let now = Instant::now();
        let to = self.nodes.get(&dst).filter(|n| n.online(now)).and_then(|n| n.addr);
        let Some(to) = to else {
            // Sealed, echoing the sender's last HELLO: a v9 node only acts on a fresh one.
            // Older nodes read just the header.
            let Some(sender) = self.nodes.get(&from) else { return };
            let msg = wire::sealed(wire::NO_ROUTE, dst, &sender.key, &sender.last_ts.to_be_bytes());
            self.sock.send_to(&msg, src).ok();
            return;
        };
        p[..wire::HDR].copy_from_slice(&wire::header(wire::FWD_FROM, from));
        self.sock.send_to(p, to).ok();
    }

    /// Only registered addresses get the roster, within their fetch budget: the reply is
    /// larger than the request.
    fn roster_part(&mut self, p: &[u8], src: SocketAddr) {
        let Some(&id) = self.by_addr.get(&src) else { return };
        let Some(off) = wire::get_u32(&p[wire::HDR..]) else { return };
        let raw = &self.roster.raw;
        let off = (off as usize).min(raw.len());
        let end = (off + wire::ROSTER_CHUNK).min(raw.len());
        let mut out = Vec::with_capacity(wire::HDR + 8 + end - off);
        out.extend_from_slice(&wire::header(wire::ROSTER_PART, 0));
        out.extend_from_slice(&(raw.len() as u32).to_be_bytes());
        out.extend_from_slice(&(off as u32).to_be_bytes());
        out.extend_from_slice(&raw[off..end]);
        let burst = self.burst();
        let Some(node) = self.nodes.get_mut(&id) else { return };
        if node.spend(out.len(), Instant::now(), burst) {
            self.sock.send_to(&out, src).ok();
        }
    }

    /// Loads `<dir>/release/manifest` and checks the files it lists when the manifest changes.
    /// A manifest that does not verify, or a listed file that is missing or does not hash to
    /// its entry, is refused as a whole and the previous release stays published. A new release
    /// tops every node's budget up to its burst, so each can fetch it once right away.
    fn reload_release_if_changed(&mut self) {
        let path = self.release_dir.join("manifest");
        let m = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        if m == self.release_mtime {
            return;
        }
        self.release_mtime = m;
        let loaded = (|| -> io::Result<Published> {
            let raw = std::fs::read(&path)?;
            let rel = Release::parse_verified(&raw, &self.admin_pub)?;
            for b in rel.files() {
                let data = std::fs::read(self.release_dir.join(release::hex(&b.sha256)))?;
                if release::sha256(&data) != b.sha256 || data.len() as u64 != b.size {
                    return Err(meshproto::invalid(&format!("{} {} does not match the manifest", b.module, b.platform)));
                }
            }
            Ok(Published::new(&rel, raw))
        })();
        match loaded {
            Ok(p) if self.release.as_ref().is_none_or(|old| p.version > old.version) => {
                eprintln!(
                    "mesh-relay: release v{} published ({} files, fetch budget {} MiB per node)",
                    p.version,
                    p.files.len(),
                    (p.burst / 1048576.0).ceil()
                );
                for n in self.nodes.values_mut() {
                    n.budget = n.budget.max(p.burst);
                }
                self.release = Some(p);
            }
            Ok(p) => eprintln!("mesh-relay: release v{} is not newer, ignored", p.version),
            Err(e) if e.kind() == io::ErrorKind::NotFound && self.release.is_none() => {}
            Err(e) => eprintln!("mesh-relay: release rejected: {e}"),
        }
    }

    /// Up to `BLOB_WINDOW` chunks of a published blob, to registered addresses only and
    /// within their fetch budget.
    fn blob_parts(&mut self, p: &[u8], src: SocketAddr) {
        let Some(&id) = self.by_addr.get(&src) else { return };
        if p.len() < wire::HDR + 37 {
            return;
        }
        let Some(node) = self.nodes.get_mut(&id) else { return };
        let Some(rel) = &self.release else { return };
        let b = &p[wire::HDR..];
        let hash: [u8; 32] = b[..32].try_into().expect("32 bytes");
        let (Some(off), count) = (wire::get_u32(&b[32..]), b[36].min(wire::BLOB_WINDOW)) else { return };
        let off = off as usize;
        let window = count as usize * wire::ROSTER_CHUNK;
        let (total, data) = if hash == rel.manifest {
            let end = (off + window).min(rel.raw.len());
            (rel.raw.len(), rel.raw.get(off..end).unwrap_or_default().to_vec())
        } else {
            let Some(&size) = rel.files.get(&hash) else { return };
            match read_range(&self.release_dir.join(release::hex(&hash)), off as u64, window.min(size.saturating_sub(off as u64) as usize)) {
                Ok(d) => (size as usize, d),
                Err(e) => {
                    eprintln!("mesh-relay: reading release file {}: {e}", release::hex(&hash));
                    return;
                }
            }
        };
        for (i, piece) in data.chunks(wire::ROSTER_CHUNK).enumerate() {
            let at = off + i * wire::ROSTER_CHUNK;
            let mut out = Vec::with_capacity(wire::HDR + 40 + piece.len());
            out.extend_from_slice(&wire::header(wire::BLOB_PART, 0));
            out.extend_from_slice(&hash);
            out.extend_from_slice(&(total as u32).to_be_bytes());
            out.extend_from_slice(&(at as u32).to_be_bytes());
            out.extend_from_slice(piece);
            if !node.spend(out.len(), Instant::now(), rel.burst) {
                break;
            }
            self.sock.send_to(&out, src).ok();
        }
    }

    fn check_liveness(&mut self, now: Instant) {
        for n in self.nodes.values_mut() {
            if n.last_seen.is_some() && !n.online(now) && !n.alerted {
                n.alerted = true;
                eprintln!("mesh-relay: {} offline for over a minute", n.name);
                self.events.send(&n.name, "offline", "已离线超过 1 分钟");
            }
        }
    }
}

/// Node liveness events: an interface, not a notification channel. The relay only emits
/// `{"source","relay","node","event":"offline"|"online","at","detail"}`; where it goes (an
/// HTTP endpoint, a script that posts to any chat tool, nowhere) is up to whoever runs it.
/// Every relay reports the same event, so a receiver that cares should dedup by (node, event).
struct Events {
    url: Option<String>,
    cmd: Option<String>,
    token: Option<String>,
    relay: String,
}

impl Events {
    /// Fire and forget in the background: an event must never block forwarding.
    fn send(&self, node: &str, event: &str, detail: &str) {
        if self.url.is_none() && self.cmd.is_none() {
            return;
        }
        let body = format!(
            "{{\"source\":\"flytomesh\",\"relay\":{},\"node\":{},\"event\":{},\"at\":{},\"detail\":{}}}",
            json_str(&self.relay),
            json_str(node),
            json_str(event),
            json_str(&rfc3339_now()),
            json_str(detail)
        );
        // HTTP through curl (the relay has no TLS stack). The token goes to curl on stdin
        // (`-K -`), so it never shows up in the process list.
        if let Some(url) = &self.url {
            let mut cfg = format!("url = {}\nheader = \"Content-Type: application/json\"\ndata = {}\n", curl_str(url), curl_str(&body));
            if let Some(t) = &self.token {
                cfg.push_str(&format!("header = {}\n", curl_str(&format!("Authorization: Bearer {t}"))));
            }
            spawn_with_stdin(Command::new("curl").args(["-sS", "-f", "-m", "10", "-K", "-"]), cfg);
        }
        if let Some(cmd) = &self.cmd {
            spawn_with_stdin(Command::new("sh").args(["-c", cmd]), body + "\n");
        }
    }
}

/// Runs a command in the background with `input` on its stdin.
fn spawn_with_stdin(cmd: &mut Command, input: String) {
    let spawned = cmd.stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::null()).spawn();
    match spawned {
        Ok(mut child) => {
            use std::io::Write;
            if let Some(mut stdin) = child.stdin.take() {
                stdin.write_all(input.as_bytes()).ok();
            }
            // Reap it from a short-lived thread so it does not linger as a zombie.
            std::thread::spawn(move || child.wait());
        }
        Err(e) => eprintln!("mesh-relay: event: {e}"),
    }
}

fn json_str(s: &str) -> String {
    let mut o = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

/// A double-quoted curl config value (curl unescapes \\ and \" inside quotes).
fn curl_str(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Current UTC time as `YYYY-MM-DDTHH:MM:SSZ`.
fn rfc3339_now() -> String {
    let secs = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let (days, rem) = ((secs / 86400) as i64, secs % 86400);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escaping() {
        assert_eq!(json_str("a\"b\\c"), r#""a\"b\\c""#);
        assert_eq!(curl_str(r#"{"k":"v\"}"#), r#""{\"k\":\"v\\\"}""#);
    }

    fn pair() -> (PairKey, PairKey) {
        let (a, b) = (x25519::StaticSecret::from([7u8; 32]), x25519::StaticSecret::from([9u8; 32]));
        let (pa, pb) = (x25519::PublicKey::from(&a).to_bytes(), x25519::PublicKey::from(&b).to_bytes());
        (PairKey::new(&a, &pb), PairKey::new(&b, &pa))
    }

    #[test]
    fn fetch_budget_caps_bursts_and_refills() {
        let now = Instant::now();
        let mut n = Node {
            name: "laptop1".into(),
            key: pair().0,
            addr: None,
            last_ts: 0,
            last_seen: None,
            alerted: false,
            budget: FETCH_BURST,
            budget_at: now,
        };
        let chunk = wire::HDR + 40 + wire::ROSTER_CHUNK;
        let mut sent = 0;
        while n.spend(chunk, now, FETCH_BURST) {
            sent += chunk;
        }
        assert!(sent as f64 <= FETCH_BURST && sent as f64 > FETCH_BURST - chunk as f64);
        assert!(!n.spend(chunk, now + Duration::from_millis(100), FETCH_BURST), "refill is slow");
        assert!(n.spend(chunk, now + Duration::from_secs(2), FETCH_BURST), "but it comes back");
    }

    /// A relay with one registered node (id 1) at `client`, publishing `files` (module, data)
    /// for linux-x86_64 from a fresh directory.
    fn relay_with_release(files: &[(&str, &[u8])], client: SocketAddr) -> (Relay, PathBuf) {
        let seed = [9u8; 32];
        let dir = std::env::temp_dir().join(format!("mesh-relay-test-{}-{}", std::process::id(), meshproto::random_u64()));
        std::fs::create_dir_all(dir.join("release")).unwrap();
        let mut body = "flytomesh-release 1\nversion 10\n".to_string();
        for (module, data) in files {
            let h = release::hex(&release::sha256(data));
            std::fs::write(dir.join("release").join(&h), data).unwrap();
            let prefix = if *module == "meshd" { String::new() } else { "#fmesh module ".to_string() };
            body += &format!("{prefix}{module} linux-x86_64 {h} {}\n", data.len());
        }
        std::fs::write(dir.join("release/manifest"), release::sign(&body, &seed).unwrap()).unwrap();
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let mut nodes = HashMap::new();
        nodes.insert(
            1,
            Node {
                name: "laptop1".into(),
                key: pair().0,
                addr: Some(client),
                last_ts: 0,
                last_seen: Some(Instant::now()),
                alerted: false,
                budget: 0.0,
                budget_at: Instant::now(),
            },
        );
        let r = Relay {
            sock,
            secret: x25519::StaticSecret::from([7u8; 32]),
            name: "relay1".into(),
            roster_path: String::new(),
            roster_mtime: None,
            admin_pub: meshproto::roster::admin_public(&seed).unwrap(),
            roster: Roster { version: 0, relays: vec![], nodes: vec![], raw: vec![] },
            nodes,
            by_addr: HashMap::from([(client, 1)]),
            events: Events { url: None, cmd: None, token: None, relay: String::new() },
            release_dir: dir.join("release"),
            release_mtime: None,
            release: None,
        };
        (r, dir)
    }

    #[test]
    fn module_files_are_served_and_the_budget_covers_a_platform() {
        let client = UdpSocket::bind("127.0.0.1:0").unwrap();
        client.set_read_timeout(Some(Duration::from_millis(500))).unwrap();
        let meshd = vec![1u8; 3000];
        let agent: Vec<u8> = (0..20_000u32).map(|i| i as u8).collect();
        let (mut r, dir) = relay_with_release(&[("meshd", &meshd), ("agent-inbox", &agent)], client.local_addr().unwrap());
        r.reload_release_if_changed();
        let p = r.release.as_ref().expect("published");
        assert_eq!(p.files.len(), 2);
        assert_eq!(p.burst, FETCH_BURST, "small releases keep the floor");
        assert_eq!(r.nodes[&1].budget, FETCH_BURST, "a new release tops the budget up");

        let hash = release::sha256(&agent);
        let mut got = vec![0u8; agent.len()];
        let mut filled = 0;
        let mut off = 0u32;
        while filled < agent.len() {
            let mut m = wire::header(wire::BLOB_GET, 0).to_vec();
            m.extend_from_slice(&hash);
            m.extend_from_slice(&off.to_be_bytes());
            m.push(wire::BLOB_WINDOW);
            r.blob_parts(&m, client.local_addr().unwrap());
            let mut buf = [0u8; 2048];
            for _ in 0..wire::BLOB_WINDOW {
                let Ok(n) = client.recv(&mut buf) else { break };
                let b = &buf[wire::HDR..n];
                assert_eq!(&b[..32], &hash);
                assert_eq!(wire::get_u32(&b[32..]), Some(agent.len() as u32));
                let at = wire::get_u32(&b[36..]).unwrap() as usize;
                got[at..at + b.len() - 40].copy_from_slice(&b[40..]);
                filled += b.len() - 40;
                off = (at + b.len() - 40) as u32;
                if filled == agent.len() {
                    break;
                }
            }
        }
        assert_eq!(got, agent);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn burst_grows_with_the_largest_platform() {
        let big = 6 * 1024 * 1024;
        let rel = Release {
            version: 11,
            meshd: vec![release::Binary { module: "meshd".into(), platform: "a".into(), sha256: [1; 32], size: 1 << 20 }],
            modules: vec![
                release::Binary { module: "fmesh-desktop".into(), platform: "a".into(), sha256: [2; 32], size: big },
                release::Binary { module: "fmesh-desktop".into(), platform: "b".into(), sha256: [3; 32], size: big },
            ],
        };
        let p = Published::new(&rel, vec![0; 100]);
        assert_eq!(p.burst, 2.0 * ((1 << 20) + big + 100) as f64);
    }

    #[test]
    fn no_route_keeps_the_old_header_and_seals_the_echo() {
        let (relay_side, node_side) = pair();
        let mut msg = wire::sealed(wire::NO_ROUTE, 42, &relay_side, &1234u64.to_be_bytes());
        // Nodes before v9 read only this.
        assert_eq!(wire::control(&msg), Some((wire::NO_ROUTE, 42)));
        let plain = wire::open(&node_side, &mut msg).expect("opens with the pair key");
        assert_eq!(wire::get_u64(plain), Some(1234));
        let mut forged = wire::header(wire::NO_ROUTE, 42).to_vec();
        forged.extend_from_slice(&[0u8; 40]);
        assert!(wire::open(&node_side, &mut forged).is_none());
    }

    #[test]
    fn time_format() {
        let t = rfc3339_now();
        assert_eq!(t.len(), 20);
        assert!(t.starts_with("20") && t.ends_with('Z'));
    }
}
