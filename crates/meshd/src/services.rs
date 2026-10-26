//! Local service registration: a program on this machine announces a service to the mesh by
//! keeping a small file in `DIR`, and withdraws it by deleting the file or letting it go stale.
//!
//! One file per service, named after it (`[a-z0-9-]{1,32}`), with one line:
//! `port <N> [ident <printable ASCII, no spaces, at most 64>]`. The file's mtime is the
//! heartbeat: rewrite or touch it at least every 30 s; after `STALE` without one the service
//! is gone, so a crashed program needs no cleanup. No socket, no extra thread: meshd reads the
//! directory on its status timer.
//!
//! On Unix the directory is root's, mode 1733: anyone can create a file, nobody can list the
//! directory or replace another user's file. The owner uid (taken from the file, not its
//! contents) is shown in the local status only; it is never sent to peers. A registration
//! claims a service for this node; it does not prove which process listens on the port.

use meshproto::service::{self, Service};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

#[cfg(unix)]
pub const DIR: &str = "/var/run/flytomesh.services";
#[cfg(windows)]
pub const DIR: &str = r"C:\ProgramData\flytoMesh\services";

/// A registration without a heartbeat for this long is ignored.
pub const STALE: Duration = Duration::from_secs(90);
/// Registration files larger than this are ignored (one short line is expected).
const MAX_FILE: u64 = 256;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Local {
    pub svc: Service,
    /// Owner of the registration file (Unix only).
    pub uid: Option<u32>,
    /// Time since the last heartbeat.
    pub age: Duration,
}

/// Creates the registration directory (or fixes its mode). A non-directory at that path, e.g.
/// a symlink, is an error rather than something to follow.
pub fn prepare(dir: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(dir) {
        Ok(m) if m.is_dir() => {}
        Ok(_) => return Err(std::io::Error::other(format!("{} is not a directory", dir.display()))),
        Err(_) => std::fs::create_dir_all(dir)?,
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o1733))?;
    }
    Ok(())
}

/// Parses one registration line: `port <N> [ident <x>]`.
pub fn parse(name: &str, text: &str) -> Option<Service> {
    let t: Vec<&str> = text.split_whitespace().collect();
    let (port, ident) = match t.as_slice() {
        ["port", p] => (p, ""),
        ["port", p, "ident", i] => (p, *i),
        _ => return None,
    };
    let svc = Service { name: name.to_string(), port: port.parse().ok().filter(|&p| p != 0)?, ident: ident.to_string() };
    (service::valid_name(&svc.name) && service::valid_ident(&svc.ident)).then_some(svc)
}

/// Reads one registration file without following links; `None` if it is not a plain, small,
/// fresh file with a valid line.
fn read(path: &Path, name: &str, now: SystemTime) -> Option<Local> {
    let m = std::fs::symlink_metadata(path).ok()?;
    if !m.is_file() || m.len() > MAX_FILE {
        return None;
    }
    let age = now.duration_since(m.modified().ok()?).unwrap_or_default();
    if age > STALE {
        return None;
    }
    #[cfg(unix)]
    let uid = {
        use std::os::unix::fs::MetadataExt;
        // A hard link to someone else's file would borrow its owner.
        if m.nlink() != 1 {
            return None;
        }
        Some(m.uid())
    };
    #[cfg(windows)]
    let uid = None;
    let mut f = std::fs::OpenOptions::new();
    f.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        f.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let mut text = String::new();
    use std::io::Read;
    f.open(path).ok()?.take(MAX_FILE).read_to_string(&mut text).ok()?;
    Some(Local { svc: parse(name, &text)?, uid, age })
}

/// The current registrations, sorted by name, at most `service::MAX_SERVICES`.
pub fn scan(dir: &Path, now: SystemTime) -> Vec<Local> {
    let Ok(entries) = std::fs::read_dir(dir) else { return vec![] };
    let mut out: Vec<Local> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            service::valid_name(&name).then(|| read(&e.path(), &name, now))?
        })
        .collect();
    out.sort_by(|a, b| a.svc.name.cmp(&b.svc.name));
    out.truncate(service::MAX_SERVICES);
    out
}

