//! Self-update from the relays.
//!
//! A relay announces its release (version + manifest SHA-256) in every HELLO_ACK. When that is
//! newer than what this node runs, the node fetches the manifest (signed by the admin key,
//! like the roster), then its own platform's binary, in chunks over the same UDP port. The
//! binary must hash to the manifest entry. It replaces the running executable (the old one is
//! kept as `<exe>.old`) and the process exits; launchd / systemd / `meshd supervise` start the
//! new one.
//!
//! Safety net: `<dir>/update.pending` counts the new binary's start attempts. Once it reaches
//! a relay it is healthy: the marker goes away and the version is recorded. If it starts three
//! times without reaching one, the old binary is put back and that version is not taken again.

use meshproto::release::{self, Release};
use meshproto::wire;
use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const MAX_ATTEMPTS: u32 = 3;
/// Re-ask for missing chunks after this long without a new one.
const STALL: Duration = Duration::from_secs(2);
/// Give up a download after this long without progress (retried on a later HELLO_ACK).
const ABANDON: Duration = Duration::from_secs(30);
/// After a failed install, leave that version alone this long (no 1 MB download every 25 s).
const RETRY_FAILED: Duration = Duration::from_secs(600);

enum Want {
    Manifest,
    Binary { version: u64 },
}

struct Download {
    relay: SocketAddr,
    hash: [u8; 32],
    want: Want,
    data: Vec<u8>,
    got: Vec<bool>,
    last_progress: Instant,
    asked: Instant,
}

pub struct Updater {
    dir: PathBuf,
    exe: PathBuf,
    admin_pub: [u8; 32],
    /// Release version this binary is known to be (0: unknown, e.g. installed by hand).
    installed: u64,
    /// A version that failed its health check and is not taken again.
    bad: u64,
    /// The version under trial, until this process reaches a relay.
    pending: Option<u64>,
    dl: Option<Download>,
    /// A version whose install failed, and when.
    failed: Option<(u64, Instant)>,
}

fn read_u64(p: &Path) -> u64 {
    std::fs::read_to_string(p).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0)
}

