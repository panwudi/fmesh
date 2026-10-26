//! meshd: flytoMesh node daemon, plus the admin commands (keys, roster signing).
//!
//! Node state lives in one directory (default /etc/flytomesh, C:\ProgramData\flytoMesh on
//! Windows): `key` (WireGuard private key),
//! `admin.pub` (roster signing key), `roster` (signed, updated from the relays). The daemon
//! writes its state to `/var/run/flytomesh.status` every 2 seconds.

mod batch;
mod dns;
mod modules;
mod netif;
mod node;
mod services;
mod summary;
mod tun;
mod udp;
mod update;

use meshproto::{b64, decode_key, roster, x25519};
use std::io::{self, Read};
use std::path::PathBuf;
use std::process::exit;

#[cfg(unix)]
const DEFAULT_DIR: &str = "/etc/flytomesh";
#[cfg(windows)]
const DEFAULT_DIR: &str = r"C:\ProgramData\flytoMesh";

const USAGE: &str = "usage:
  meshd up [--dir D] [--listen PORT]                   run the node (root)
           [--inbound tcp:22,udp:53]                   only these ports may be opened from the mesh
           [--force-relay] [--no-vpn-bypass]           testing: relay only / do not pin past a VPN
  meshd supervise [up flags]                           run `meshd up`, restart it whenever it exits,
                                                       log to <dir>/meshd.log (Windows startup task)
  meshd status [--summary]                             print the running node's state
                                                       (--summary: online count, services, one line per device)
  meshd genkey                                         new X25519 private key (node or relay)
  meshd pubkey < key                                   public key of a private key
  meshd admin-genkey                                   new roster signing key (keep on the admin machine)
  meshd admin-pubkey < admin-key                       its public key, for admin.pub
  meshd roster-sign --admin-key F < body > roster      sign a roster body
  meshd roster-check --admin-pub B64 < roster          verify and summarize a roster
  meshd release-sign --admin-key F < body > manifest   sign a release manifest (deploy/push-release.sh)
  meshd release-check --admin-pub B64 < manifest       verify and summarize a release manifest";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let rest = args.get(1..).unwrap_or(&[]);
    let res = match args.first().map(String::as_str) {
        Some("up") => up(rest),
        Some("supervise") => supervise(rest),
        Some("status") => status(rest),
        Some("version" | "--version") => {
            println!("meshd {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some("genkey") => {
            println!("{}", b64(&meshproto::new_secret().to_bytes()));
            Ok(())
        }
        Some("pubkey") => stdin_key().map(|k| {
            println!("{}", b64(x25519::PublicKey::from(&x25519::StaticSecret::from(k)).as_bytes()));
        }),
        Some("admin-genkey") => {
            let mut seed = [0u8; 32];
            meshproto::fill_random(&mut seed);
            println!("{}", b64(&seed));
            Ok(())
        }
        Some("admin-pubkey") => stdin_key().and_then(|k| roster::admin_public(&k)).map(|p| println!("{}", b64(&p))),
        Some("roster-sign") => roster_sign(rest),
        Some("roster-check") => roster_check(rest),
        Some("release-sign") => release_sign(rest),
        Some("release-check") => release_check(rest),
        _ => {
            eprintln!("{USAGE}");
            exit(2);
        }
    };
    if let Err(e) = res {
        eprintln!("meshd: {e}");
        exit(1);
    }
}

fn bad(m: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, m)
}

fn stdin_key() -> io::Result<[u8; 32]> {
    let mut s = String::new();
    io::stdin().read_line(&mut s)?;
    decode_key(&s)
}

fn stdin_all() -> io::Result<Vec<u8>> {
    let mut v = vec![];
    io::stdin().read_to_end(&mut v)?;
    Ok(v)
}

/// `--flag value` pairs plus boolean flags.
fn flags<'a>(args: &'a [String], booleans: &[&str]) -> io::Result<Vec<(&'a str, &'a str)>> {
    let mut out = vec![];
    let mut it = args.iter();
    while let Some(f) = it.next() {
        if booleans.contains(&f.as_str()) {
            out.push((f.as_str(), ""));
        } else {
            let v = it.next().ok_or_else(|| bad(format!("{f} needs a value")))?;
            out.push((f.as_str(), v.as_str()));
        }
    }
    Ok(out)
}

fn up(args: &[String]) -> io::Result<()> {
    let mut cfg = node::Config {
        dir: DEFAULT_DIR.into(),
        listen: meshproto::NODE_PORT,
        force_relay: false,
        no_vpn_bypass: false,
        inbound: None,
    };
    for (f, v) in flags(args, &["--force-relay", "--no-vpn-bypass"])? {
        match f {
            "--dir" => cfg.dir = PathBuf::from(v),
            "--listen" => cfg.listen = v.parse().map_err(|_| bad("bad --listen".into()))?,
            "--force-relay" => cfg.force_relay = true,
            "--no-vpn-bypass" => cfg.no_vpn_bypass = true,
            "--inbound" => cfg.inbound = Some(node::Inbound::parse(v)?),
            _ => return Err(bad(format!("unknown flag {f}"))),
        }
    }
    node::run(cfg)
}

