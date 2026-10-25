//! Bind a receiver to its own Claude session, including explicit background continuations.
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{Result, hub, local_control, state};

const TAIL_BYTES: u64 = 1024 * 1024;

struct Native {
    socket: PathBuf,
    token: String,
}

pub struct Runtime {
    /// The parent's current conversation. It follows a new conversation in the same Claude process
    /// (for example /clear), unless the previous one was explicitly continued elsewhere.
    id: Mutex<String>,
    /// The parent process's start stamp from its first record, so a reused pid cannot pass.
    origin: Mutex<Option<Value>>,
    pid: u32,
    config: PathBuf,
    project: PathBuf,
    native: Option<Native>,
    channel: bool,
    handoffs: PathBuf,
    continued: Mutex<Option<String>>,
    scans: Mutex<HashMap<String, Scan>>,
}

struct Scan {
    stamp: (u64, u64, u64, i64, i64),
    target: Option<String>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Link {
    source: String,
    target: String,
    project: PathBuf,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Handoff {
    source: String,
    target: String,
    project: PathBuf,
    pub name: String,
    pub description: String,
}

fn controlled(path: &Path, limit: u64) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file()
        || meta.uid() != unsafe { libc::geteuid() }
        || meta.mode() & 0o022 != 0
        || meta.len() > limit
    {
        return Err("Claude session metadata must be bounded and owner-controlled".into());
    }
    Ok(file)
}

fn parent_args(pid: u32) -> Vec<String> {
    #[cfg(target_os = "linux")]
    {
        let path = format!("/proc/{pid}/cmdline");
        let Ok(bytes) = std::fs::read(path) else {
            return Vec::new();
        };
        if bytes.len() > 1024 * 1024 {
            return Vec::new();
        }
        bytes
            .split(|b| *b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .collect()
    }
    #[cfg(target_os = "macos")]
    {
        let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid as i32];
        let mut bytes = vec![0u8; 1024 * 1024];
        let mut len = bytes.len();
        if unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                3,
                bytes.as_mut_ptr().cast(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        } != 0
            || len < 4
        {
            return Vec::new();
        }
        bytes.truncate(len);
        let count = i32::from_ne_bytes(bytes[..4].try_into().unwrap());
        if !(1..=10000).contains(&count) {
            return Vec::new();
        }
        let Some(end) = bytes[4..].iter().position(|b| *b == 0) else {
            return Vec::new();
        };
        bytes[4 + end..]
            .split(|b| *b == 0)
            .filter(|s| !s.is_empty())
            .take(count as usize)
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .collect()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = pid;
        Vec::new()
    }
}

fn channel_enabled(args: &[String]) -> bool {
    for (index, arg) in args.iter().enumerate() {
        if arg == "--" {
            break;
        }
        if arg == "--dangerously-load-development-channels"
            && args[index + 1..]
                .iter()
                .take_while(|arg| !arg.starts_with('-'))
                .any(|arg| arg == "server:agent-inbox-join")
        {
            return true;
        }
        if arg
            .strip_prefix("--dangerously-load-development-channels=")
            .is_some_and(|value| {
                value
                    .split_whitespace()
                    .any(|item| item == "server:agent-inbox-join")
            })
        {
            return true;
        }
    }
    false
}

fn connect_bounded(path: &Path) -> Result<UnixStream> {
    let bytes = path.as_os_str().as_bytes();
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    if bytes.is_empty() || bytes.contains(&0) || bytes.len() >= address.sun_path.len() {
        return Err("Claude inbox socket path is too long or invalid".into());
    }
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (slot, byte) in address.sun_path.iter_mut().zip(bytes) {
        *slot = *byte as libc::c_char;
    }
    let length =
        (std::mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1) as libc::socklen_t;
    #[cfg(target_os = "macos")]
    {
        address.sun_len = length as u8;
    }
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    if fd < 0 {
        return Err("cannot open Claude inbox socket".into());
    }
    let stream = unsafe { UnixStream::from_raw_fd(fd) };
    if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } != 0 {
        return Err("cannot protect Claude inbox socket descriptor".into());
    }
    stream.set_nonblocking(true)?;
    if unsafe { libc::connect(fd, (&address as *const libc::sockaddr_un).cast(), length) } != 0 {
        if std::io::Error::last_os_error().raw_os_error() != Some(libc::EINPROGRESS) {
            return Err("Claude inbox socket is unavailable".into());
        }
        let mut event = libc::pollfd {
            fd,
            events: libc::POLLOUT,
            revents: 0,
        };
        if unsafe { libc::poll(&mut event, 1, 200) } != 1 {
            return Err("Claude inbox connection timed out".into());
        }
        let mut error = 0i32;
        let mut len = std::mem::size_of_val(&error) as libc::socklen_t;
        if unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_ERROR,
                (&mut error as *mut i32).cast(),
                &mut len,
            )
        } != 0
            || error != 0
        {
            return Err("Claude inbox connection failed".into());
        }
    }
    stream.set_nonblocking(false)?;
    Ok(stream)
}