fn sibling(exe: &Path, suffix: &str) -> PathBuf {
    let mut s = exe.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

impl Updater {
    /// Called first thing at startup. Counts this start against a pending update and rolls
    /// back (exiting, so the service restarts the old binary) once it has failed too often.
    pub fn start(dir: &Path, admin_pub: [u8; 32]) -> io::Result<Updater> {
        let exe = std::env::current_exe()?;
        let pending_path = dir.join("update.pending");
        let mut u = Updater {
            dir: dir.to_path_buf(),
            exe: exe.clone(),
            admin_pub,
            installed: read_u64(&dir.join("release.version")),
            bad: read_u64(&dir.join("release.bad")),
            pending: None,
            dl: None,
            failed: None,
        };
        u.clean_stale();
        if let Ok(s) = std::fs::read_to_string(&pending_path) {
            let mut it = s.split_whitespace().map(|x| x.parse::<u64>().unwrap_or(0));
            let (version, attempts) = (it.next().unwrap_or(0), it.next().unwrap_or(0) as u32 + 1);
            if attempts > MAX_ATTEMPTS {
                eprintln!("meshd: release v{version} never reached a relay in {MAX_ATTEMPTS} starts, rolling back");
                let old = sibling(&exe, ".old");
                if old.exists() {
                    std::fs::rename(&exe, sibling(&exe, ".bad")).ok();
                    std::fs::rename(&old, &exe)?;
                }
                std::fs::write(dir.join("release.bad"), version.to_string()).ok();
                std::fs::remove_file(&pending_path).ok();
                std::process::exit(4);
            }
            std::fs::write(&pending_path, format!("{version} {attempts}"))?;
            eprintln!("meshd: running release v{version}, start {attempts} of {MAX_ATTEMPTS} before it counts as healthy");
            u.pending = Some(version);
        }
        Ok(u)
    }

    /// The process reached a relay: a pending update is healthy.
    pub fn healthy(&mut self) {
        let Some(v) = self.pending.take() else { return };
        std::fs::write(self.dir.join("release.version"), v.to_string()).ok();
        std::fs::remove_file(self.dir.join("update.pending")).ok();
        // Fails on Windows while the supervisor still runs from it; harmless.
        std::fs::remove_file(sibling(&self.exe, ".old")).ok();
        self.installed = v;
        eprintln!("meshd: release v{v} is healthy ({})", release::platform());
    }

    /// A relay announced release `version` with manifest `hash`.
    pub fn announced(&mut self, sock: &UdpSocket, relay: SocketAddr, version: u64, hash: [u8; 32]) {
        let recently_failed = self.failed.is_some_and(|(v, t)| v == version && t.elapsed() < RETRY_FAILED);
        if version <= self.installed || version == self.bad || recently_failed || self.pending.is_some() || self.dl.is_some() {
            return;
        }
        eprintln!("meshd: release v{version} announced (running v{}), fetching the manifest", self.installed);
        self.begin(sock, relay, hash, Want::Manifest);
    }

    fn begin(&mut self, sock: &UdpSocket, relay: SocketAddr, hash: [u8; 32], want: Want) {
        let now = Instant::now();
        self.dl = Some(Download { relay, hash, want, data: vec![], got: vec![], last_progress: now, asked: now });
        self.ask(sock);
    }

    /// Asks for the next window, starting at the first missing chunk.
    fn ask(&mut self, sock: &UdpSocket) {
        let Some(d) = self.dl.as_mut() else { return };
        let first = d.got.iter().position(|g| !g).unwrap_or(d.got.len());
        let mut m = wire::header(wire::BLOB_GET, 0).to_vec();
        m.extend_from_slice(&d.hash);
        m.extend_from_slice(&((first * wire::ROSTER_CHUNK) as u32).to_be_bytes());
        m.push(wire::BLOB_WINDOW);
        sock.send_to(&m, d.relay).ok();
        d.asked = Instant::now();
    }

    /// A BLOB_PART from `relay`.
    pub fn part(&mut self, sock: &UdpSocket, relay: SocketAddr, p: &[u8]) {
        let Some(d) = self.dl.as_mut().filter(|d| d.relay == relay) else { return };
        let b = &p[wire::HDR..];
        if b.len() < 40 || b[..32] != d.hash {
            return;
        }
        let (Some(total), Some(off)) = (wire::get_u32(&b[32..]), wire::get_u32(&b[36..])) else { return };
        let (total, off, bytes) = (total as usize, off as usize, &b[40..]);
        if total > 64 << 20 || off % wire::ROSTER_CHUNK != 0 || off + bytes.len() > total {
            return;
        }
        if d.got.is_empty() {
            d.data = vec![0; total];
            d.got = vec![false; total.div_ceil(wire::ROSTER_CHUNK)];
        }
        if d.data.len() != total {
            return;
        }
        let i = off / wire::ROSTER_CHUNK;
        if !d.got[i] {
            d.data[off..off + bytes.len()].copy_from_slice(bytes);
            d.got[i] = true;
            d.last_progress = Instant::now();
        }
        if d.got.iter().all(|g| *g) {
            let d = self.dl.take().expect("download");
            self.complete(sock, d);
        } else if (i + 1).is_multiple_of(wire::BLOB_WINDOW as usize) {
            // Last chunk of a window: ask for the next one (from the first gap) right away.
            self.ask(sock);
        }
    }

    /// Re-asks for stalled downloads and gives up dead ones; called from the timer tick.
    pub fn tick(&mut self, sock: &UdpSocket) {
        let Some(d) = &self.dl else { return };
        if d.last_progress.elapsed() > ABANDON {
            eprintln!("meshd: update download stalled, will retry on a later announcement");
            self.dl = None;
        } else if d.asked.elapsed() > STALL {
            self.ask(sock);
        }
    }

    fn complete(&mut self, sock: &UdpSocket, d: Download) {
        if release::sha256(&d.data) != d.hash {
            eprintln!("meshd: update blob does not match its hash, dropped");
            return;
        }
        match d.want {
            Want::Manifest => {
                let rel = match Release::parse_verified(&d.data, &self.admin_pub) {
                    Ok(r) => r,
                    Err(e) => {
                        eprintln!("meshd: release manifest rejected: {e}");
                        return;
                    }
                };
                let Some(bin) = rel.for_platform(&release::platform()).cloned() else {
                    eprintln!("meshd: release v{} has no {} binary", rel.version, release::platform());
                    self.record(rel.version);
                    return;
                };
                // Already running it (e.g. installed by hand): just remember the version.
                if std::fs::read(&self.exe).is_ok_and(|me| release::sha256(&me) == bin.sha256) {
                    self.record(rel.version);
                    return;
                }
                eprintln!("meshd: fetching the {} binary of release v{} ({} bytes)", bin.platform, rel.version, bin.size);
                self.begin(sock, d.relay, bin.sha256, Want::Binary { version: rel.version });
            }
            Want::Binary { version } => {
                if let Err(e) = self.install(&d.data, version) {
                    eprintln!("meshd: installing release v{version}: {e} (retrying in 10 minutes)");
                    self.failed = Some((version, Instant::now()));
                    return;
                }
                eprintln!("meshd: release v{version} installed, restarting into it");
                std::process::exit(3);
            }
        }
    }

    /// Deletes `.stale-*` leftovers whose process has exited (best effort).
    fn clean_stale(&self) {
        let (Some(dir), Some(name)) = (self.exe.parent(), self.exe.file_name()) else { return };
        let prefix = format!("{}.stale-", name.to_string_lossy());
        if let Ok(entries) = std::fs::read_dir(dir) {
            for e in entries.flatten() {
                if e.file_name().to_string_lossy().starts_with(&prefix) {
                    std::fs::remove_file(e.path()).ok();
                }
            }
        }
    }

    fn record(&mut self, version: u64) {
        self.installed = version;
        std::fs::write(self.dir.join("release.version"), version.to_string()).ok();
    }

    /// Writes the new binary next to the running one, then swaps them (renaming a running
    /// executable is allowed on every platform; the old one stays as `<exe>.old`).
    fn install(&self, data: &[u8], version: u64) -> io::Result<()> {
        let new = sibling(&self.exe, ".new");
        std::fs::write(&new, data)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&new, std::fs::Permissions::from_mode(0o755))?;
        }
        std::fs::write(self.dir.join("update.pending"), format!("{version} 0"))?;
        let old = sibling(&self.exe, ".old");
        // On Windows `meshd supervise` keeps running from the previous `.old`, which can then
        // be neither deleted nor replaced, only renamed: move it aside (cleaned up later).
        if old.exists() && std::fs::remove_file(&old).is_err() {
            let stale = sibling(&self.exe, &format!(".stale-{}", meshproto::random_u64()));
            std::fs::rename(&old, stale)?;
        }
        std::fs::rename(&self.exe, &old)?;
        if let Err(e) = std::fs::rename(&new, &self.exe) {
            std::fs::rename(&old, &self.exe).ok();
            std::fs::remove_file(self.dir.join("update.pending")).ok();
            return Err(e);
        }
        Ok(())
    }

    pub fn status(&self) -> String {
        let dl = match &self.dl {
            Some(d) => {
                let have = d.got.iter().filter(|g| **g).count();
                format!(" downloading {have}/{}", d.got.len())
            }
            None => String::new(),
        };
        format!("release v{} ({}){dl}", self.installed, release::platform())
    }
}
