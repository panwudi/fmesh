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
//!
//! Module files (modules.rs) come after meshd's own update, one at a time, written to disk as
//! they arrive. Nothing about them touches the health check or rollback above.

use crate::modules::{Modules, State as ModState};
use meshproto::release::{self, Binary, Release};
use meshproto::wire;
use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const MAX_ATTEMPTS: u32 = 3;
/// Re-ask for missing chunks after this long without a new one.
const STALL: Duration = Duration::from_secs(2);
/// Pause a download after this long without progress. The chunks received so far are kept and
/// the download resumes when the same file is asked for again (on a later HELLO_ACK), so a file
/// larger than a relay's fetch budget still completes.
const ABANDON: Duration = Duration::from_secs(30);
/// After a failed install, leave that version alone this long (no 1 MB download every 25 s).
const RETRY_FAILED: Duration = Duration::from_secs(600);
/// How often module states are recomputed from the files.
const MODULES_EVERY: Duration = Duration::from_secs(10);

enum Want {
    Manifest,
    Binary { version: u64 },
    Module { name: String, bin: Binary },
}

struct Download {
    relay: SocketAddr,
    hash: [u8; 32],
    want: Want,
    /// In memory (manifest, meshd), or `sink` for module files.
    data: Vec<u8>,
    sink: Option<std::fs::File>,
    total: usize,
    got: Vec<bool>,
    last_progress: Instant,
    asked: Instant,
}