/// Keeps `meshd up` running: Windows has no launchd / systemd, and meshd exits on purpose
/// after taking a newer roster. The child's output goes to `<dir>/meshd.log` (kept under
/// 5 MB by rotating to `meshd.log.1`).
fn supervise(args: &[String]) -> io::Result<()> {
    use std::io::Write;
    let mut dir = PathBuf::from(DEFAULT_DIR);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--dir" {
            dir = PathBuf::from(it.next().ok_or_else(|| bad("--dir needs a value".into()))?);
        }
    }
    let exe = std::env::current_exe()?;
    let log_path = dir.join("meshd.log");
    loop {
        if std::fs::metadata(&log_path).is_ok_and(|m| m.len() > 5 << 20) {
            std::fs::rename(&log_path, dir.join("meshd.log.1")).ok();
        }
        let mut log = std::fs::OpenOptions::new().create(true).append(true).open(&log_path)?;
        let started = std::time::Instant::now();
        let st = std::process::Command::new(&exe)
            .arg("up")
            .args(args)
            .stdout(log.try_clone()?)
            .stderr(log.try_clone()?)
            .status();
        writeln!(log, "meshd supervise: child exited: {st:?}").ok();
        // A crash loop should not spin; a deliberate restart (new roster) comes back fast.
        let pause = if started.elapsed() < std::time::Duration::from_secs(10) { 5 } else { 1 };
        std::thread::sleep(std::time::Duration::from_secs(pause));
    }
}

fn status(args: &[String]) -> io::Result<()> {
    let summary = match args {
        [] => false,
        [f] if f == "--summary" => true,
        [f, ..] => return Err(bad(format!("unknown flag {f}"))),
    };
    let path = node::STATUS_FILE;
    let age = std::fs::metadata(path).and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok());
    let fresh = age.is_some_and(|a| a.as_secs() <= 10);
    if !fresh {
        eprintln!("meshd: {path} is stale or missing; is meshd running?");
    }
    let text = std::fs::read_to_string(path)?;
    if summary {
        print!("{}", summary::render(&text, fresh));
    } else {
        print!("{text}");
    }
    Ok(())
}

fn roster_sign(args: &[String]) -> io::Result<()> {
    let mut key = None;
    for (f, v) in flags(args, &[])? {
        match f {
            "--admin-key" => key = Some(decode_key(&std::fs::read_to_string(v)?)?),
            _ => return Err(bad(format!("unknown flag {f}"))),
        }
    }
    let key = key.ok_or_else(|| bad("--admin-key is required".into()))?;
    let body = String::from_utf8(stdin_all()?).map_err(|_| bad("roster body is not UTF-8".into()))?;
    print!("{}", roster::sign(&body, &key)?);
    Ok(())
}

fn release_sign(args: &[String]) -> io::Result<()> {
    let mut key = None;
    for (f, v) in flags(args, &[])? {
        match f {
            "--admin-key" => key = Some(decode_key(&std::fs::read_to_string(v)?)?),
            _ => return Err(bad(format!("unknown flag {f}"))),
        }
    }
    let key = key.ok_or_else(|| bad("--admin-key is required".into()))?;
    let body = String::from_utf8(stdin_all()?).map_err(|_| bad("release body is not UTF-8".into()))?;
    print!("{}", meshproto::release::sign(&body, &key)?);
    Ok(())
}

fn release_check(args: &[String]) -> io::Result<()> {
    let mut admin = None;
    for (f, v) in flags(args, &[])? {
        match f {
            "--admin-pub" => admin = Some(decode_key(v)?),
            _ => return Err(bad(format!("unknown flag {f}"))),
        }
    }
    let admin = admin.ok_or_else(|| bad("--admin-pub is required".into()))?;
    let r = meshproto::release::Release::parse_verified(&stdin_all()?, &admin)?;
    println!("release v{} ok: {} binaries", r.version, r.meshd.len());
    for b in &r.meshd {
        println!("  {} {} {}", b.platform, meshproto::release::hex(&b.sha256), b.size);
    }
    for b in &r.modules {
        println!("  module {} {} {} {}", b.module, b.platform, meshproto::release::hex(&b.sha256), b.size);
    }
    Ok(())
}

fn roster_check(args: &[String]) -> io::Result<()> {
    let mut admin = None;
    for (f, v) in flags(args, &[])? {
        match f {
            "--admin-pub" => admin = Some(decode_key(v)?),
            _ => return Err(bad(format!("unknown flag {f}"))),
        }
    }
    let admin = admin.ok_or_else(|| bad("--admin-pub is required".into()))?;
    let r = roster::Roster::parse_verified(&stdin_all()?, &admin)?;
    println!("roster v{} ok: {} relays, {} nodes", r.version, r.relays.len(), r.nodes.len());
    for x in &r.relays {
        println!("  relay {} {}", x.name, x.addr);
    }
    for n in &r.nodes {
        println!("  node {} {} {}", n.id, n.name, n.ip);
    }
    Ok(())
}
