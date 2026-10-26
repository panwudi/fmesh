//! Files of the other fMesh modules, carried in the signed release manifest (`#fmesh module`
//! lines). meshd fetches only what this machine has, after its own update and through the same
//! relays, into a cache; nothing is installed until asked.
//!
//! - Desktop (`meshtray`, `fmesh-desktop`) and CLI (`fmesh`) live next to meshd in its own
//!   directory, so meshd installs them itself: on a local request (`install-desktop`,
//!   `install-cli`, see services.rs) or automatically for the groups listed in
//!   `<dir>/auto-update` (written by root). Same trust as meshd's own update.
//! - Agent Mesh (`agent-inbox`) belongs to a user. meshd only caches its file when an
//!   agent-inbox service is registered here; the user's own `fmesh update agent` installs it.
//!
//! Every cached file is checked against the manifest hash before it is published under its
//! final name and again right before it is installed.

use meshproto::release::{self, Binary, Release};
use std::collections::{BTreeMap, HashMap};
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// World readable: users install their agent-inbox from here. Files are named by SHA-256.
#[cfg(target_os = "macos")]
pub const CACHE: &str = "/Library/Caches/flytomesh";
#[cfg(target_os = "linux")]
pub const CACHE: &str = "/var/cache/flytomesh";
#[cfg(windows)]
pub const CACHE: &str = r"C:\ProgramData\flytoMesh\cache";

/// Install groups and their files.
pub const GROUPS: [(&str, &[&str]); 3] = [("desktop", &["meshtray", "fmesh-desktop"]), ("cli", &["fmesh"]), ("agent", &["agent-inbox"])];

