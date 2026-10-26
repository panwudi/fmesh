//! The node daemon: every peer in the roster, reached directly when hole punching works and
//! through a relay otherwise.
//!
//! Three threads, blocking I/O, no async runtime:
//! - encrypt: TUN (drained in batches) -> look up the peer by destination IP -> encrypt into
//!   a batch, each packet tagged with where it goes.
//! - send: batches -> UDP. Runs of packets for the same connected socket go out in one
//!   batched syscall (`sendmsg_x` / `sendmmsg`).
//! - receive: poll over all sockets (250 ms timeout) -> WireGuard / relay / disco handling,
//!   plus all timers (WireGuard, relay HELLO, hole punching, roster fetch, status file).
//!
//! Sockets, all on the node's port (SO_REUSEPORT, see `udp`): `wan` (unconnected, pinned past
//! a VPN default route), `lan` (unconnected, never pinned; on-link destinations while `wan` is
//! pinned), and one connected socket per peer with a direct path.
//!
//! The receive thread owns all path decisions and publishes the result per peer
//! (`PeerState::path`, `conn`); the send side only reads it.

use crate::netif::{self, LocalAddr};
use crate::tun::Tun;
use crate::update::Updater;
use crate::{batch, dns, udp};
use boringtun::noise::handshake::parse_handshake_anon;
use boringtun::noise::rate_limiter::RateLimiter;
use boringtun::noise::{Packet, Tunn, TunnResult};
use meshproto::crypto::PairKey;
use meshproto::roster::Roster;
use meshproto::service::{self, ModuleInfo, Service};
use meshproto::{wire, x25519};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

/// Tunnel MTU. The largest packet on the wire is MTU + 32 (WireGuard) + 4 (relay header) +
/// 28 (IPv4 + UDP) = 1444 bytes. The office line measured a 1480-byte path MTU and drops
/// fragments, so 1420 (1484 through a relay) killed TCP over relays (2026-10-16); 1380 also
/// leaves room for PPPoE and VPN overlays elsewhere.
pub const MTU: usize = 1380;
const TICK: Duration = Duration::from_millis(250);
const KEEPALIVE: u16 = 25;
/// HELLO interval while a relay answers, and the retry interval while it does not.
const HELLO_EVERY: Duration = Duration::from_secs(25);
/// Local "check for updates" requests ask the relays at most this often.
const CHECK_MIN_GAP: Duration = Duration::from_secs(5);
const HELLO_RETRY: Duration = Duration::from_secs(3);
/// A relay that has not answered a HELLO for this long is considered down.
const RELAY_DEAD: Duration = Duration::from_secs(60);
/// Without a direct path: CALL + PING this often. With one: PING it this often.
const PROBE_EVERY: Duration = Duration::from_secs(5);
const DIRECT_PING_EVERY: Duration = Duration::from_secs(10);
/// With a direct path: also re-probe every candidate this often, to find a better path after
/// a network change (the current one keeps working meanwhile).
const FULL_PROBE_EVERY: Duration = Duration::from_secs(60);
/// A direct path with no PONG for this long may be replaced by any address the peer is
/// verifiably sending from.
const DIRECT_STALE: Duration = Duration::from_secs(15);
/// Back off probing a peer the relay says is not registered.
const NO_ROUTE_BACKOFF: Duration = Duration::from_secs(15);
/// Three missed direct PINGs: fall back to the relay.
const DIRECT_DEAD: Duration = Duration::from_secs(35);
const PING_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_CANDIDATES: usize = 8;
/// Local addresses are re-read this often (getifaddrs, cheap): a change means a network change,
/// and every peer socket and direct path is rebuilt. The VPN check runs a command, so less often.
const LOCAL_ADDRS_EVERY: Duration = Duration::from_secs(5);
const VPN_CHECK_EVERY: Duration = Duration::from_secs(30);
const STATUS_EVERY: Duration = Duration::from_secs(2);
/// A peer whose last WireGuard handshake is older than this is offline (sessions last 180 s
/// and keepalives renew them while the peer is up).
const ONLINE: Duration = Duration::from_secs(180);
/// Local services are re-announced to each online peer this often, and on every change.
const ANNOUNCE_EVERY: Duration = Duration::from_secs(30);
/// How long peers may keep our announcement (several missed re-announcements).
const ANNOUNCE_TTL: u16 = 120;
/// Received announcements are kept at most this long, whatever the sender asks.
const MAX_SERVICES_TTL: u16 = 300;
/// An announcement whose timestamp is further than this from our clock is dropped (nodes keep
/// within 60 s of the relays, so honest ones are well inside).
const MAX_SERVICES_SKEW_NANOS: u64 = 120_000_000_000;
/// World-readable (the config directory is not): `meshd status` and the tray icon read it.
#[cfg(unix)]
pub const STATUS_FILE: &str = "/var/run/flytomesh.status";
#[cfg(windows)]
pub const STATUS_FILE: &str = r"C:\ProgramData\flytoMesh\status";
/// World-readable copies of the verified signed roster and the admin public key, so local
/// programs can check node ids and keys themselves (docs/network.md section 5). Nothing secret.
#[cfg(unix)]
pub const PUBLIC_ROSTER: &str = "/var/run/flytomesh.roster";
#[cfg(unix)]
pub const PUBLIC_ADMIN: &str = "/var/run/flytomesh.admin.pub";
#[cfg(windows)]
pub const PUBLIC_ROSTER: &str = r"C:\ProgramData\flytoMesh\public\roster";
#[cfg(windows)]
pub const PUBLIC_ADMIN: &str = r"C:\ProgramData\flytoMesh\public\admin.pub";
/// Headroom in front of every outgoing WireGuard packet for the relay header.
const HEAD: usize = wire::HDR;
const BUF: usize = 65536;
/// Packets per encrypt batch, one slot each: headroom + an MTU-sized packet + 32 bytes of
/// WireGuard overhead fits in 2 KB.
const BATCH: usize = batch::MAX;
const SLOT: usize = 2048;
/// Batches in flight between the encrypt and send threads.
const BATCHES: usize = 8;
/// Datagrams drained from one readable socket before looking at the others.
const RX_DRAIN: usize = 64;

#[derive(Clone)]
pub struct Config {
    pub dir: PathBuf,
    pub listen: u16,
    /// Never use a direct path (testing the relay path between two machines on one LAN).
    pub force_relay: bool,
    /// Do not pin the socket past a VPN default route (testing only: lets a VPN Mac use a
    /// second LAN link such as Thunderbolt, at the cost of relay traffic going through the VPN).
    pub no_vpn_bypass: bool,
    /// Inbound allowlist for traffic arriving from the mesh: `None` lets everything in;
    /// otherwise only ICMP, TCP / UDP to these ports, and TCP that is not a new connection.
    pub inbound: Option<Inbound>,
}

/// Which ports other mesh nodes may open on this machine (e.g. only ssh on a production box
/// with no firewall of its own). Packets that fail it are dropped before reaching the OS.
#[derive(Clone, Debug, Default)]
pub struct Inbound {
    pub tcp: Vec<u16>,
    pub udp: Vec<u16>,
}

impl Inbound {
    /// `tcp:22,udp:53` style.
    pub fn parse(spec: &str) -> io::Result<Inbound> {
        let mut r = Inbound::default();
        for item in spec.split(',').map(str::trim).filter(|x| !x.is_empty()) {
            let bad = || io::Error::new(io::ErrorKind::InvalidInput, format!("bad --inbound item {item} (want tcp:PORT or udp:PORT)"));
            let (proto, port) = item.split_once(':').ok_or_else(bad)?;
            let port: u16 = port.parse().map_err(|_| bad())?;
            match proto {
                "tcp" => r.tcp.push(port),
                "udp" => r.udp.push(port),
                _ => return Err(bad()),
            }
        }
        Ok(r)
    }