impl Runtime {
    pub fn from_env(sessions: &Path) -> Result<Option<Arc<Self>>> {
        let Some(id) = std::env::var_os("CLAUDE_CODE_SESSION_ID") else {
            return Ok(None);
        };
        let id = id
            .to_str()
            .filter(|id| state::is_id(id))
            .ok_or("invalid Claude session id")?
            .to_owned();
        let home = std::env::var_os("HOME").ok_or("HOME is unset")?;
        let config = std::env::var_os("CLAUDE_CONFIG_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(home).join(".claude"));
        let project = std::env::var_os("CLAUDE_PROJECT_DIR")
            .map(PathBuf::from)
            .unwrap_or(std::env::current_dir()?)
            .canonicalize()?;
        let native = match (
            std::env::var_os("CLAUDE_CODE_MESSAGING_SOCKET"),
            std::env::var("CLAUDE_CODE_MESSAGING_TOKEN").ok(),
        ) {
            (Some(socket), Some(token))
                if !socket.is_empty()
                    && (16..=4096).contains(&token.len())
                    && !token.contains(['\r', '\n']) =>
            {
                Some(Native {
                    socket: socket.into(),
                    token,
                })
            }
            (None, None) => None,
            _ => return Err("incomplete Claude native inbox environment".into()),
        };
        let pid = unsafe { libc::getppid() } as u32;
        Ok(Some(Arc::new(Self {
            id: Mutex::new(id),
            origin: Mutex::new(None),
            pid,
            config,
            project,
            native,
            channel: channel_enabled(&parent_args(pid)),
            handoffs: sessions.join("continuations"),
            continued: Mutex::new(None),
            scans: Mutex::new(HashMap::new()),
        })))
    }

    fn transcript(&self, id: &str) -> PathBuf {
        let slug: String = self
            .project
            .to_string_lossy()
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        self.config
            .join("projects")
            .join(slug)
            .join(format!("{id}.jsonl"))
    }

    fn continuation(&self, id: &str) -> Result<Option<String>> {
        let observed = self.handoffs.join(format!("observed-{id}.json"));
        if observed.exists() {
            local_control::private_dir(&self.handoffs)?;
            let link: Link = serde_json::from_reader(controlled(&observed, 64 * 1024)?)
                .map_err(|_| "invalid observed Claude continuation")?;
            if link.source != id
                || link.project != self.project
                || !state::is_id(&link.target)
                || link.target == id
            {
                return Err("observed Claude continuation identity mismatch".into());
            }
            return Ok(Some(link.target));
        }
        let path = self.transcript(id);
        let mut file = match controlled(&path, 1024 * 1024 * 1024) {
            Ok(file) => file,
            Err(_) if !path.exists() => return Ok(None),
            Err(error) => return Err(error),
        };
        let meta = file.metadata()?;
        let len = meta.len();
        let stamp = (meta.dev(), meta.ino(), len, meta.mtime(), meta.mtime_nsec());
        if let Some(scan) = self
            .scans
            .lock()
            .map_err(|_| "Claude lifecycle lock unavailable")?
            .get(id)
            && scan.stamp == stamp
        {
            return Ok(scan.target.clone());
        }
        let start = len.saturating_sub(TAIL_BYTES);
        file.seek(SeekFrom::Start(start))?;
        let mut reader = BufReader::new(file.take(TAIL_BYTES));
        let mut line = Vec::new();
        if start != 0 {
            reader.read_until(b'\n', &mut line)?;
        }
        let mut target = None;
        loop {
            line.clear();
            if reader.read_until(b'\n', &mut line)? == 0 {
                break;
            }
            // A writer's partial last record is not a completed handoff.
            if line.last() != Some(&b'\n') {
                break;
            }
            if !line
                .windows(b"continued-in".len())
                .any(|s| s == b"continued-in")
            {
                continue;
            }
            if let Ok(value) = serde_json::from_slice::<Value>(&line)
                && value["type"] == "continued-in"
                && value["sessionId"] == id
                && let Some(next) = value["continuedInSessionId"]
                    .as_str()
                    .filter(|next| state::is_id(next) && *next != id)
            {
                target = Some(next.to_owned());
            }
        }
        if let Some(target) = &target {
            local_control::private_dir(&self.handoffs)?;
            state::write_json(
                &observed,
                &Link {
                    source: id.to_owned(),
                    target: target.clone(),
                    project: self.project.clone(),
                },
            )?;
        }
        let mut scans = self
            .scans
            .lock()
            .map_err(|_| "Claude lifecycle lock unavailable")?;
        if scans.len() >= 16 {
            scans.clear();
        }
        scans.insert(
            id.to_owned(),
            Scan {
                stamp,
                target: target.clone(),
            },
        );
        Ok(target)
    }

    fn id(&self) -> Result<String> {
        Ok(self
            .id
            .lock()
            .map_err(|_| "Claude lifecycle lock unavailable")?
            .clone())
    }

    pub fn continued_to(&self) -> Result<Option<String>> {
        self.record()?;
        let id = self.id()?;
        let mut cached = self
            .continued
            .lock()
            .map_err(|_| "Claude lifecycle lock unavailable")?;
        if cached.is_none() {
            *cached = self.continuation(&id)?;
        }
        Ok(cached.clone())
    }

    fn record(&self) -> Result<Value> {
        let path = self
            .config
            .join("sessions")
            .join(format!("{}.json", self.pid));
        let value: Value = serde_json::from_reader(controlled(&path, 64 * 1024)?)
            .map_err(|_| "invalid Claude session metadata")?;
        let current = value["sessionId"]
            .as_str()
            .filter(|id| state::is_id(id))
            .ok_or("invalid Claude session metadata")?;
        if value["pid"].as_u64() != Some(self.pid as u64)
            || value["cwd"]
                .as_str()
                .and_then(|p| Path::new(p).canonicalize().ok())
                .as_ref()
                != Some(&self.project)
            || unsafe { libc::kill(self.pid as i32, 0) } != 0
        {
            return Err("Claude inbox does not belong to this receiver's parent session".into());
        }
        let stamp = json!([value["procStart"], value["startedAt"]]);
        let mut origin = self
            .origin
            .lock()
            .map_err(|_| "Claude lifecycle lock unavailable")?;
        if origin.get_or_insert_with(|| stamp.clone()) != &stamp {
            return Err("Claude inbox parent process was replaced".into());
        }
        drop(origin);
        let mut id = self
            .id
            .lock()
            .map_err(|_| "Claude lifecycle lock unavailable")?;
        // The same process started a new conversation in place. An explicit continuation keeps
        // the old binding so that its receiver retires and hands its name to the successor.
        if *id != current && self.continuation(&id)?.is_none() {
            *id = current.to_owned();
            *self
                .continued
                .lock()
                .map_err(|_| "Claude lifecycle lock unavailable")? = None;
        }
        Ok(value)
    }

    fn connect(&self) -> Result<UnixStream> {
        let native = self.native.as_ref().ok_or("no Claude native inbox")?;
        let record = self.record()?;
        if record["peerProtocol"] != 1
            || record["messagingSocketPath"].as_str().map(Path::new)
                != Some(native.socket.as_path())
        {
            return Err("Claude native inbox protocol or identity mismatch".into());
        }
        let parent = native.socket.parent().ok_or("invalid Claude inbox path")?;
        let dir = std::fs::symlink_metadata(parent)?;
        let socket = std::fs::symlink_metadata(&native.socket)?;
        let uid = unsafe { libc::geteuid() };
        if !native.socket.is_absolute()
            || !dir.is_dir()
            || dir.uid() != uid
            || dir.mode() & 0o022 != 0
            || !socket.file_type().is_socket()
            || socket.uid() != uid
            || socket.mode() & 0o077 != 0
        {
            return Err("Claude native inbox must be an owner-controlled Unix socket".into());
        }
        let stream = connect_bounded(&native.socket)?;
        stream.set_read_timeout(Some(Duration::from_secs(1)))?;
        stream.set_write_timeout(Some(Duration::from_secs(1)))?;
        #[cfg(target_os = "linux")]
        {
            let mut peer: libc::ucred = unsafe { std::mem::zeroed() };
            let mut len = std::mem::size_of_val(&peer) as libc::socklen_t;
            if unsafe {
                libc::getsockopt(
                    stream.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_PEERCRED,
                    (&mut peer as *mut libc::ucred).cast(),
                    &mut len,
                )
            } != 0
                || peer.uid != uid
                || peer.pid != self.pid as i32
            {
                return Err("Claude inbox peer identity mismatch".into());
            }
        }
        #[cfg(target_os = "macos")]
        {
            let mut peer = 0i32;
            let mut len = std::mem::size_of_val(&peer) as libc::socklen_t;
            if unsafe {
                libc::getsockopt(
                    stream.as_raw_fd(),
                    0,
                    libc::LOCAL_PEERPID,
                    (&mut peer as *mut i32).cast(),
                    &mut len,
                )
            } != 0
                || peer != self.pid as i32
            {
                return Err("Claude inbox peer identity mismatch".into());
            }
        }
        Ok(stream)
    }

    pub fn transport(&self) -> &'static str {
        if self.continued_to().ok().flatten().is_some() {
            "continued"
        } else if self.native.is_some() {
            "native"
        } else if self.channel {
            "channel"
        } else {
            "unavailable"
        }
    }