fn exe_name(module: &str) -> String {
    if cfg!(windows) {
        format!("{module}.exe")
    } else {
        module.to_string()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum State {
    /// The installed file is this release's.
    Current,
    /// Verified in the cache, waiting to be installed.
    Ready,
    /// Not fetched yet (meshd's own update goes first).
    Waiting,
    Downloading,
    Failed(String),
}

#[derive(Clone, Debug)]
pub struct Entry {
    pub bin: Binary,
    pub state: State,
}

pub struct Modules {
    cache: PathBuf,
    /// meshd's own directory, where the desktop and CLI binaries live.
    system_dir: PathBuf,
    /// `<dir>/auto-update`: groups installed without asking.
    auto_file: PathBuf,
    release: Option<Release>,
    pub agent_present: bool,
    pub entries: BTreeMap<String, Entry>,
    /// Hashes of installed files by (path, size, mtime), so an 11 MB file is not re-read.
    hashes: HashMap<PathBuf, (u64, Option<SystemTime>, [u8; 32])>,
}

impl Modules {
    pub fn new(system_dir: PathBuf, dir: &Path) -> Modules {
        Modules {
            cache: PathBuf::from(CACHE),
            system_dir,
            auto_file: dir.join("auto-update"),
            release: None,
            agent_present: false,
            entries: BTreeMap::new(),
            hashes: HashMap::new(),
        }
    }

    #[cfg(test)]
    pub fn with_cache(mut self, cache: PathBuf) -> Modules {
        self.cache = cache;
        self
    }

    pub fn release_version(&self) -> Option<u64> {
        self.release.as_ref().map(|r| r.version)
    }

    /// A newer verified manifest: forget the old entries and cached files it no longer lists.
    pub fn set_release(&mut self, rel: Release) {
        if self.release.as_ref().is_some_and(|r| r.version >= rel.version) {
            return;
        }
        let keep: Vec<String> = rel.modules.iter().map(|b| release::hex(&b.sha256)).collect();
        if let Ok(list) = std::fs::read_dir(&self.cache) {
            for e in list.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                let stem = name.trim_end_matches(".part");
                if !keep.iter().any(|k| k == stem) {
                    std::fs::remove_file(e.path()).ok();
                }
            }
        }
        self.release = Some(rel);
        self.entries.clear();
        self.refresh();
    }

    /// Modules this machine has, by the files in meshd's directory and a registered
    /// agent-inbox service.
    fn wanted(&self) -> Vec<&'static str> {
        let mut out = vec![];
        for (group, files) in GROUPS {
            for f in files {
                let present = if group == "agent" { self.agent_present } else { self.system_dir.join(exe_name(f)).is_file() };
                if present {
                    out.push(*f);
                }
            }
        }
        out
    }

    fn installed_hash(&mut self, path: &Path) -> Option<[u8; 32]> {
        let m = std::fs::metadata(path).ok()?;
        let key = (m.len(), m.modified().ok());
        if let Some((len, mtime, h)) = self.hashes.get(path) {
            if (*len, *mtime) == key {
                return Some(*h);
            }
        }
        let h = release::sha256_file(path).ok()?;
        self.hashes.insert(path.to_path_buf(), (key.0, key.1, h));
        Some(h)
    }

    pub fn cached(&self, bin: &Binary) -> PathBuf {
        self.cache.join(release::hex(&bin.sha256))
    }

    /// Recomputes every state from the files (cheap after the first time).
    pub fn refresh(&mut self) {
        let Some(rel) = self.release.clone() else { return };
        let platform = release::platform();
        let wanted = self.wanted();
        self.entries.retain(|k, _| wanted.contains(&k.as_str()));
        for name in wanted {
            let Some(bin) = rel.module(name, &platform).cloned() else { continue };
            let prev = self.entries.get(name).map(|e| e.state.clone());
            let installed = if name == "agent-inbox" { None } else { self.installed_hash(&self.system_dir.join(exe_name(name))) };
            let cached = std::fs::metadata(self.cached(&bin)).is_ok_and(|m| m.len() == bin.size);
            let state = if installed == Some(bin.sha256) {
                State::Current
            } else if cached {
                State::Ready
            } else {
                match prev {
                    Some(s @ (State::Downloading | State::Failed(_))) => s,
                    _ => State::Waiting,
                }
            };
            self.entries.insert(name.to_string(), Entry { bin, state });
        }
    }

    /// The next file to fetch.
    pub fn next_download(&self) -> Option<(String, Binary)> {
        self.entries.iter().find(|(_, e)| e.state == State::Waiting).map(|(n, e)| (n.clone(), e.bin.clone()))
    }

    /// Failed files are tried again (called every few minutes).
    pub fn retry_failed(&mut self) {
        for e in self.entries.values_mut() {
            if matches!(e.state, State::Failed(_)) {
                e.state = State::Waiting;
            }
        }
        self.refresh();
    }

    pub fn set_state(&mut self, name: &str, state: State) {
        if let Some(e) = self.entries.get_mut(name) {
            e.state = state;
        }
    }

    /// Where a download is written while it is incomplete.
    pub fn part_path(&self, bin: &Binary) -> io::Result<PathBuf> {
        std::fs::create_dir_all(&self.cache)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&self.cache, std::fs::Permissions::from_mode(0o755))?;
        }
        Ok(self.cache.join(format!("{}.part", release::hex(&bin.sha256))))
    }

    /// A finished download: publish it under its final name if it hashes right.
    pub fn finished(&mut self, name: &str, bin: &Binary) -> Result<(), String> {
        let part = self.cache.join(format!("{}.part", release::hex(&bin.sha256)));
        let ok = release::sha256_file(&part).is_ok_and(|h| h == bin.sha256);
        if !ok {
            std::fs::remove_file(&part).ok();
            self.set_state(name, State::Failed("downloaded file does not match the manifest".into()));
            return Err(format!("{name}: downloaded file does not match the manifest"));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&part, std::fs::Permissions::from_mode(0o644)).ok();
        }
        std::fs::rename(&part, self.cached(bin)).map_err(|e| e.to_string())?;
        self.set_state(name, State::Ready);
        Ok(())
    }

    /// Groups installed without asking (`<dir>/auto-update`, one group per line).
    pub fn auto_groups(&self) -> Vec<String> {
        std::fs::read_to_string(&self.auto_file).unwrap_or_default().split_whitespace().map(str::to_string).collect()
    }

    /// Installs every ready file of `group` into meshd's directory. Agent files are never
    /// installed by meshd. Returns what happened, for the log.
    pub fn install(&mut self, group: &str) -> Result<Vec<String>, String> {
        let Some((_, files)) = GROUPS.iter().find(|(g, _)| *g == group) else { return Err(format!("unknown group {group}")) };
        if group == "agent" {
            return Err("agent-inbox is installed by its user (fmesh update agent)".into());
        }
        let mut done = vec![];
        for f in *files {
            let Some(e) = self.entries.get(*f).cloned() else { continue };
            if e.state != State::Ready {
                continue;
            }
            let target = self.system_dir.join(exe_name(f));
            match replace(&self.cached(&e.bin), &e.bin, &target) {
                Ok(()) => done.push(format!("{f} -> {}", target.display())),
                Err(err) => {
                    self.set_state(f, State::Failed(format!("install: {err}")));
                    return Err(format!("{f}: {err}"));
                }
            }
        }
        self.refresh();
        Ok(done)
    }

    /// `module <name> <state> release <n> [progress <a>/<b>] [sha256 <hex>] [path <cached>]
    /// [error <text>]`, one per module this machine has (docs/network.md section 5).
    pub fn status_lines(&self, progress: Option<(&str, usize, usize)>) -> Vec<String> {
        let version = self.release_version().unwrap_or(0);
        self.entries
            .iter()
            .map(|(name, e)| {
                let state = match &e.state {
                    State::Current => "current",
                    State::Ready => "ready",
                    State::Waiting => "waiting",
                    State::Downloading => "downloading",
                    State::Failed(_) => "failed",
                };
                let mut s = format!("module {name} {state} release {version}");
                if let Some((_, a, b)) = progress.filter(|(n, _, _)| n == name) {
                    s += &format!(" progress {a}/{b}");
                }
                if e.state == State::Ready {
                    s += &format!(" sha256 {} path {}", release::hex(&e.bin.sha256), self.cached(&e.bin).display());
                }
                if let State::Failed(err) = &e.state {
                    s += &format!(" error {}", err.replace(['\n', '\r'], " "));
                }
                s
            })
            .collect()
    }
}