impl Download {
    fn is_module(&self) -> bool {
        matches!(self.want, Want::Module { .. })
    }
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
    /// A stalled download, kept for resuming.
    paused: Option<Download>,
    /// A version whose install failed, and when.
    failed: Option<(u64, Instant)>,
    /// The newest release a relay announced: version, relay name, when.
    latest: Option<(u64, String, Instant)>,
    /// The relay that announced it, for module downloads.
    latest_relay: Option<SocketAddr>,
    /// The last thing that went wrong, for the status file.
    error: Option<String>,
    pub modules: Modules,
    /// A manifest that did not verify, not fetched again for a while.
    manifest_failed: Option<([u8; 32], Instant)>,
    modules_at: Option<Instant>,
    modules_retry: Instant,
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
            paused: None,
            failed: None,
            latest: None,
            latest_relay: None,
            error: None,
            modules: Modules::new(exe.parent().map(Path::to_path_buf).unwrap_or_default(), dir),
            manifest_failed: None,
            modules_at: None,
            modules_retry: Instant::now(),
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
        self.error = None;
        eprintln!("meshd: release v{v} is healthy ({})", release::platform());
    }

    /// Relay `relay_name` announced release `version` with manifest `hash`.
    pub fn announced(&mut self, sock: &UdpSocket, relay: SocketAddr, relay_name: &str, version: u64, hash: [u8; 32]) {
        if self.latest.as_ref().is_none_or(|(v, _, _)| version >= *v) {
            self.latest = Some((version, relay_name.to_string(), Instant::now()));
            self.latest_relay = Some(relay);
        }
        let recently_failed = self.failed.is_some_and(|(v, t)| v == version && t.elapsed() < RETRY_FAILED);
        let need_meshd = !(version <= self.installed || version == self.bad || recently_failed || self.pending.is_some());
        // The manifest also lists module files: fetch it once per release even when meshd
        // itself is already current.
        let bad_manifest = self.manifest_failed.is_some_and(|(h, t)| h == hash && t.elapsed() < RETRY_FAILED);
        let need_manifest = version >= self.installed && self.modules.release_version().is_none_or(|v| v < version) && !bad_manifest;
        if !(need_meshd || need_manifest) {
            return;
        }
        // A module download never holds up meshd's own update or a newer manifest.
        if self.dl.as_ref().is_some_and(Download::is_module) {
            self.pause_module();
        }
        if self.dl.is_some() {
            return;
        }
        if need_meshd {
            eprintln!("meshd: release v{version} announced (running v{}), fetching the manifest", self.installed);
        }
        self.begin(sock, relay, hash, Want::Manifest);
    }

    fn pause_module(&mut self) {
        if let Some(d) = self.dl.take() {
            if let Want::Module { name, .. } = &d.want {
                self.modules.set_state(name, ModState::Waiting);
            }
            self.paused = Some(d);
        }
    }

    fn begin(&mut self, sock: &UdpSocket, relay: SocketAddr, hash: [u8; 32], want: Want) {
        let now = Instant::now();
        let (data, sink, total, got) = match self.paused.take_if(|p| p.hash == hash) {
            Some(p) => {
                let have = p.got.iter().filter(|g| **g).count();
                eprintln!("meshd: resuming the download at {have}/{} chunks", p.got.len());
                (p.data, p.sink, p.total, p.got)
            }
            None => (vec![], None, 0, vec![]),
        };
        let sink = match (&want, sink) {
            (_, Some(f)) => Some(f),
            (Want::Module { name, bin }, None) => {
                let opened = self.modules.part_path(bin).and_then(std::fs::File::create);
                match opened {
                    Ok(f) => Some(f),
                    Err(e) => {
                        self.modules.set_state(name, ModState::Failed(format!("cache: {e}")));
                        return;
                    }
                }
            }
            _ => None,
        };
        if let Want::Module { name, .. } = &want {
            self.modules.set_state(name, ModState::Downloading);
        }
        self.dl = Some(Download { relay, hash, want, data, sink, total, got, last_progress: now, asked: now });
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
        if total > 64 << 20 || off % wire::ROSTER_CHUNK != 0 || off + bytes.len() > total || bytes.is_empty() || off >= total {
            return;
        }
        if d.got.is_empty() {
            if d.sink.is_none() {
                d.data = vec![0; total];
            }
            d.total = total;
            d.got = vec![false; total.div_ceil(wire::ROSTER_CHUNK)];
        }
        if d.total != total {
            return;
        }
        let i = off / wire::ROSTER_CHUNK;
        if !d.got[i] {
            match &d.sink {
                Some(f) => {
                    if write_at(f, bytes, off as u64).is_err() {
                        return;
                    }
                }
                None => d.data[off..off + bytes.len()].copy_from_slice(bytes),
            }
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

    /// Re-asks for stalled downloads and gives up dead ones; called from the timer tick. With
    /// nothing else going on, fetches the next module file.
    pub fn tick(&mut self, sock: &UdpSocket) {
        if let Some(d) = &self.dl {
            if d.last_progress.elapsed() > ABANDON {
                eprintln!("meshd: update download stalled, will resume later");
                if d.is_module() {
                    self.pause_module();
                } else {
                    self.error = Some("download stalled, resuming on the next announcement".into());
                    self.paused = self.dl.take();
                }
            } else if d.asked.elapsed() > STALL {
                self.ask(sock);
            }
            return;
        }
        let Some(rv) = self.modules.release_version() else { return };
        if self.pending.is_some() || rv > self.installed && rv != self.bad {
            return; // meshd's own update first
        }
        if self.modules_at.is_some_and(|t| t.elapsed() < MODULES_EVERY) {
            return;
        }
        self.modules_at = Some(Instant::now());
        if self.modules_retry.elapsed() > RETRY_FAILED {
            self.modules_retry = Instant::now();
            self.modules.retry_failed();
        } else {
            self.modules.refresh();
        }
        self.auto_install();
        if let (Some((name, bin)), Some(relay)) = (self.modules.next_download(), self.latest_relay) {
            eprintln!("meshd: fetching module file {name} of release v{rv} ({} bytes)", bin.size);
            self.begin(sock, relay, bin.sha256, Want::Module { name, bin });
        }
    }

    /// Installs ready files of the groups listed in `<dir>/auto-update`.
    fn auto_install(&mut self) {
        for group in self.modules.auto_groups() {
            self.install_group(&group);
        }
    }

    /// A local request or the auto-update list: install one group's ready files.
    pub fn install_group(&mut self, group: &str) -> Option<String> {
        let ready = crate::modules::GROUPS
            .iter()
            .find(|(g, _)| *g == group)
            .is_some_and(|(_, files)| files.iter().any(|f| self.modules.entries.get(*f).is_some_and(|e| e.state == ModState::Ready)));
        if !ready {
            return None;
        }
        let res = self.modules.install(group);
        match &res {
            Ok(done) => eprintln!("meshd: installed {group}: {}", done.join(", ")),
            Err(e) => eprintln!("meshd: installing {group} failed: {e}"),
        }
        Some(res.map(|d| d.join(", ")).unwrap_or_else(|e| e))
    }

    /// The `module` lines of the status file.
    pub fn module_lines(&self) -> Vec<String> {
        let progress = self.dl.as_ref().and_then(|d| match &d.want {
            Want::Module { name, .. } => Some((name.as_str(), d.got.iter().filter(|g| **g).count(), d.got.len())),
            _ => None,
        });
        self.modules.status_lines(progress)
    }

    fn complete(&mut self, sock: &UdpSocket, d: Download) {
        if let Want::Module { name, bin } = &d.want {
            drop(d.sink);
            match self.modules.finished(name, bin) {
                Ok(()) => {
                    eprintln!("meshd: module file {name} ready");
                    self.auto_install();
                }
                Err(e) => eprintln!("meshd: {e}"),
            }
            self.modules_at = None; // next file right away
            return;
        }
        if release::sha256(&d.data) != d.hash {
            eprintln!("meshd: update blob does not match its hash, dropped");
            self.error = Some("downloaded file does not match its hash".into());
            return;
        }
        match d.want {
            Want::Manifest => {
                let rel = match Release::parse_verified(&d.data, &self.admin_pub) {
                    Ok(r) => r,
                    Err(e) => {
                        eprintln!("meshd: release manifest rejected: {e}");
                        self.error = Some(format!("manifest rejected: {e}"));
                        self.manifest_failed = Some((d.hash, Instant::now()));
                        return;
                    }
                };
                // A newer manifest makes an older paused download pointless.
                if self.paused.as_ref().is_some_and(|p| p.is_module()) {
                    self.paused = None;
                }
                self.modules.set_release(rel.clone());
                self.modules_at = None;
                if rel.version <= self.installed || rel.version == self.bad {
                    return; // fetched for its module files; meshd is current
                }
                let Some(bin) = rel.for_platform(&release::platform()).cloned() else {
                    eprintln!("meshd: release v{} has no {} binary", rel.version, release::platform());
                    self.error = Some(format!("release v{} has no {} binary", rel.version, release::platform()));
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
            Want::Module { .. } => unreachable!("handled above"),
            Want::Binary { version } => {
                if let Err(e) = self.install(&d.data, version) {
                    eprintln!("meshd: installing release v{version}: {e} (retrying in 10 minutes)");
                    self.error = Some(format!("installing v{version} failed: {e}"));
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
        self.error = None;
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

    #[cfg(test)]
    fn bare(admin_pub: [u8; 32]) -> Updater {
        Updater::bare_in(admin_pub, std::env::temp_dir())
    }

    #[cfg(test)]
    fn bare_in(admin_pub: [u8; 32], sys: PathBuf) -> Updater {
        Updater {
            dir: std::env::temp_dir(),
            exe: PathBuf::from("/nonexistent/meshd"),
            admin_pub,
            installed: 0,
            bad: 0,
            pending: None,
            dl: None,
            paused: None,
            failed: None,
            latest: None,
            latest_relay: None,
            error: None,
            modules: Modules::new(sys.clone(), &sys),
            manifest_failed: None,
            modules_at: None,
            modules_retry: Instant::now(),
        }
    }

    /// The `update` line of the status file (docs/network.md section 5):
    /// `update version <x.y.z> release <n> [latest <n> via <relay> age <s>] state <s>
    /// [progress <a>/<b>] [bad <n>] [error <text>]`. `latest` is what a relay said last, so
    /// "up to date" is only as fresh as `age`.
    pub fn status_line(&self) -> String {
        let mut s = format!("update version {} release {}", env!("CARGO_PKG_VERSION"), self.installed);
        if let Some((v, relay, at)) = &self.latest {
            s += &format!(" latest {v} via {relay} age {}", at.elapsed().as_secs());
        }
        let count = |d: &Download| (d.got.iter().filter(|g| **g).count(), d.got.len());
        let (state, progress) = match (&self.pending, &self.dl, &self.paused) {
            (Some(_), _, _) => ("trial", None),
            (_, Some(d), _) if matches!(d.want, Want::Binary { .. }) => ("downloading", Some(count(d))),
            (_, Some(_), _) => ("checking", None),
            (_, None, Some(d)) => ("paused", Some(count(d))),
            _ => ("idle", None),
        };
        s += &format!(" state {state}");
        if let Some((have, total)) = progress {
            s += &format!(" progress {have}/{total}");
        }
        if self.bad > self.installed {
            s += &format!(" bad {}", self.bad);
        }
        let auto = self.modules.auto_groups();
        if !auto.is_empty() {
            s += &format!(" auto {}", auto.join(","));
        }
        if let Some(e) = &self.error {
            s += &format!(" error {}", e.replace(['\n', '\r'], " "));
        }
        s
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

#[cfg(unix)]
fn write_at(f: &std::fs::File, data: &[u8], off: u64) -> io::Result<()> {
    std::os::unix::fs::FileExt::write_all_at(f, data, off)
}

#[cfg(windows)]
fn write_at(f: &std::fs::File, mut data: &[u8], mut off: u64) -> io::Result<()> {
    while !data.is_empty() {
        let n = std::os::windows::fs::FileExt::seek_write(f, data, off)?;
        data = &data[n..];
        off += n as u64;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn part(hash: &[u8; 32], total: usize, off: usize, data: &[u8]) -> Vec<u8> {
        let mut m = wire::header(wire::BLOB_PART, 0).to_vec();
        m.extend_from_slice(hash);
        m.extend_from_slice(&(total as u32).to_be_bytes());
        m.extend_from_slice(&(off as u32).to_be_bytes());
        m.extend_from_slice(&data[off..(off + wire::ROSTER_CHUNK).min(total)]);
        m
    }

    #[test]
    fn a_stalled_download_resumes_where_it_stopped() {
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let relay = UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
        // Not a manifest: completing it only logs a rejection.
        let blob: Vec<u8> = (0..5 * wire::ROSTER_CHUNK as u32 + 7).map(|i| i as u8).collect();
        let hash = release::sha256(&blob);
        let mut u = Updater::bare([0; 32]);
        u.begin(&sock, relay, hash, Want::Manifest);
        for off in [0, wire::ROSTER_CHUNK, 3 * wire::ROSTER_CHUNK] {
            u.part(&sock, relay, &part(&hash, blob.len(), off, &blob));
        }

        // The relay's budget runs out: no progress for longer than ABANDON.
        u.dl.as_mut().unwrap().last_progress = Instant::now() - ABANDON - Duration::from_secs(1);
        u.tick(&sock);
        assert!(u.dl.is_none());
        assert_eq!(u.paused.as_ref().unwrap().got.iter().filter(|g| **g).count(), 3);

        // Another file does not throw it away; the same one picks it up.
        u.begin(&sock, relay, [1; 32], Want::Manifest);
        assert!(u.paused.is_some());
        u.dl = None;
        u.begin(&sock, relay, hash, Want::Manifest);
        assert!(u.paused.is_none());
        assert_eq!(u.dl.as_ref().unwrap().got.iter().filter(|g| **g).count(), 3);
        for off in [2 * wire::ROSTER_CHUNK, 4 * wire::ROSTER_CHUNK] {
            u.part(&sock, relay, &part(&hash, blob.len(), off, &blob));
        }
        assert!(u.dl.is_some(), "the last chunk is still missing");
        u.part(&sock, relay, &part(&hash, blob.len(), 5 * wire::ROSTER_CHUNK, &blob));
        assert!(u.dl.is_none(), "complete");
    }
}

#[cfg(test)]
mod module_tests {
    use super::*;

    fn part(hash: &[u8; 32], data: &[u8], off: usize) -> Vec<u8> {
        let mut m = wire::header(wire::BLOB_PART, 0).to_vec();
        m.extend_from_slice(hash);
        m.extend_from_slice(&(data.len() as u32).to_be_bytes());
        m.extend_from_slice(&(off as u32).to_be_bytes());
        m.extend_from_slice(&data[off..(off + wire::ROSTER_CHUNK).min(data.len())]);
        m
    }

    #[test]
    fn module_files_download_to_disk_after_meshd_is_current() {
        let tmp = |t: &str| {
            let d = std::env::temp_dir().join(format!("meshd-upd-{t}-{}", meshproto::random_u64()));
            std::fs::create_dir_all(&d).unwrap();
            d
        };
        let (sys, cache) = (tmp("sys"), tmp("cache"));
        let fname = if cfg!(windows) { "fmesh.exe" } else { "fmesh" };
        std::fs::write(sys.join(fname), b"old cli").unwrap();
        let new: Vec<u8> = (0..3 * wire::ROSTER_CHUNK as u32 + 5).map(|i| (i * 7) as u8).collect();
        let bin = Binary { module: "fmesh".into(), platform: release::platform(), sha256: release::sha256(&new), size: new.len() as u64 };

        let mut u = Updater::bare_in([0; 32], sys.clone());
        u.modules = Modules::new(sys.clone(), &sys).with_cache(cache.clone());
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let relay = UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
        u.latest_relay = Some(relay);
        u.modules.set_release(Release { version: 12, meshd: vec![], modules: vec![bin.clone()] });

        // meshd itself is behind: its own update goes first.
        u.installed = 11;
        u.tick(&sock);
        assert!(u.dl.is_none());

        u.installed = 12;
        u.tick(&sock);
        assert!(u.dl.as_ref().is_some_and(|d| d.is_module() && d.data.is_empty()), "written to a file, not memory");
        assert!(u.module_lines()[0].starts_with("module fmesh downloading release 12 progress 0/"), "{:?}", u.module_lines());
        for off in (0..new.len()).step_by(wire::ROSTER_CHUNK) {
            u.part(&sock, relay, &part(&bin.sha256, &new, off));
        }
        assert!(u.dl.is_none());
        assert!(u.module_lines()[0].starts_with("module fmesh ready release 12 sha256 "), "{:?}", u.module_lines());

        // A local request installs it.
        assert!(u.install_group("cli").is_some());
        assert_eq!(std::fs::read(sys.join(fname)).unwrap(), new);
        assert_eq!(u.module_lines(), vec!["module fmesh current release 12".to_string()]);
        std::fs::remove_dir_all(&sys).ok();
        std::fs::remove_dir_all(&cache).ok();
    }
}

#[cfg(test)]
mod status_tests {
    use super::*;

    #[test]
    fn update_line() {
        let mut u = Updater::bare([0; 32]);
        u.installed = 11;
        assert_eq!(u.status_line(), format!("update version {} release 11 state idle", env!("CARGO_PKG_VERSION")));
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        u.latest = Some((12, "sh".into(), Instant::now()));
        u.bad = 12;
        u.error = Some("installing v12 failed: denied\nsecond line".into());
        let _ = &sock;
        let line = u.status_line();
        assert!(line.contains(" latest 12 via sh age 0 state idle bad 12 error installing v12 failed: denied second line"), "{line}");
    }
}