    pub fn ready(&self) -> bool {
        if !matches!(self.continued_to(), Ok(None)) {
            return false;
        }
        if self.native.is_some() {
            self.connect().is_ok()
        } else {
            self.channel && self.record().is_ok()
        }
    }

    pub fn uses_native(&self) -> bool {
        self.native.is_some()
    }

    pub fn send(&self, text: &str) -> Result<()> {
        if self.continued_to()?.is_some() {
            return Err("Claude conversation continued elsewhere".into());
        }
        let mut stream = self.connect()?;
        let native = self.native.as_ref().unwrap();
        // Only the current parent's inherited token is used, never another session's credentials.
        for value in [
            json!({"type":"auth","token":native.token}),
            json!({"type":"user","message":{"role":"user","content":text}}),
        ] {
            let mut line = serde_json::to_vec(&value)?;
            line.push(b'\n');
            stream.write_all(&line)?;
        }
        stream.shutdown(std::net::Shutdown::Write)?;
        Ok(())
    }

    pub fn publish(&self, target: &str, name: &str, description: &str) -> Result<()> {
        if self.continued_to()?.as_deref() != Some(target) {
            return Err("no explicit Claude continuation".into());
        }
        hub::validate_component(name)?;
        crate::directory::description(description)?;
        local_control::private_dir(&self.handoffs)?;
        let path = self.handoffs.join(format!("{target}.json"));
        if path.exists() {
            let existing: Handoff = serde_json::from_reader(controlled(&path, 64 * 1024)?)
                .map_err(|_| "invalid existing Claude continuation metadata")?;
            if existing.source != self.id()?
                || existing.target != target
                || existing.project != self.project
            {
                return Err("Claude continuation metadata conflicts with another handoff".into());
            }
            // A reconnect of the retired receiver must not overwrite its original custom name.
            return Ok(());
        }
        state::write_json(
            &path,
            &Handoff {
                source: self.id()?,
                target: target.to_owned(),
                project: self.project.clone(),
                name: name.to_owned(),
                description: description.to_owned(),
            },
        )
    }