/// Copies a verified cached file over `target`: hash it again, write `<target>.fmesh-new`, then
/// rename. A running executable cannot be replaced on Windows, only renamed, so the old one is
/// moved aside first; the tray notices and restarts itself.
fn replace(cached: &Path, bin: &Binary, target: &Path) -> io::Result<()> {
    if release::sha256_file(cached)? != bin.sha256 {
        return Err(io::Error::other("cached file changed since it was verified"));
    }
    let mut new = target.as_os_str().to_owned();
    new.push(".fmesh-new");
    let new = PathBuf::from(new);
    std::fs::copy(cached, &new)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&new, std::fs::Permissions::from_mode(0o755))?;
    }
    // Leftovers of earlier replacements whose process has exited (Windows).
    if let (Some(dir), Some(name)) = (target.parent(), target.file_name()) {
        let prefix = format!("{}.stale-", name.to_string_lossy());
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            if e.file_name().to_string_lossy().starts_with(&prefix) {
                std::fs::remove_file(e.path()).ok();
            }
        }
    }
    if target.exists() {
        #[cfg(windows)]
        {
            let mut stale = target.as_os_str().to_owned();
            stale.push(format!(".stale-{}", meshproto::random_u64()));
            std::fs::rename(target, PathBuf::from(stale))?;
        }
    }
    std::fs::rename(&new, target)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("meshd-modules-{tag}-{}", meshproto::random_u64()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn rel(files: &[(&str, &[u8])]) -> Release {
        let p = release::platform();
        Release {
            version: 12,
            meshd: vec![],
            modules: files
                .iter()
                .map(|(m, d)| Binary { module: m.to_string(), platform: p.clone(), sha256: release::sha256(d), size: d.len() as u64 })
                .collect(),
        }
    }

    #[test]
    fn only_present_modules_are_fetched_and_installed_when_asked() {
        let (sys, cache) = (tmp("sys"), tmp("cache"));
        std::fs::write(sys.join(exe_name("meshtray")), b"old tray").unwrap();
        std::fs::write(cache.join("stale-file"), b"x").unwrap();
        let mut m = Modules::new(sys.clone(), &sys).with_cache(cache.clone());
        m.set_release(rel(&[("meshtray", b"new tray"), ("fmesh", b"cli"), ("agent-inbox", b"agent")]));
        assert!(!cache.join("stale-file").exists(), "files of other releases are removed");
        // Only the tray is on this machine; no agent registration.
        let (name, bin) = m.next_download().unwrap();
        assert_eq!(name, "meshtray");
        assert_eq!(m.entries.len(), 1);
        assert_eq!(m.status_lines(None), vec!["module meshtray waiting release 12".to_string()]);

        // Download it (written as .part), then publish.
        std::fs::write(m.part_path(&bin).unwrap(), b"new tray").unwrap();
        m.finished(&name, &bin).unwrap();
        assert_eq!(m.entries["meshtray"].state, State::Ready);
        assert!(m.status_lines(None)[0].starts_with("module meshtray ready release 12 sha256 "));
        assert!(m.next_download().is_none());

        // Agent files are not meshd's to install; the tray is.
        assert!(m.install("agent").is_err());
        assert_eq!(m.install("desktop").unwrap().len(), 1);
        assert_eq!(std::fs::read(sys.join(exe_name("meshtray"))).unwrap(), b"new tray");
        assert_eq!(m.entries["meshtray"].state, State::Current);

        // An agent registration makes its file wanted.
        m.agent_present = true;
        m.refresh();
        assert_eq!(m.next_download().unwrap().0, "agent-inbox");
        std::fs::remove_dir_all(&sys).ok();
        std::fs::remove_dir_all(&cache).ok();
    }

    #[test]
    fn a_bad_download_is_not_published() {
        let (sys, cache) = (tmp("sys"), tmp("cache"));
        std::fs::write(sys.join(exe_name("fmesh")), b"old").unwrap();
        let mut m = Modules::new(sys.clone(), &sys).with_cache(cache.clone());
        m.set_release(rel(&[("fmesh", b"good")]));
        let (name, bin) = m.next_download().unwrap();
        std::fs::write(m.part_path(&bin).unwrap(), b"evil").unwrap();
        assert!(m.finished(&name, &bin).is_err());
        assert!(!m.cached(&bin).exists());
        assert!(matches!(m.entries["fmesh"].state, State::Failed(_)));
        assert!(m.status_lines(None)[0].contains(" error "));
        std::fs::remove_dir_all(&sys).ok();
        std::fs::remove_dir_all(&cache).ok();
    }
}