/// Request files: `.request-<anything>` in the same directory, one word per line (`check`:
/// ask the relays for the current release now). Any local user can write one; meshd reads and
/// deletes them on its status timer. A request only makes meshd do sooner what it does anyway,
/// so it needs no authorization. Read like registrations: plain small files, no links.
pub const REQUEST_PREFIX: &str = ".request-";
const MAX_REQUESTS: usize = 16;

pub fn take_requests(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else { return vec![] };
    let mut out = vec![];
    for e in entries.flatten().filter(|e| e.file_name().to_string_lossy().starts_with(REQUEST_PREFIX)).take(MAX_REQUESTS) {
        let path = e.path();
        if let Ok(m) = std::fs::symlink_metadata(&path) {
            #[cfg(unix)]
            let single = {
                use std::os::unix::fs::MetadataExt;
                m.nlink() == 1
            };
            #[cfg(not(unix))]
            let single = true;
            if m.is_file() && single && m.len() <= MAX_FILE {
                if let Ok(text) = std::fs::read_to_string(&path) {
                    out.extend(text.split_whitespace().filter(|w| service::valid_name(w)).map(str::to_string));
                }
            }
        }
        std::fs::remove_file(&path).ok();
    }
    out.sort();
    out.dedup();
    out
}

pub fn dir() -> PathBuf {
    PathBuf::from(DIR)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        let d = std::env::temp_dir().join(format!("meshd-svc-{}", meshproto::random_u64()));
        prepare(&d).unwrap();
        d
    }

    #[test]
    fn parses_registration_lines() {
        assert_eq!(parse("agent-inbox", "port 8832 ident sha256:ab\n").unwrap().ident, "sha256:ab");
        assert_eq!(parse("x", "port 1").unwrap().port, 1);
        for bad in ["", "port", "port 0", "port 70000", "port 1 ident", "port 1 ident a b", "8832", "port 1 extra x"] {
            assert!(parse("x", bad).is_none(), "{bad:?}");
        }
        assert!(parse("Bad", "port 1").is_none());
    }

    #[test]
    fn scan_skips_stale_bad_and_linked_files() {
        let d = tmp();
        std::fs::write(d.join("agent-inbox"), "port 8832 ident k1\n").unwrap();
        std::fs::write(d.join("garbage"), "hello\n").unwrap();
        std::fs::write(d.join("Upper"), "port 1\n").unwrap();
        std::fs::write(d.join("big"), format!("port 1 ident {}\n", "x".repeat(300))).unwrap();
        let now = SystemTime::now();
        let got = scan(&d, now);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].svc, Service { name: "agent-inbox".into(), port: 8832, ident: "k1".into() });
        // No heartbeat for longer than STALE: gone.
        assert!(scan(&d, now + STALE + Duration::from_secs(1)).is_empty());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&d).unwrap().permissions().mode() & 0o7777, 0o1733);
            assert_eq!(got[0].uid, Some(unsafe { libc::getuid() }));
            let outside = d.with_extension("target");
            std::fs::write(&outside, "port 9 ident root\n").unwrap();
            std::os::unix::fs::symlink(&outside, d.join("linked")).unwrap();
            std::fs::hard_link(&outside, d.join("hard")).unwrap();
            assert_eq!(scan(&d, now).len(), 1, "symlinks and hard links are ignored");
            std::fs::remove_file(&outside).ok();
        }
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn requests_are_read_once_and_ignored_as_services() {
        let d = tmp();
        std::fs::write(d.join(".request-a1"), "check\n").unwrap();
        std::fs::write(d.join(".request-b2"), "check BAD\n").unwrap();
        std::fs::write(d.join("agent-inbox"), "port 8832\n").unwrap();
        assert_eq!(scan(&d, SystemTime::now()).len(), 1, "a request is not a service");
        assert_eq!(take_requests(&d), vec!["check".to_string()]);
        assert!(take_requests(&d).is_empty(), "consumed");
        assert!(d.join("agent-inbox").exists());
        std::fs::remove_dir_all(&d).ok();
    }

    #[cfg(unix)]
    #[test]
    fn prepare_refuses_a_symlink() {
        let d = tmp();
        let link = d.with_extension("link");
        std::os::unix::fs::symlink(&d, &link).unwrap();
        assert!(prepare(&link).is_err());
        std::fs::remove_file(&link).ok();
        std::fs::remove_dir_all(&d).ok();
    }
}