    /// Whether a decrypted IP packet from a peer may be delivered.
    pub fn allows(&self, pkt: &[u8]) -> bool {
        if pkt.len() < 20 || pkt[0] >> 4 != 4 {
            return false; // the mesh is IPv4 only
        }
        let ihl = ((pkt[0] & 0x0F) as usize) * 4;
        let frag_off = u16::from_be_bytes([pkt[6], pkt[7]]) & 0x1FFF;
        let l4 = pkt.get(ihl..).unwrap_or(&[]);
        match pkt[9] {
            1 => true, // ICMP
            // Later fragments carry no ports; their first fragment was already checked.
            6 | 17 if frag_off != 0 => true,
            6 if l4.len() >= 14 => {
                let dport = u16::from_be_bytes([l4[2], l4[3]]);
                let flags = l4[13];
                let new_conn = flags & 0x02 != 0 && flags & 0x10 == 0; // SYN without ACK
                self.tcp.contains(&dport) || !new_conn
            }
            17 if l4.len() >= 4 => self.udp.contains(&u16::from_be_bytes([l4[2], l4[3]])),
            _ => false,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Path {
    None,
    Direct(SocketAddr),
    Relay(SocketAddr),
}

struct Peer {
    id: u16,
    name: String,
    ip: Ipv4Addr,
    key: [u8; 32],
    disco: PairKey,
    st: Mutex<PeerState>,
}

struct PeerState {
    tunn: Tunn,
    /// Where WireGuard packets go now; written by the receive thread only.
    path: Path,
    direct: Option<SocketAddr>,
    /// Relay index to use for this peer: the last one a packet from it came through.
    relay: Option<usize>,
    /// The peer's RTT to each relay (ms), from its last CALL: a relay is chosen to minimize
    /// my RTT + its RTT, so both directions agree and a peer next to a relay is reached there.
    peer_relay_rtt: Vec<Option<u32>>,
    candidates: Vec<SocketAddrV4>,
    pings: Vec<(u64, SocketAddr, Instant)>,
    last_pong: Option<Instant>,
    /// RTT of the current direct path; a new path must be clearly faster to replace it.
    direct_rtt: Option<Duration>,
    next_probe: Instant,
    next_full_probe: Instant,
    /// Socket connected to `direct`, once the receive thread has opened it.
    conn: Option<Arc<Conn>>,
    /// `direct` is on-link while `wan` is pinned: unconnected sends must use `lan`.
    direct_lan: bool,
    /// The peer's announced services, until `services_until`.
    services: Vec<Service>,
    services_until: Option<Instant>,
    /// Timestamp of the last accepted announcement (replays and reordering are dropped).
    services_ts: u64,
    /// When our services were last sent to this peer, and whether that list was non-empty
    /// (so a withdrawal is owed when ours becomes empty).
    services_sent: Option<Instant>,
    services_sent_some: bool,
    /// The peer's announced modules (since v13), like its services.
    modules: Vec<ModuleInfo>,
    modules_until: Option<Instant>,
    modules_ts: u64,
    modules_sent: Option<Instant>,
}

/// A socket connected to one peer address.
struct Conn {
    sock: UdpSocket,
    peer: SocketAddr,
}

struct Shared {
    wan: Arc<UdpSocket>,
    lan: Arc<UdpSocket>,
    /// `<name>.mesh` responder on loopback (None if the port was taken).
    dns: Option<UdpSocket>,
    peers: Vec<Peer>,
    by_ip: HashMap<Ipv4Addr, usize>,
    by_id: HashMap<u16, usize>,
}

struct RelayState {
    name: String,
    addr: SocketAddr,
    key: PairKey,
    /// Timestamp of the last HELLO sent; only an ACK echoing it counts.
    hello_ts: u64,
    /// Timestamp echoed by the last accepted HELLO_ACK.
    acked_ts: u64,
    hello_at: Instant,
    next_hello: Instant,
    rtt: Option<Duration>,
    last_ack: Option<Instant>,
    public: Option<SocketAddrV4>,
    version: u64,
}

/// A NO_ROUTE echo is fresh when it names a HELLO sent no earlier than the last acknowledged
/// one: the relay echoes the newest HELLO it accepted, which may be one whose ACK was lost.
fn fresh_no_route(echoed: u64, acked: u64, sent: u64) -> bool {
    acked != 0 && echoed >= acked && echoed <= sent
}

impl RelayState {
    fn alive(&self, now: Instant) -> bool {
        self.last_ack.is_some_and(|t| now.duration_since(t) < RELAY_DEAD)
    }
}

struct Fetch {
    relay: usize,
    data: Vec<u8>,
    asked: Instant,
}

/// Receive-thread state.
struct Rx<'a> {
    sh: &'a Shared,
    cfg: &'a Config,
    me: meshproto::roster::Node,
    secret: x25519::StaticSecret,
    public: x25519::PublicKey,
    admin_pub: [u8; 32],
    roster_version: u64,
    rl: Arc<RateLimiter>,
    tun: &'a Tun,
    relays: Vec<RelayState>,
    addr_peer: HashMap<SocketAddr, usize>,
    local: Vec<LocalAddr>,
    /// Interface `wan` is pinned to because a VPN owns the default route.
    bound_if: Option<String>,
    /// The peers' connected sockets, polled along with `wan` and `lan`.
    conns: Vec<Arc<Conn>>,
    /// Every node's name (lowercase) and mesh IP, for `.mesh` lookups.
    names: HashMap<String, Ipv4Addr>,
    fetch: Option<Fetch>,
    updater: Updater,
    /// Local service registrations (`None`: the directory could not be prepared).
    services_dir: Option<PathBuf>,
    local_services: Vec<crate::services::Local>,
    services_ts: u64,
    local_modules: Vec<ModuleInfo>,
    modules_ts: u64,
    /// When a local "check for updates" request last made us ask the relays.
    last_check: Option<Instant>,
    out: Vec<u8>,
}

pub fn run(cfg: Config) -> io::Result<()> {
    let secret = meshproto::read_secret(&cfg.dir.join("key").to_string_lossy())?;
    let public = x25519::PublicKey::from(&secret);
    let admin_pub = meshproto::decode_key(&std::fs::read_to_string(cfg.dir.join("admin.pub"))?)?;
    // First: count this start against a pending self-update (may roll back and exit).
    let updater = Updater::start(&cfg.dir, admin_pub)?;
    let roster_raw = std::fs::read(cfg.dir.join("roster"))?;
    let roster = Roster::parse_verified(&roster_raw, &admin_pub)?;
    let me = roster
        .node_by_key(public.as_bytes())
        .ok_or_else(|| meshproto::invalid("this node's key is not in the roster"))?
        .clone();
    // Admin key first: a reader that sees a new roster can always verify it.
    for (path, data) in [(PUBLIC_ADMIN, format!("{}\n", meshproto::b64(&admin_pub)).into_bytes()), (PUBLIC_ROSTER, roster_raw)] {
        if let Err(e) = publish(std::path::Path::new(path), &data) {
            eprintln!("meshd: publishing {path}: {e}");
        }
    }

    // One limiter for all peers, so unauthenticated handshakes are counted together; with a
    // shared limiter Tunn does not reset the count itself, the receive thread does.
    let rl = Arc::new(RateLimiter::new(&public, 100));
    let now = Instant::now();
    let mut peers = vec![];
    for n in roster.nodes.iter().filter(|n| n.id != me.id) {
        let tunn = Tunn::new(
            secret.clone(),
            x25519::PublicKey::from(n.key),
            None,
            Some(KEEPALIVE),
            n.id as u32, // session index = id << 8, so receiver_idx >> 8 finds the peer
            Some(rl.clone()),
        );
        peers.push(Peer {
            id: n.id,
            name: n.name.clone(),
            ip: n.ip,
            key: n.key,
            disco: PairKey::new(&secret, &n.key),
            st: Mutex::new(PeerState {
                tunn,
                path: Path::None,
                direct: None,
                relay: None,
                peer_relay_rtt: vec![None; roster.relays.len()],
                candidates: vec![],
                pings: vec![],
                last_pong: None,
                direct_rtt: None,
                next_probe: now,
                next_full_probe: now + FULL_PROBE_EVERY,
                conn: None,
                direct_lan: false,
                services: vec![],
                services_until: None,
                services_ts: 0,
                services_sent: None,
                services_sent_some: false,
                modules: vec![],
                modules_until: None,
                modules_ts: 0,
                modules_sent: None,
            }),
        });
    }
    let relays = roster
        .relays
        .iter()
        .map(|r| RelayState {
            name: r.name.clone(),
            addr: SocketAddr::V4(r.addr),
            key: PairKey::new(&secret, &r.key),
            hello_ts: 0,
            acked_ts: 0,
            hello_at: now,
            next_hello: now,
            rtt: None,
            last_ack: None,
            public: None,
            version: 0,
        })
        .collect();

    let tun = crate::tun::open()?;
    configure(tun.name(), me.ip)?;
    let wan = Arc::new(udp::open(cfg.listen, None, None)?);
    // Windows has one socket for everything (no SO_REUSEPORT): `lan` is `wan`.
    #[cfg(unix)]
    let lan = Arc::new(udp::open(cfg.listen, None, None)?);
    #[cfg(windows)]
    let lan = wan.clone();
    let names: Vec<(String, Ipv4Addr)> = roster.nodes.iter().map(|n| (n.name.to_ascii_lowercase(), n.ip)).collect();
    let dns_sock = match UdpSocket::bind(("127.0.0.1", dns::PORT)) {
        Ok(s) => {
            // Drained until empty by the receive thread: it must never block (on Windows
            // `udp::recv_now` is a plain recv, which on a blocking socket hung the whole
            // receive thread after the first query, 2026-10-17).
            s.set_nonblocking(true)?;
            if let Err(e) = dns::configure(tun.name(), &names) {
                eprintln!("meshd: .mesh resolver setup: {e}");
            }
            Some(s)
        }
        Err(e) => {
            eprintln!("meshd: .mesh names disabled, 127.0.0.1:{}: {e}", dns::PORT);
            None
        }
    };
    eprintln!(
        "meshd: {} ({}) on {} {}/16, udp :{}, roster v{}, {}, {} peers, {} relays{}",
        me.name,
        me.id,
        tun.name(),
        me.ip,
        cfg.listen,
        roster.version,
        updater.status(),
        peers.len(),
        roster.relays.len(),
        if cfg.force_relay { ", force-relay" } else { "" }
    );

    let sh = Shared {
        wan,
        lan,
        dns: dns_sock,
        by_ip: peers.iter().enumerate().map(|(i, p)| (p.ip, i)).collect(),
        by_id: peers.iter().enumerate().map(|(i, p)| (p.id, i)).collect(),
        peers,
    };
    let services_dir = crate::services::dir();
    let services_dir = match crate::services::prepare(&services_dir) {
        Ok(()) => Some(services_dir),
        Err(e) => {
            eprintln!("meshd: service registrations disabled ({}: {e})", services_dir.display());
            None
        }
    };
    let mut rx = Rx {
        sh: &sh,
        cfg: &cfg,
        me,
        secret,
        public,
        admin_pub,
        roster_version: roster.version,
        rl,
        tun: &tun,
        relays,
        addr_peer: HashMap::new(),
        local: vec![],
        bound_if: None,
        conns: vec![],
        names: names.into_iter().collect(),
        fetch: None,
        updater,
        services_dir,
        last_check: None,
        local_modules: vec![],
        modules_ts: 0,
        local_services: vec![],
        services_ts: 0,
        out: vec![0u8; BUF],
    };

    // Either thread failing is fatal: exit and let launchd / systemd restart us, instead of
    // limping on with a tunnel that only works in one direction.
    std::thread::scope(|s| {
        s.spawn(|| {
            let e = tx_loop(&sh, &tun, s);
            eprintln!("meshd: encrypt thread: {e}");
            std::process::exit(1);
        });
        let e = rx.run();
        eprintln!("meshd: receive thread: {e}");
        std::process::exit(1);
    })
}

/// Assigns the mesh address with a /16 route over the interface and brings it up.
fn configure(ifname: &str, ip: Ipv4Addr) -> io::Result<()> {
    let ip = ip.to_string();
    let mtu = MTU.to_string();
    #[cfg(target_os = "macos")]
    let cmds: Vec<Vec<String>> = {
        let net = format!("{}.0.0/16", ip.split('.').take(2).collect::<Vec<_>>().join("."));
        vec![
            vec!["ifconfig".into(), ifname.into(), "inet".into(), ip.clone(), ip.clone(), "netmask".into(), "255.255.0.0".into(), "mtu".into(), mtu, "up".into()],
            vec!["route".into(), "-q".into(), "-n".into(), "add".into(), "-net".into(), net, "-interface".into(), ifname.into()],
        ]
    };
    #[cfg(target_os = "linux")]
    let cmds: Vec<Vec<String>> = vec![
        vec!["ip".into(), "addr".into(), "add".into(), format!("{ip}/16"), "dev".into(), ifname.into()],
        vec!["ip".into(), "link".into(), "set".into(), "dev".into(), ifname.into(), "mtu".into(), mtu, "up".into()],
    ];
    #[cfg(windows)]
    let cmds: Vec<Vec<String>> = vec![
        vec!["netsh".into(), "interface".into(), "ipv4".into(), "set".into(), "address".into(), format!("name={ifname}"), "source=static".into(), format!("address={ip}"), "mask=255.255.0.0".into()],
        vec!["netsh".into(), "interface".into(), "ipv4".into(), "set".into(), "subinterface".into(), ifname.into(), format!("mtu={mtu}"), "store=active".into()],
    ];
    for c in cmds {
        // A freshly created Windows adapter takes a moment to show up for netsh.
        let mut tries = if cfg!(windows) { 20 } else { 1 };
        loop {
            let st = std::process::Command::new(&c[0]).args(&c[1..]).stdout(std::process::Stdio::null()).status()?;
            if st.success() {
                break;
            }
            tries -= 1;
            if tries == 0 {
                return Err(io::Error::other(format!("{} failed: {st}", c.join(" "))));
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }
    // Windows files a new adapter under the Public profile, whose firewall drops inbound
    // traffic from peers; the mesh is our own private network.
    #[cfg(windows)]
    powershell(&format!("Set-NetConnectionProfile -InterfaceAlias '{ifname}' -NetworkCategory Private"));
    Ok(())
}

#[cfg(windows)]
pub fn powershell(script: &str) -> bool {
    let ok = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .stdout(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !ok {
        eprintln!("meshd: powershell failed: {script}");
    }
    ok
}

/// Sends a WireGuard packet that sits at `buf[HEAD..HEAD + len]` along `path`; the relay
/// header is written into the headroom, so nothing is copied. A direct packet goes through the
/// peer's connected socket when it is for that address, else unconnected through `lan` when
/// `lan` is set (on-link destination while `wan` is pinned).
fn send_wg(sh: &Shared, st: &PeerState, path: Path, peer_id: u16, buf: &mut [u8], len: usize, lan: bool) {
    // Drops on a full socket buffer are fine: WireGuard and the inner TCP recover.
    match path {
        Path::Direct(a) => {
            let pkt = &buf[HEAD..HEAD + len];
            match &st.conn {
                Some(c) if c.peer == a => c.sock.send(pkt).ok(),
                _ if lan => sh.lan.send_to(pkt, a).ok(),
                _ => sh.wan.send_to(pkt, a).ok(),
            };
        }
        Path::Relay(r) => {
            buf[..HEAD].copy_from_slice(&wire::header(wire::FWD, peer_id));
            sh.wan.send_to(&buf[..HEAD + len], r).ok();
        }
        Path::None => {}
    }
}

/// Length of a result that boringtun wrote into `buf[HEAD..]`, moved to start right at
/// `HEAD` if it did not already (it always does today; this keeps the headroom contract
/// true even if that changes). `off` is the result's offset from `buf[HEAD..]`, taken while
/// the borrow was alive via `offset`.
fn settle(buf: &mut [u8], off: usize, len: usize) -> usize {
    if off != 0 {
        buf.copy_within(HEAD + off..HEAD + off + len, HEAD);
    }
    len
}

fn offset(base: usize, b: &[u8]) -> (usize, usize) {
    (b.as_ptr() as usize - base, b.len())
}

/// Where one encrypted packet goes.
enum Out {
    Conn(Arc<Conn>),
    /// Unconnected send to an address, through `lan` when `true`.
    To(SocketAddr, bool),
}

/// Encrypted packets on their way from the encrypt thread to the send thread: packet `i` is
/// `data[start..start + len]` of entry `i`.
struct Batch {
    data: Vec<u8>,
    ent: Vec<(usize, usize, Out)>,
}

/// The send path, split over two cores:
///
/// - encrypt (this thread): TUN -> encrypt -> batch. utun hands out one packet per syscall
///   (`recvmsg_x` on utun also returns one), so this side is inherently per packet.
/// - send (spawned): batch -> UDP. It merges whatever batches are waiting, so the more the
///   encrypt side gets ahead, the cheaper each packet is to send.
///
/// In the single-peer spike, one thread doing both ran at 93% CPU and averaged 1.2 packets
/// per batch, 0.86 Gbit/s on the Thunderbolt link; split, 1.04 Gbit/s.
fn tx_loop<'s, 'e>(sh: &'s Shared, tun: &Tun, scope: &'s std::thread::Scope<'s, 'e>) -> io::Error {
    use std::sync::mpsc::sync_channel;
    let (full_tx, full_rx) = sync_channel::<Box<Batch>>(BATCHES);
    let (free_tx, free_rx) = sync_channel::<Box<Batch>>(BATCHES);
    for _ in 0..BATCHES {
        free_tx.send(Box::new(Batch { data: vec![0u8; BATCH * SLOT], ent: Vec::with_capacity(BATCH) })).unwrap();
    }
    let free_back = free_tx.clone();
    scope.spawn(move || {
        send_loop(sh, full_rx, free_tx);
        eprintln!("meshd: send thread exited");
        std::process::exit(1);
    });

    let mut pkts = vec![0u8; BATCH * SLOT];
    let mut lens = [0usize; BATCH];
    loop {
        lens[0] = match tun.read(&mut pkts[..SLOT]) {
            Ok(n) => n,
            Err(e) => return e,
        };
        let mut cnt = 1;
        while cnt < BATCH {
            match tun.read_nb(&mut pkts[cnt * SLOT..(cnt + 1) * SLOT]) {
                Ok(n) => {
                    lens[cnt] = n;
                    cnt += 1;
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => return e,
            }
        }

        let Ok(mut b) = free_rx.recv() else { return io::Error::other("send thread exited") };
        b.ent.clear();
        // The peer lock is kept across consecutive packets for the same peer.
        let mut held: Option<(usize, std::sync::MutexGuard<PeerState>)> = None;
        for i in 0..cnt {
            let pkt = &pkts[i * SLOT..i * SLOT + lens[i]];
            // IPv4 only for now: the mesh is 10.249.0.0/16.
            if pkt.len() < 20 || pkt[0] >> 4 != 4 {
                continue;
            }
            let dst = Ipv4Addr::new(pkt[16], pkt[17], pkt[18], pkt[19]);
            let Some(&pi) = sh.by_ip.get(&dst) else { continue };
            let peer = &sh.peers[pi];
            let slot = b.ent.len() * SLOT;
            if held.as_ref().is_none_or(|(h, _)| *h != pi) {
                drop(held.take()); // never hold two peer locks at once
                held = Some((pi, peer.st.lock().unwrap()));
            }
            let st = &mut held.as_mut().unwrap().1;
            let base = b.data[slot + HEAD..].as_ptr() as usize;
            let (off, len) = match st.tunn.encapsulate(pkt, &mut b.data[slot + HEAD..slot + SLOT]) {
                TunnResult::WriteToNetwork(r) => offset(base, r),
                _ => continue,
            };
            let (out, relayed) = match st.path {
                Path::Direct(a) => match &st.conn {
                    Some(c) if c.peer == a => (Out::Conn(c.clone()), false),
                    _ => (Out::To(a, st.direct_lan), false),
                },
                Path::Relay(r) => (Out::To(r, false), true),
                Path::None => continue,
            };
            let len = settle(&mut b.data[slot..slot + SLOT], off, len);
            if relayed {
                b.data[slot..slot + HEAD].copy_from_slice(&wire::header(wire::FWD, peer.id));
                b.ent.push((slot, HEAD + len, out));
            } else {
                b.ent.push((slot + HEAD, len, out));
            }
        }
        drop(held);
        if b.ent.is_empty() {
            free_back.send(b).ok();
            continue;
        }
        if full_tx.send(b).is_err() {
            return io::Error::other("send thread exited");
        }
    }
}

fn send_loop(sh: &Shared, full: std::sync::mpsc::Receiver<Box<Batch>>, free: std::sync::mpsc::SyncSender<Box<Batch>>) {
    let mut held: Vec<Box<Batch>> = Vec::with_capacity(BATCHES);
    while let Ok(first) = full.recv() {
        held.push(first);
        let mut total = held[0].ent.len();
        while total < BATCH {
            match full.try_recv() {
                Ok(b) => {
                    total += b.ent.len();
                    held.push(b);
                }
                Err(_) => break,
            }
        }
        {
            // Consecutive packets for the same connected socket go out in one syscall.
            let mut run: Vec<&[u8]> = Vec::with_capacity(BATCH);
            let mut run_conn: Option<&Arc<Conn>> = None;
            let flush = |run: &mut Vec<&[u8]>, c: Option<&Arc<Conn>>| {
                if let Some(c) = c {
                    // Partial sends mean a full socket buffer; the tail is recovered like any loss.
                    batch::send_batch(&c.sock, run).ok();
                }
                run.clear();
            };
            for b in &held {
                for (start, len, out) in &b.ent {
                    let pkt = &b.data[*start..*start + *len];
                    match out {
                        Out::Conn(c) => {
                            if run_conn.is_some_and(|r| !Arc::ptr_eq(r, c)) || run.len() == BATCH {
                                flush(&mut run, run_conn);
                            }
                            run_conn = Some(c);
                            run.push(pkt);
                        }
                        Out::To(a, lan) => {
                            flush(&mut run, run_conn);
                            run_conn = None;
                            let s = if *lan { &sh.lan } else { &sh.wan };
                            s.send_to(pkt, a).ok();
                        }
                    }
                }
            }
            flush(&mut run, run_conn);
        }
        for mut b in held.drain(..) {
            b.ent.clear();
            if free.send(b).is_err() {
                return;
            }
        }
    }
}

#[derive(Clone, Copy)]
enum Via {
    Direct(SocketAddr),
    Relay(usize),
}

impl<'a> Rx<'a> {
    fn run(&mut self) -> io::Error {
        let sh = self.sh;
        let mut buf = vec![0u8; BUF];
        let mut ready = vec![];
        let start = Instant::now();
        let (mut next_tick, mut next_reset, mut next_local, mut next_status) = (start, start, start, start);
        let mut next_vpn = start;
        loop {
            let conns = self.conns.clone();
            let mut socks: Vec<&UdpSocket> = vec![&sh.wan];
            if !Arc::ptr_eq(&sh.wan, &sh.lan) {
                socks.push(&sh.lan);
            }
            let shared = socks.len();
            let dns_at = sh.dns.as_ref().map(|d| {
                socks.push(d);
                socks.len() - 1
            });
            socks.extend(conns.iter().map(|c| &c.sock));
            let wait_ms = next_tick.saturating_duration_since(Instant::now()).as_millis() as i32;
            if let Err(e) = udp::wait(&socks, wait_ms, &mut ready) {
                return e;
            }
            for &i in &ready {
                if Some(i) == dns_at {
                    while let Ok((n, src)) = udp::recv_now(socks[i], &mut buf) {
                        if let Some(r) = dns::answer(&buf[..n], |name| self.names.get(name).copied()) {
                            socks[i].send_to(&r, src).ok();
                        }
                    }
                    continue;
                }
                for _ in 0..RX_DRAIN {
                    match udp::recv_now(socks[i], &mut buf) {
                        Ok((n, src)) => self.packet(&mut buf[..n], src),
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                        // ICMP port unreachable from a peer or relay surfaces as an error
                        // (ConnectionReset on Windows).
                        Err(e) if matches!(e.kind(), io::ErrorKind::ConnectionRefused | io::ErrorKind::ConnectionReset) => {}
                        Err(e) if i < shared => return e,
                        Err(_) => break, // a peer socket; it is replaced on the next path change
                    }
                }
            }
            let now = Instant::now();
            if now >= next_reset {
                next_reset = now + Duration::from_secs(1);
                self.rl.reset_count();
            }
            if now >= next_local {
                next_local = now + LOCAL_ADDRS_EVERY;
                let check_vpn = now >= next_vpn;
                if check_vpn {
                    next_vpn = now + VPN_CHECK_EVERY;
                }
                self.refresh_local(check_vpn);
            }
            if now >= next_tick {
                next_tick = now + TICK;
                self.timers(now);
                self.sync_conns();
            }
            if now >= next_status {
                next_status = now + STATUS_EVERY;
                self.services_tick(now);
                self.write_status(now);
            }
        }
    }

    /// `dst` is reachable without a gateway on a physical interface.
    fn on_link(&self, dst: SocketAddr) -> bool {
        let SocketAddr::V4(d) = dst else { return false };
        self.local.iter().any(|a| !netif::is_tunnel(&a.ifname) && a.on_link(*d.ip()))
    }

    /// Unconnected sends to `dst` must use `lan`: `wan` is pinned and `dst` is on-link elsewhere.
    fn needs_lan(&self, dst: SocketAddr) -> bool {
        self.bound_if.is_some() && self.on_link(dst)
    }

    fn sock_for(&self, dst: SocketAddr) -> &UdpSocket {
        if self.needs_lan(dst) {
            &self.sh.lan
        } else {
            &self.sh.wan
        }
    }

    fn path_lan(&self, path: Path) -> bool {
        matches!(path, Path::Direct(a) if self.needs_lan(a))
    }

    /// Opens, replaces or closes each peer's connected socket to match its direct path.
    /// Packets that reached a new socket between bind and connect (when it was still a
    /// wildcard socket) are drained and handled like any other.
    fn sync_conns(&mut self) {
        if cfg!(windows) {
            return; // no per-peer connected sockets without SO_REUSEPORT
        }
        let mut opened = vec![];
        for pi in 0..self.sh.peers.len() {
            let peer = &self.sh.peers[pi];
            let (want, have) = {
                let st = peer.st.lock().unwrap();
                let want = st.direct.filter(|_| !self.cfg.force_relay);
                (want, st.conn.as_ref().map(|c| c.peer))
            };
            if want == have {
                continue;
            }
            let conn = match want {
                Some(SocketAddr::V4(d)) => {
                    let pin = if self.on_link(SocketAddr::V4(d)) { None } else { self.bound_if.as_deref() };
                    match udp::open(self.cfg.listen, Some(d), pin) {
                        Ok(sock) => Some(Arc::new(Conn { sock, peer: SocketAddr::V4(d) })),
                        Err(e) => {
                            eprintln!("meshd: {} socket to {d}: {e}", peer.name);
                            None
                        }
                    }
                }
                _ => None,
            };
            let mut st = peer.st.lock().unwrap();
            if st.direct.filter(|_| !self.cfg.force_relay) == want {
                st.conn = conn.clone();
                opened.extend(conn);
            }
        }
        self.conns = self
            .sh
            .peers
            .iter()
            .filter_map(|p| p.st.lock().unwrap().conn.clone())
            .collect();
        let mut buf = vec![0u8; BUF];
        for c in opened {
            while let Ok((n, src)) = udp::recv_now(&c.sock, &mut buf) {
                self.packet(&mut buf[..n], src);
            }
        }
    }

    /// Re-reads local addresses (and, with `check_vpn`, re-pins `wan` if a VPN came or went).
    ///
    /// When the set of local addresses changes (Wi-Fi on / off, new network), every peer's
    /// connected socket is dropped and every direct path forgotten, then re-probed at once:
    /// macOS fixes a connected socket's source address at connect time, so after a network
    /// change it keeps sending from an address that no longer exists (2026-10-17: Wi-Fi was
    /// switched on and off on laptop1, and its path to a Windows peer stayed dead until restart).
    fn refresh_local(&mut self, check_vpn: bool) {
        let mut before: Vec<Ipv4Addr> = self.local.iter().map(|a| a.ip).collect();
        self.local = netif::local_ipv4(self.tun.name());
        let mut after: Vec<Ipv4Addr> = self.local.iter().map(|a| a.ip).collect();
        before.sort();
        after.sort();
        if !before.is_empty() && before != after {
            eprintln!("meshd: local addresses {before:?} -> {after:?}, re-probing every peer");
            let now = Instant::now();
            for peer in &self.sh.peers {
                let mut st = peer.st.lock().unwrap();
                st.conn = None;
                if let Some(d) = st.direct.take() {
                    self.addr_peer.remove(&d);
                }
                st.direct_rtt = None;
                st.last_pong = None;
                st.next_probe = now;
                st.next_full_probe = now;
            }
        }
        let bound = if !check_vpn {
            self.bound_if.clone()
        } else if self.cfg.no_vpn_bypass {
            None
        } else {
            netif::vpn_bypass_interface()
        };
        if bound != self.bound_if {
            match netif::bind_interface(&self.sh.wan, bound.as_deref()) {
                Ok(()) => {
                    match &bound {
                        Some(i) => eprintln!("meshd: default route is a VPN, wan socket pinned to {i}"),
                        None => eprintln!("meshd: wan socket follows the default route"),
                    }
                    self.bound_if = bound;
                    // Peer sockets were pinned (or not) for the old situation: reopen them.
                    for peer in &self.sh.peers {
                        peer.st.lock().unwrap().conn = None;
                    }
                }
                Err(e) => eprintln!("meshd: pinning socket to {bound:?}: {e}"),
            }
        }
        for peer in &self.sh.peers {
            let mut st = peer.st.lock().unwrap();
            self.refresh_path(&mut st);
        }
    }

    fn relay_index(&self, src: SocketAddr) -> Option<usize> {
        self.relays.iter().position(|r| r.addr == src)
    }

    fn packet(&mut self, p: &mut [u8], src: SocketAddr) {
        if wire::is_wireguard(p) {
            self.wg_direct(p, src);
            return;
        }
        let Some((kind, field)) = wire::control(p) else { return };
        match kind {
            wire::DISCO => self.disco(p, Via::Direct(src)),
            _ => {
                let Some(ri) = self.relay_index(src) else { return };
                match kind {
                    wire::HELLO_ACK => self.hello_ack(ri, p),
                    wire::FWD_FROM => {
                        let Some(&pi) = self.sh.by_id.get(&field) else { return };
                        let payload = &mut p[wire::HDR..];
                        if wire::is_wireguard(payload) {
                            self.wg(pi, payload, Via::Relay(ri));
                        } else if wire::control(payload).is_some_and(|(k, id)| k == wire::DISCO && id == field) {
                            self.disco(payload, Via::Relay(ri));
                        }
                    }
                    wire::NO_ROUTE => self.no_route(ri, field, p),
                    wire::ROSTER_PART => self.roster_part(ri, p),
                    wire::BLOB_PART => self.updater.part(&self.sh.wan, self.relays[ri].addr, p),
                    _ => {}
                }
            }
        }
    }

    /// A WireGuard packet straight from the network: find the peer it belongs to.
    fn wg_direct(&mut self, p: &mut [u8], src: SocketAddr) {
        if let Some(&pi) = self.addr_peer.get(&src) {
            self.wg(pi, p, Via::Direct(src));
            return;
        }
        // Unknown address: check mac1 (cheap) and let the shared limiter answer with a
        // cookie under load, before spending a DH on the anonymous handshake parse.
        let reply_sock = if self.needs_lan(src) { &self.sh.lan } else { &self.sh.wan };
        let pi = match self.rl.verify_packet(Some(src.ip()), p, &mut self.out) {
            Ok(Packet::HandshakeInit(init)) => parse_handshake_anon(&self.secret, &self.public, &init)
                .ok()
                .and_then(|h| self.sh.peers.iter().position(|peer| peer.key == h.peer_static_public)),
            Ok(Packet::HandshakeResponse(r)) => self.sh.by_id.get(&((r.receiver_idx >> 8) as u16)).copied(),
            Ok(Packet::PacketCookieReply(c)) => self.sh.by_id.get(&((c.receiver_idx >> 8) as u16)).copied(),
            Ok(Packet::PacketData(d)) => self.sh.by_id.get(&((d.receiver_idx >> 8) as u16)).copied(),
            Err(TunnResult::WriteToNetwork(cookie)) => {
                reply_sock.send_to(cookie, src).ok();
                None
            }
            Err(_) => None,
        };
        if let Some(pi) = pi {
            self.wg(pi, p, Via::Direct(src));
        }
    }

    fn wg(&mut self, pi: usize, p: &[u8], via: Via) {
        if self.cfg.force_relay && matches!(via, Via::Direct(_)) {
            return;
        }
        let ptype = p[0];
        let peer = &self.sh.peers[pi];
        let mut st = peer.st.lock().unwrap();
        let src_ip = match via {
            Via::Direct(a) => a.ip(),
            Via::Relay(ri) => self.relays[ri].addr.ip(),
        };
        let mut authenticated = false;
        let mut reply = None;
        // A decrypted packet for the TUN: (offset, length, is IPv6). Written after the peer
        // lock is released, so the encrypt thread is not held up by the syscall.
        let mut to_tun = None;
        let base = self.out[HEAD..].as_ptr() as usize;
        match st.tunn.decapsulate(Some(src_ip), p, &mut self.out[HEAD..]) {
            // Cryptokey routing (WireGuard's AllowedIPs): a peer may only send from its own mesh
            // address, or any node holding a valid key could pose as another one. The mesh is
            // IPv4 only, so no IPv6 source is ever a peer's.
            TunnResult::WriteToTunnelV4(b, from) => {
                authenticated = true;
                if from == peer.ip {
                    let (off, len) = offset(base, b);
                    to_tun = Some((off, len, false));
                }
            }
            TunnResult::WriteToTunnelV6(..) => authenticated = true,
            TunnResult::WriteToNetwork(b) => {
                // A handshake response from us, or a cookie: goes back the way it came.
                // A handshake response we received (type 2) completes our handshake.
                authenticated = ptype == 2;
                reply = Some(offset(base, b));
            }
            // Keepalive (type 4) or a handshake response that completed (type 2).
            TunnResult::Done => authenticated = ptype == 4 || ptype == 2,
            TunnResult::Err(_) => {}
        }
        if let Some((off, len)) = reply {
            let len = settle(&mut self.out, off, len);
            let back = match via {
                Via::Direct(a) => Path::Direct(a),
                Via::Relay(ri) => Path::Relay(self.relays[ri].addr),
            };
            let lan = self.path_lan(back);
            send_wg(self.sh, &st, back, peer.id, &mut self.out, len, lan);
        }
        if authenticated {
            let stale = st.last_pong.is_none_or(|t| t.elapsed() > DIRECT_STALE);
            match via {
                // Follow the peer to a new address only when we have no working direct path:
                // if both sides follow each other while preferring different paths, they flap.
                Via::Direct(a) if st.direct != Some(a) && (st.direct.is_none() || stale) => {
                    eprintln!("meshd: {} direct at {a} (authenticated WireGuard)", peer.name);
                    if let Some(old) = st.direct.replace(a) {
                        self.addr_peer.remove(&old);
                    }
                    self.addr_peer.insert(a, pi);
                    st.last_pong = Some(Instant::now());
                    st.direct_rtt = None;
                }
                Via::Relay(ri) => st.relay = Some(ri),
                _ => {}
            }
            self.refresh_path(&mut st);
        }
        if let Some((off, len, v6)) = to_tun {
            drop(st);
            let pkt = &self.out[HEAD + off..HEAD + off + len];
            if self.cfg.inbound.as_ref().is_some_and(|f| !f.allows(pkt)) {
                return;
            }
            if let Err(e) = self.tun.write(pkt, v6) {
                eprintln!("meshd: tun write: {e}");
            }
            return;
        }
        // A completed handshake may release packets queued while it was in progress (a data
        // packet never does, hence only here).
        while let TunnResult::WriteToNetwork(b) = st.tunn.decapsulate(None, &[], &mut self.out[HEAD..]) {
            let (off, len) = offset(base, b);
            let len = settle(&mut self.out, off, len);
            let lan = self.path_lan(st.path);
            send_wg(self.sh, &st, st.path, peer.id, &mut self.out, len, lan);
        }
    }

    /// Picks the path the send thread uses for this peer.
    fn refresh_path(&self, st: &mut PeerState) {
        let now = Instant::now();
        st.direct_lan = st.direct.is_some_and(|d| self.needs_lan(d));
        let relay = self.pick_relay(st, now);
        // A "direct" path can be worse than a relay (e.g. laptop1 -> a VPN exit host hairpins through the
        // VPN tunnel that exits at that host, seconds of RTT): prefer the relay when the direct
        // RTT is over 1.5x the relay path's (my RTT + the peer's RTT to that relay).
        let relay_ms = relay.and_then(|ri| {
            let mine = self.relays[ri].rtt.filter(|_| self.relays[ri].alive(now))?.as_millis() as u64;
            Some(mine + st.peer_relay_rtt.get(ri).copied().flatten()? as u64)
        });
        let direct_slow = matches!((st.direct_rtt, relay_ms), (Some(d), Some(r)) if d.as_millis() as u64 > r * 3 / 2 + 10);
        st.path = match st.direct {
            Some(d) if !self.cfg.force_relay && !direct_slow => Path::Direct(d),
            _ => match relay {
                Some(ri) => Path::Relay(self.relays[ri].addr),
                None => Path::None,
            },
        };
    }

    /// The relay with the lowest sum of my RTT and the peer's RTT (both alive and measured);
    /// without the peer's numbers, its last-used relay while alive, else my lowest-RTT relay,
    /// else (nothing answered yet) the first relay.
    fn pick_relay(&self, st: &PeerState, now: Instant) -> Option<usize> {
        let mine = |i: usize| self.relays[i].rtt.filter(|_| self.relays[i].alive(now)).map(|d| d.as_millis() as u32);
        // (relay, my leg, peer's leg) for every relay both of us reach.
        let legs: Vec<(usize, u32, u32)> = (0..self.relays.len())
            .filter_map(|i| Some((i, mine(i)?, st.peer_relay_rtt.get(i).copied().flatten()?)))
            .collect();
        if let Some(best) = legs.iter().map(|&(_, a, b)| a + b).min() {
            // Among relays within 15% of the best total, prefer one next to either end: one
            // long-haul leg instead of two (e.g. Shanghai -> Hong Kong across the border is as
            // fast as going to Hong Kong directly, but has far less bandwidth; measured
            // 2026-10-16, ~70 ms either way).
            return legs
                .iter()
                .filter(|&&(_, a, b)| (a + b) * 100 <= best * 115 + 100)
                .min_by_key(|&&(_, a, b)| a.min(b))
                .map(|&(i, _, _)| i);
        }
        if let Some(i) = st.relay.filter(|&i| self.relays[i].alive(now)) {
            return Some(i);
        }
        let best = (0..self.relays.len()).filter_map(|i| Some((i, mine(i)?))).min_by_key(|&(_, r)| r);
        best.map(|(i, _)| i).or(if self.relays.is_empty() { None } else { Some(0) })
    }

    /// Acts only on a NO_ROUTE sealed by the relay and echoing a HELLO it accepted since our
    /// last ACK: a spoofed or replayed one would otherwise push peers off a working relay.
    /// Relays before v9 send it unsealed; it is ignored and the path falls back on timeouts.
    fn no_route(&mut self, ri: usize, id: u16, p: &mut [u8]) {
        let r = &self.relays[ri];
        let Some(ts) = wire::open(&r.key, p).and_then(|plain| wire::get_u64(plain)) else { return };
        if !fresh_no_route(ts, r.acked_ts, r.hello_ts) {
            return;
        }
        let Some(&pi) = self.sh.by_id.get(&id) else { return };
        let mut st = self.sh.peers[pi].st.lock().unwrap();
        // Try the next relay that is alive; the peer may only be registered there.
        let now = Instant::now();
        let n = self.relays.len();
        st.relay = (1..n).map(|k| (ri + k) % n).find(|&i| self.relays[i].alive(now));
        if let Some(r) = st.peer_relay_rtt.get_mut(ri) {
            *r = None;
        }
        st.next_probe = now + NO_ROUTE_BACKOFF;
        self.refresh_path(&mut st);
    }

    fn hello_ack(&mut self, ri: usize, p: &mut [u8]) {
        let r = &mut self.relays[ri];
        let Some(ack) = wire::open(&r.key, p).and_then(|plain| wire::parse_hello_ack(plain)) else { return };
        let wire::HelloAck { ts, public, roster_version: version, release } = ack;
        if ts != r.hello_ts {
            return;
        }
        r.acked_ts = ts;
        let now = Instant::now();
        let was_alive = r.alive(now);
        r.rtt = Some(now.duration_since(r.hello_at));
        r.last_ack = Some(now);
        if r.public != Some(public) {
            eprintln!("meshd: relay {} sees us at {public}", r.name);
            r.public = Some(public);
        }
        r.version = version;
        r.next_hello = now + HELLO_EVERY;
        // Reaching a relay is the health check for a freshly installed release.
        let (relay_addr, relay_name) = (r.addr, r.name.clone());
        self.updater.healthy();
        if let Some((rv, hash)) = release {
            self.updater.announced(&self.sh.wan, relay_addr, &relay_name, rv, hash);
        }
        let r = &mut self.relays[ri];
        if !was_alive {
            eprintln!("meshd: relay {} up, rtt {:?}", r.name, r.rtt.unwrap());
            for peer in &self.sh.peers {
                let mut st = peer.st.lock().unwrap();
                self.refresh_path(&mut st);
            }
        }
        if version > self.roster_version && self.fetch.is_none() {
            eprintln!("meshd: relay {} has roster v{version}, fetching", self.relays[ri].name);
            self.fetch = Some(Fetch { relay: ri, data: vec![], asked: now });
            self.ask_roster(ri, 0);
        }
    }

    fn ask_roster(&self, ri: usize, off: u32) {
        let mut m = wire::header(wire::ROSTER_GET, 0).to_vec();
        m.extend_from_slice(&off.to_be_bytes());
        self.sh.wan.send_to(&m, self.relays[ri].addr).ok();
    }

    fn roster_part(&mut self, ri: usize, p: &[u8]) {
        let Some(f) = self.fetch.as_mut().filter(|f| f.relay == ri) else { return };
        let b = &p[wire::HDR..];
        let (Some(total), Some(off)) = (wire::get_u32(b), b.get(4..).and_then(wire::get_u32)) else { return };
        if off as usize != f.data.len() || total > 1 << 20 {
            return;
        }
        f.data.extend_from_slice(&b[8..]);
        f.asked = Instant::now();
        if f.data.len() < total as usize {
            let next = f.data.len() as u32;
            self.ask_roster(ri, next);
            return;
        }
        let data = self.fetch.take().unwrap().data;
        match Roster::parse_verified(&data, &self.admin_pub) {
            Ok(r) if r.version > self.roster_version => {
                let path = self.cfg.dir.join("roster");
                let tmp = self.cfg.dir.join("roster.tmp");
                if let Err(e) = std::fs::write(&tmp, &data).and_then(|_| std::fs::rename(&tmp, &path)) {
                    eprintln!("meshd: saving roster: {e}");
                    return;
                }
                // Peers, keys and addresses may all have changed: start over from the new
                // roster (launchd / systemd restarts us; a second or so of disruption).
                eprintln!("meshd: roster v{} -> v{}, restarting", self.roster_version, r.version);
                std::process::exit(3);
            }
            Ok(_) => {}
            Err(e) => eprintln!("meshd: fetched roster rejected: {e}"),
        }
    }

    /// My candidate addresses: what each relay sees, plus local interface addresses.
    fn my_candidates(&self) -> Vec<SocketAddrV4> {
        let mut c: Vec<SocketAddrV4> = self.relays.iter().filter_map(|r| r.public).collect();
        c.extend(
            self.local
                .iter()
                .filter(|a| !netif::is_tunnel(&a.ifname))
                .map(|a| SocketAddrV4::new(a.ip, self.cfg.listen)),
        );
        c.sort();
        c.dedup();
        c.truncate(MAX_CANDIDATES);
        c
    }

    /// Sends a sealed DISCO message to a peer, directly or through a relay.
    fn send_disco(&self, pi: usize, to: Path, body: &[u8]) {
        let peer = &self.sh.peers[pi];
        let msg = wire::sealed(wire::DISCO, self.me.id, &peer.disco, body);
        match to {
            Path::Direct(a) => {
                self.sock_for(a).send_to(&msg, a).ok();
            }
            Path::Relay(r) => {
                let mut m = wire::header(wire::FWD, peer.id).to_vec();
                m.extend_from_slice(&msg);
                self.sh.wan.send_to(&m, r).ok();
            }
            Path::None => {}
        }
    }

    /// CALL through a relay: my candidate addresses, then my RTT to each live relay.
    fn send_call(&self, pi: usize, st: &PeerState, cands: &[SocketAddrV4], now: Instant) {
        let mut call = vec![wire::CALL, cands.len() as u8];
        for &c in cands {
            wire::put_addr(&mut call, c);
        }
        let rtts: Vec<(u8, u16)> = (0..self.relays.len())
            .filter(|&i| self.relays[i].alive(now))
            .filter_map(|i| Some((i as u8, self.relays[i].rtt?.as_millis().min(u16::MAX as u128) as u16)))
            .collect();
        call.push(rtts.len() as u8);
        for (i, ms) in rtts {
            call.push(i);
            call.extend_from_slice(&ms.to_be_bytes());
        }
        if let Some(ri) = self.pick_relay(st, now) {
            self.send_disco(pi, Path::Relay(self.relays[ri].addr), &call);
        }
    }

    fn send_ping(&self, pi: usize, st: &mut PeerState, to: SocketAddr) {
        let txid = meshproto::random_u64();
        st.pings.push((txid, to, Instant::now()));
        let mut body = vec![wire::PING];
        body.extend_from_slice(&txid.to_be_bytes());
        self.send_disco(pi, Path::Direct(to), &body);
    }

    fn disco(&mut self, p: &mut [u8], via: Via) {
        let Some((_, id)) = wire::control(p) else { return };
        let Some(&pi) = self.sh.by_id.get(&id) else { return };
        let peer = &self.sh.peers[pi];
        let Some(plain) = wire::open(&peer.disco, p) else { return };
        let Some((&kind, body)) = plain.split_first() else { return };
        let mut st = peer.st.lock().unwrap();
        if let Via::Relay(ri) = via {
            st.relay = Some(ri);
        }
        // Either path, and also with --force-relay: announcements are not about paths.
        if kind == wire::SERVICES {
            if let Some((ts, ttl, list)) = service::decode(body) {
                if accept_services(st.services_ts, ts, meshproto::now_nanos()) {
                    st.services_ts = ts;
                    st.services = list;
                    st.services_until = Some(Instant::now() + services_ttl(ttl));
                }
            }
            return;
        }
        if kind == wire::MODULES {
            if let Some((ts, ttl, list)) = service::decode_modules(body) {
                if accept_services(st.modules_ts, ts, meshproto::now_nanos()) {
                    st.modules_ts = ts;
                    st.modules = list;
                    st.modules_until = Some(Instant::now() + services_ttl(ttl));
                }
            }
            return;
        }
        if self.cfg.force_relay && kind != wire::CALL {
            self.refresh_path(&mut st);
            return;
        }
        match (kind, via) {
            (wire::CALL, _) => {
                let n = (*body.first().unwrap_or(&0) as usize).min(MAX_CANDIDATES);
                st.candidates = (0..n).filter_map(|k| body.get(1 + k * 6..).and_then(wire::get_addr)).collect();
                // Then the sender's relay RTTs: count, then (relay index u8, ms u16) each.
                let rest = body.get(1 + n * 6..).unwrap_or(&[]);
                let m = *rest.first().unwrap_or(&0) as usize;
                st.peer_relay_rtt.iter_mut().for_each(|r| *r = None);
                for e in rest.get(1..).unwrap_or(&[]).chunks_exact(3).take(m) {
                    if let Some(r) = st.peer_relay_rtt.get_mut(e[0] as usize) {
                        *r = Some(u16::from_be_bytes([e[1], e[2]]) as u32);
                    }
                }
                self.refresh_path(&mut st);
                if st.direct.is_none() && !self.cfg.force_relay {
                    for c in st.candidates.clone() {
                        self.send_ping(pi, &mut st, SocketAddr::V4(c));
                    }
                }
            }
            (wire::PING, Via::Direct(src)) => {
                let Some(txid) = wire::get_u64(body) else { return };
                let mut pong = vec![wire::PONG];
                pong.extend_from_slice(&txid.to_be_bytes());
                if let SocketAddr::V4(s) = src {
                    wire::put_addr(&mut pong, s);
                }
                self.send_disco(pi, Path::Direct(src), &pong);
                // They reached us from `src`: it is worth probing back.
                if st.direct.is_none() && !st.pings.iter().any(|&(_, a, _)| a == src) {
                    self.send_ping(pi, &mut st, src);
                }
            }
            (wire::PONG, Via::Direct(src)) => {
                let Some(txid) = wire::get_u64(body) else { return };
                let Some(k) = st.pings.iter().position(|&(t, a, _)| t == txid && a == src) else { return };
                let (_, _, sent) = st.pings.swap_remove(k);
                let rtt = sent.elapsed();
                if st.direct == Some(src) {
                    st.last_pong = Some(Instant::now());
                    st.direct_rtt = Some(rtt);
                    return;
                }
                // Pongs arrive in RTT order during a probe round, so the first one wins and a
                // later one must be clearly (25%) faster, or the current path must be failing.
                let stale = st.last_pong.is_none_or(|t| t.elapsed() > DIRECT_STALE);
                let better = st.direct_rtt.is_none_or(|cur| rtt * 5 / 4 < cur);
                if st.direct.is_none() || stale || better {
                    eprintln!("meshd: {} direct at {src}, rtt {rtt:?}", peer.name);
                    if let Some(old) = st.direct.replace(src) {
                        self.addr_peer.remove(&old);
                    }
                    self.addr_peer.insert(src, pi);
                    st.last_pong = Some(Instant::now());
                    st.direct_rtt = Some(rtt);
                    self.refresh_path(&mut st);
                }
            }
            _ => {}
        }
    }

    fn timers(&mut self, now: Instant) {
        // Relays: HELLO, retried quickly until one answers.
        for ri in 0..self.relays.len() {
            let r = &mut self.relays[ri];
            if now < r.next_hello {
                continue;
            }
            let ts = meshproto::now_nanos().max(r.hello_ts + 1);
            r.hello_ts = ts;
            r.hello_at = now;
            r.next_hello = now + if r.alive(now) { HELLO_EVERY } else { HELLO_RETRY };
            let mut plain = ts.to_be_bytes().to_vec();
            plain.extend_from_slice(&self.roster_version.to_be_bytes());
            let msg = wire::sealed(wire::HELLO, self.me.id, &r.key, &plain);
            self.sh.wan.send_to(&msg, r.addr).ok();
        }
        self.updater.tick(&self.sh.wan);
        if let Some(f) = &self.fetch {
            if now.duration_since(f.asked) > Duration::from_secs(5) {
                eprintln!("meshd: roster fetch from {} timed out", self.relays[f.relay].name);
                self.fetch = None;
            }
        }

        let cands = self.my_candidates();
        for pi in 0..self.sh.peers.len() {
            let peer = &self.sh.peers[pi];
            let mut st = peer.st.lock().unwrap();
            self.refresh_path(&mut st);

            // WireGuard timers: handshakes, keepalives.
            let base = self.out[HEAD..].as_ptr() as usize;
            let res = match st.tunn.update_timers(&mut self.out[HEAD..]) {
                TunnResult::WriteToNetwork(b) => Some(offset(base, b)),
                _ => None,
            };
            if let Some((off, len)) = res {
                let len = settle(&mut self.out, off, len);
                let lan = self.path_lan(st.path);
                send_wg(self.sh, &st, st.path, peer.id, &mut self.out, len, lan);
            }

            st.pings.retain(|&(_, _, t)| now.duration_since(t) < PING_TIMEOUT);
            if self.cfg.force_relay {
                if now >= st.next_probe {
                    st.next_probe = now + FULL_PROBE_EVERY / 2;
                    self.send_call(pi, &st, &[], now);
                }
                continue;
            }
            if let Some(d) = st.direct {
                if st.last_pong.is_some_and(|t| now.duration_since(t) > DIRECT_DEAD) {
                    eprintln!("meshd: {} direct path {d} lost, back to relay", peer.name);
                    st.direct = None;
                    st.direct_rtt = None;
                    self.addr_peer.remove(&d);
                    self.refresh_path(&mut st);
                } else if now >= st.next_probe {
                    st.next_probe = now + DIRECT_PING_EVERY;
                    self.send_ping(pi, &mut st, d);
                }
                if now < st.next_full_probe {
                    continue;
                }
            } else if now < st.next_probe {
                continue;
            } else {
                st.next_probe = now + PROBE_EVERY;
            }
            st.next_full_probe = now + FULL_PROBE_EVERY;
            // Tell the peer where we are (through a relay), and try where it said it was.
            self.send_call(pi, &st, &cands, now);
            for c in st.candidates.clone() {
                if st.direct != Some(SocketAddr::V4(c)) {
                    self.send_ping(pi, &mut st, SocketAddr::V4(c));
                }
            }
        }
    }

    /// Reads the local registrations, sends them to online peers when due, and drops expired
    /// announcements from peers.
    fn services_tick(&mut self, now: Instant) {
        let scanned = match &self.services_dir {
            Some(d) => crate::services::scan(d, SystemTime::now()),
            None => vec![],
        };
        let requests = self.services_dir.as_deref().map(crate::services::take_requests).unwrap_or_default();
        self.updater.modules.agent_present = scanned.iter().any(|l| l.svc.name == "agent-inbox");
        // One-click update of the desktop or CLI module: install the verified cached files.
        for r in &requests {
            if let Some(group) = r.strip_prefix("install-") {
                if group != "agent" {
                    self.updater.install_group(group);
                }
            }
        }
        // "Check for updates": ask every relay now instead of at the next HELLO. Rate limited,
        // since any local user can ask.
        if requests.iter().any(|r| r == "check") && self.last_check.is_none_or(|t| now.duration_since(t) >= CHECK_MIN_GAP) {
            self.last_check = Some(now);
            for r in &mut self.relays {
                r.next_hello = now;
            }
        }
        let changed = scanned.iter().map(|l| &l.svc).ne(self.local_services.iter().map(|l| &l.svc));
        self.local_services = scanned;
        let modules = self.updater.modules.announce();
        let modules_changed = modules != self.local_modules;
        self.local_modules = modules;
        let mut modules_body = None;
        let list: Vec<Service> = self.local_services.iter().map(|l| l.svc.clone()).collect();
        let mut body = None;
        let sh = self.sh;
        for (pi, peer) in sh.peers.iter().enumerate() {
            let mut st = peer.st.lock().unwrap();
            let online = st.tunn.stats().0.is_some_and(|d| d < ONLINE);
            if !online || st.services_until.is_some_and(|t| now >= t) {
                st.services.clear();
                st.services_until = None;
            }
            if !online || st.modules_until.is_some_and(|t| now >= t) {
                st.modules.clear();
                st.modules_until = None;
            }
            if !online {
                st.services_sent = None; // announce again as soon as it is back
                st.modules_sent = None;
                continue;
            }
            let path = st.path;
            if path != Path::None && (modules_changed || st.modules_sent.is_none_or(|t| now.duration_since(t) >= ANNOUNCE_EVERY)) {
                if modules_body.is_none() {
                    let ts = meshproto::now_nanos().max(self.modules_ts + 1);
                    self.modules_ts = ts;
                    let mut b = vec![wire::MODULES];
                    b.extend(service::encode_modules(ts, ANNOUNCE_TTL, &self.local_modules).expect("module names are fixed"));
                    modules_body = Some(b);
                }
                self.send_disco(pi, path, modules_body.as_deref().expect("built above"));
                st.modules_sent = Some(now);
            }
            if path == Path::None || !announce_due(&list, changed, st.services_sent_some, st.services_sent, now) {
                continue;
            }
            if body.is_none() {
                let ts = meshproto::now_nanos().max(self.services_ts + 1);
                self.services_ts = ts;
                let mut b = vec![wire::SERVICES];
                b.extend(service::encode(ts, ANNOUNCE_TTL, &list).expect("registrations are validated"));
                body = Some(b);
            }
            self.send_disco(pi, path, body.as_deref().expect("built above"));
            st.services_sent = Some(now);
            st.services_sent_some = !list.is_empty();
        }
    }

    /// Human-readable state for `meshd status` (and later the tray icon).
    fn write_status(&self, now: Instant) {
        let mut s = String::new();
        let _ = writeln!(
            s,
            "node {} {} id {} roster v{} {} key {}",
            self.me.name,
            self.me.ip,
            self.me.id,
            self.roster_version,
            self.updater.status(),
            meshproto::b64(&self.me.key)
        );
        let _ = writeln!(s, "{}", self.updater.status_line());
        for line in self.updater.module_lines() {
            let _ = writeln!(s, "{line}");
        }
        for r in &self.relays {
            let state = if r.alive(now) { "up" } else { "down" };
            let rtt = r.rtt.map(|d| format!("{}ms", d.as_millis())).unwrap_or("-".into());
            let public = r.public.map(|a| a.to_string()).unwrap_or("-".into());
            let _ = writeln!(s, "relay {} {} {state} rtt {rtt} sees-us {public}", r.name, r.addr);
        }
        let mut svc_lines = String::new();
        for l in &self.local_services {
            let expires = crate::services::STALE.saturating_sub(l.age).as_secs();
            let uid = l.uid.map(|u| u.to_string()).unwrap_or("-".into());
            let ident = if l.svc.ident.is_empty() { "-" } else { &l.svc.ident };
            let (me, svc) = (&self.me, &l.svc);
            let _ = writeln!(
                svc_lines,
                "service {} {} id {} name {} port {} ident {ident} expires {expires}s uid {uid}",
                me.name, me.ip, me.id, svc.name, svc.port
            );
        }
        let modules_line = |name: &str, ip: &Ipv4Addr, id: u16, list: &[ModuleInfo]| {
            let words: Vec<String> = list.iter().map(|m| format!("{}={}", m.name, if m.version.is_empty() { "?" } else { &m.version })).collect();
            format!("modules {name} {ip} id {id} {}", words.join(" "))
        };
        let _ = writeln!(svc_lines, "{}", modules_line(&self.me.name, &self.me.ip, self.me.id, &self.local_modules));
        for peer in &self.sh.peers {
            let st = peer.st.lock().unwrap();
            if st.modules_until.is_some() && !st.modules.is_empty() {
                let _ = writeln!(svc_lines, "{}", modules_line(&peer.name, &peer.ip, peer.id, &st.modules));
            }
            if let Some(until) = st.services_until {
                for svc in &st.services {
                    let expires = until.saturating_duration_since(now).as_secs();
                    let ident = if svc.ident.is_empty() { "-" } else { &svc.ident };
                    let _ = writeln!(
                        svc_lines,
                        "service {} {} id {} name {} port {} ident {ident} expires {expires}s",
                        peer.name, peer.ip, peer.id, svc.name, svc.port
                    );
                }
            }
            let (hs, tx, rx, _, _) = st.tunn.stats();
            let path = match st.path {
                Path::Direct(a) if st.conn.as_ref().is_some_and(|c| c.peer == a) => format!("direct {a}"),
                Path::Direct(a) => format!("direct {a} (unconnected)"),
                Path::Relay(a) => {
                    let name = self.relays.iter().find(|r| r.addr == a).map(|r| r.name.as_str()).unwrap_or("?");
                    format!("relay {name}")
                }
                Path::None => "none".into(),
            };
            let hs = hs.map(|d| format!("{}s", d.as_secs())).unwrap_or("never".into());
            let key = meshproto::b64(&peer.key);
            let _ = writeln!(s, "peer {} {} {path} handshake {hs} tx {tx} rx {rx} id {} key {key}", peer.name, peer.ip, peer.id);
        }
        s.push_str(&svc_lines);
        let tmp = format!("{STATUS_FILE}.tmp");
        if std::fs::write(&tmp, s).is_ok() {
            std::fs::rename(&tmp, STATUS_FILE).ok();
        }
    }
}

/// Whether an announcement with timestamp `ts` replaces one with `last`: newer only (drops
/// replays and reordering) and close to our clock (drops old captures after a restart).
fn accept_services(last: u64, ts: u64, now_nanos: u64) -> bool {
    ts > last && ts.abs_diff(now_nanos) <= MAX_SERVICES_SKEW_NANOS
}

fn services_ttl(asked: u16) -> Duration {
    Duration::from_secs(asked.min(MAX_SERVICES_TTL) as u64)
}

/// Whether our (online) peer should get our service list now: on a change, then every
/// `ANNOUNCE_EVERY`, as long as we have services or still owe it a withdrawal.
fn announce_due(list: &[Service], changed: bool, sent_some: bool, sent: Option<Instant>, now: Instant) -> bool {
    if list.is_empty() && !sent_some {
        return false;
    }
    changed || sent.is_none_or(|t| now.duration_since(t) >= ANNOUNCE_EVERY)
}

/// Writes a world-readable file atomically (temporary file, then rename).
fn publish(path: &std::path::Path, data: &[u8]) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = std::path::PathBuf::from(tmp);
    std::fs::write(&tmp, data)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o644))?;
    }
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::Inbound;
    use super::{accept_services, announce_due, services_ttl, ANNOUNCE_EVERY, MAX_SERVICES_SKEW_NANOS};
    use meshproto::service::Service;
    use std::time::{Duration, Instant};

    #[test]
    fn no_route_must_echo_a_hello_since_the_last_ack() {
        use super::fresh_no_route;
        assert!(!fresh_no_route(5, 0, 5), "no ACK yet: nothing to compare with");
        assert!(fresh_no_route(10, 10, 10));
        assert!(fresh_no_route(12, 10, 12), "newer HELLO accepted, its ACK lost");
        assert!(!fresh_no_route(9, 10, 12), "replayed from before the last ACK");
        assert!(!fresh_no_route(13, 10, 12), "a HELLO we never sent");
    }

    #[test]
    fn announcements_must_be_newer_and_near_our_clock() {
        let now = 1_000_000_000_000_000_000u64;
        assert!(accept_services(0, now, now));
        assert!(accept_services(now - 5, now, now));
        assert!(!accept_services(now, now, now), "replay of the same one");
        assert!(!accept_services(now, now - 1, now), "older than the last accepted");
        assert!(!accept_services(0, now - MAX_SERVICES_SKEW_NANOS - 1, now), "an old capture after a restart");
        assert!(!accept_services(0, now + MAX_SERVICES_SKEW_NANOS + 1, now), "from the future");
        assert_eq!(services_ttl(60), Duration::from_secs(60));
        assert_eq!(services_ttl(u16::MAX), Duration::from_secs(300));
    }

    #[test]
    fn announce_on_change_then_periodically_and_withdraw_once() {
        let now = Instant::now();
        let some = [Service { name: "agent-inbox".into(), port: 8832, ident: String::new() }];
        assert!(announce_due(&some, false, false, None, now), "never sent");
        assert!(!announce_due(&some, false, true, Some(now), now));
        assert!(announce_due(&some, true, true, Some(now), now), "changed");
        assert!(announce_due(&some, false, true, Some(now), now + ANNOUNCE_EVERY), "periodic");
        assert!(announce_due(&[], true, true, Some(now), now), "withdrawal owed");
        assert!(!announce_due(&[], true, false, Some(now), now), "nothing to say");
        assert!(!announce_due(&[], false, false, None, now), "nodes without services stay silent");
    }

    #[test]
    fn publish_replaces_atomically_and_is_world_readable() {
        let dir = std::env::temp_dir().join(format!("meshd-publish-{}", meshproto::random_u64()));
        let path = dir.join("sub").join("roster");
        super::publish(&path, b"one").unwrap();
        super::publish(&path, b"two").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"two");
        assert!(!dir.join("sub").join("roster.tmp").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o644);
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    fn tcp(dport: u16, flags: u8) -> Vec<u8> {
        let mut p = vec![0u8; 40];
        p[0] = 0x45;
        p[9] = 6;
        p[22..24].copy_from_slice(&dport.to_be_bytes());
        p[33] = flags;
        p
    }

    fn udp(dport: u16) -> Vec<u8> {
        let mut p = vec![0u8; 28];
        p[0] = 0x45;
        p[9] = 17;
        p[22..24].copy_from_slice(&dport.to_be_bytes());
        p
    }

    #[test]
    fn only_listed_ports_open() {
        let f = Inbound::parse("tcp:22").unwrap();
        assert!(f.allows(&tcp(22, 0x02))); // SYN to ssh
        assert!(!f.allows(&tcp(8088, 0x02))); // SYN to the AC web page
        assert!(f.allows(&tcp(51234, 0x10))); // ACK: reply to a connection we opened
        assert!(!f.allows(&udp(1812))); // RADIUS
        let mut icmp = vec![0u8; 28];
        icmp[0] = 0x45;
        icmp[9] = 1;
        assert!(f.allows(&icmp));
    }
}