    pub fn inherited(&self) -> Result<Option<Handoff>> {
        let id = self.id()?;
        let path = self.handoffs.join(format!("{id}.json"));
        if !path.exists() {
            return Ok(None);
        }
        local_control::private_dir(&self.handoffs)?;
        let handoff: Handoff = serde_json::from_reader(controlled(&path, 64 * 1024)?)
            .map_err(|_| "invalid Claude continuation metadata")?;
        if !state::is_id(&handoff.source)
            || handoff.target != id
            || handoff.project != self.project
            || self.continuation(&handoff.source)?.as_deref() != Some(id.as_str())
        {
            return Err("Claude continuation metadata does not match this conversation".into());
        }
        hub::validate_component(&handoff.name)?;
        crate::directory::description(&handoff.description)?;
        Ok(Some(handoff))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    struct Fixture {
        root: PathBuf,
        runtime: Runtime,
    }
    impl Fixture {
        fn new() -> Self {
            let root = PathBuf::from("/tmp").join(format!("ai-runtime-{}", state::id().unwrap()));
            local_control::private_dir(&root).unwrap();
            let project = root.canonicalize().unwrap();
            std::fs::create_dir(root.join("sessions")).unwrap();
            let runtime = Runtime {
                id: Mutex::new(state::id().unwrap()),
                origin: Mutex::new(None),
                pid: std::process::id(),
                config: root.clone(),
                project,
                native: Some(Native {
                    socket: root.join("native.sock"),
                    token: "runtime-test-own-child-token-0000".into(),
                }),
                channel: false,
                handoffs: root.join("handoffs"),
                continued: Mutex::new(None),
                scans: Mutex::new(HashMap::new()),
            };
            state::write_json(
                &root.join("sessions").join(format!("{}.json", runtime.pid)),
                &json!({
                    "pid":runtime.pid,"sessionId":runtime.id().unwrap(),"cwd":runtime.project,"peerProtocol":1,
                    "messagingSocketPath":runtime.native.as_ref().unwrap().socket,
                }),
            )
            .unwrap();
            let path = runtime.transcript(&runtime.id().unwrap());
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "").unwrap();
            Self { root, runtime }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn channel_flag_must_name_this_server_and_not_be_prompt_text() {
        let args = |items: &[&str]| items.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert!(channel_enabled(&args(&[
            "claude",
            "--dangerously-load-development-channels",
            "server:other",
            "server:agent-inbox-join"
        ])));
        assert!(channel_enabled(&args(&[
            "claude",
            "--dangerously-load-development-channels=server:agent-inbox-join"
        ])));
        assert!(!channel_enabled(&args(&[
            "claude",
            "--dangerously-load-development-channels",
            "server:other"
        ])));
        assert!(!channel_enabled(&args(&[
            "claude",
            "--",
            "--dangerously-load-development-channels",
            "server:agent-inbox-join"
        ])));
    }

    #[test]
    fn native_socket_is_checked_against_parent_session_protocol_and_file_identity() {
        let f = Fixture::new();
        let socket = &f.runtime.native.as_ref().unwrap().socket;
        let _listener = std::os::unix::net::UnixListener::bind(socket).unwrap();
        std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(f.runtime.ready());
        std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o666)).unwrap();
        assert!(!f.runtime.ready());
        std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600)).unwrap();
        let record_path = f
            .root
            .join("sessions")
            .join(format!("{}.json", f.runtime.pid));
        let mut record: Value = state::read_json(&record_path).unwrap();
        record["pid"] = json!(f.runtime.pid + 1);
        state::write_json(&record_path, &record).unwrap();
        assert!(!f.runtime.ready());
        record["pid"] = json!(f.runtime.pid);
        record["cwd"] = json!(f.root.join("sessions"));
        state::write_json(&record_path, &record).unwrap();
        assert!(!f.runtime.ready());
        record["cwd"] = json!(f.runtime.project);
        record["peerProtocol"] = json!(2);
        state::write_json(&record_path, &record).unwrap();
        assert!(!f.runtime.ready());
        record["peerProtocol"] = json!(1);
        state::write_json(&record_path, &record).unwrap();
        let actual = f.root.join("actual.sock");
        std::fs::rename(socket, &actual).unwrap();
        symlink(&actual, socket).unwrap();
        assert!(!f.runtime.ready());
    }

    #[test]
    fn continuation_requires_complete_matching_record_and_survives_transcript_tail_growth() {
        let f = Fixture::new();
        let target = state::id().unwrap();
        let path = f.runtime.transcript(&f.runtime.id().unwrap());
        std::fs::write(&path, format!("{}\n", json!({"type":"continued-in","sessionId":state::id().unwrap(),"continuedInSessionId":target}))).unwrap();
        assert!(f.runtime.continued_to().unwrap().is_none());
        std::fs::write(
            &path,
            json!({"type":"continued-in","sessionId":f.runtime.id().unwrap(),"continuedInSessionId":target})
                .to_string(),
        )
        .unwrap();
        assert!(f.runtime.continued_to().unwrap().is_none());
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(file).unwrap();
        assert_eq!(
            f.runtime.continued_to().unwrap().as_deref(),
            Some(target.as_str())
        );
        std::fs::write(&path, "x".repeat(TAIL_BYTES as usize + 1)).unwrap();
        *f.runtime.continued.lock().unwrap() = None;
        assert_eq!(
            f.runtime.continued_to().unwrap().as_deref(),
            Some(target.as_str())
        );
    }

    #[test]
    fn handoff_is_scoped_to_an_explicit_same_project_successor_and_keeps_original_name() {
        let mut f = Fixture::new();
        let source = state::id().unwrap();
        local_control::private_dir(&f.runtime.handoffs).unwrap();
        let path = f
            .runtime
            .handoffs
            .join(format!("{}.json", f.runtime.id().unwrap()));
        let handoff = Handoff {
            source: source.clone(),
            target: f.runtime.id().unwrap(),
            project: f.runtime.project.clone(),
            name: "original-owner".into(),
            description: "Owns work".into(),
        };
        state::write_json(&path, &handoff).unwrap();
        assert!(
            f.runtime.inherited().is_err(),
            "a made-up metadata file is not a conversation continuation"
        );
        let transcript = f.runtime.transcript(&source);
        std::fs::write(&transcript, format!("{}\n",json!({"type":"continued-in","sessionId":source,"continuedInSessionId":f.runtime.id().unwrap()}))).unwrap();
        assert_eq!(
            f.runtime.inherited().unwrap().unwrap().name,
            "original-owner"
        );
        f.runtime.project = f.root.join("unrelated-project");
        assert!(f.runtime.inherited().is_err());
    }

    #[test]
    fn a_new_conversation_in_the_same_process_keeps_the_receiver() {
        let f = Fixture::new();
        let socket = &f.runtime.native.as_ref().unwrap().socket;
        let _listener = std::os::unix::net::UnixListener::bind(socket).unwrap();
        std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600)).unwrap();
        let record_path = f
            .root
            .join("sessions")
            .join(format!("{}.json", f.runtime.pid));
        let mut record: Value = state::read_json(&record_path).unwrap();
        let cleared = state::id().unwrap();
        record["sessionId"] = json!(cleared);
        state::write_json(&record_path, &record).unwrap();
        assert!(f.runtime.ready());
        assert_eq!(f.runtime.id().unwrap(), cleared);
        assert_eq!(f.runtime.transport(), "native");
        record["procStart"] = json!("a reused pid");
        state::write_json(&record_path, &record).unwrap();
        assert!(!f.runtime.ready());
    }

    #[test]
    fn an_explicit_continuation_is_not_mistaken_for_a_new_conversation() {
        let f = Fixture::new();
        let original = f.runtime.id().unwrap();
        let target = state::id().unwrap();
        std::fs::write(
            f.runtime.transcript(&original),
            format!(
                "{}\n",
                json!({"type":"continued-in","sessionId":original,"continuedInSessionId":target})
            ),
        )
        .unwrap();
        let record_path = f
            .root
            .join("sessions")
            .join(format!("{}.json", f.runtime.pid));
        let mut record: Value = state::read_json(&record_path).unwrap();
        record["sessionId"] = json!(state::id().unwrap());
        state::write_json(&record_path, &record).unwrap();
        assert_eq!(
            f.runtime.continued_to().unwrap().as_deref(),
            Some(target.as_str())
        );
        assert_eq!(f.runtime.id().unwrap(), original);
        assert!(!f.runtime.ready());
    }
}
