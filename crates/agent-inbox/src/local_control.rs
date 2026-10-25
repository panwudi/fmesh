//! Same-user local control. No TCP listener and no credentials in diagnostics.
use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};

use crate::{Result, state};
use serde_json::{Value, json};

const TIMEOUT: Duration = Duration::from_millis(200);
const DIRECTORY_LIMIT: usize = 1024 * 1024;

pub fn private_dir(path: &Path) -> Result<()> {
    state::parent(&path.join("placeholder"))?;
    let meta = std::fs::symlink_metadata(path)?;
    if !meta.is_dir()
        || meta.uid() != unsafe { libc::geteuid() }
        || meta.permissions().mode() & 0o777 != 0o700
    {
        return Err(
            "local control/session directory must be owned by this user and mode 700".into(),
        );
    }
    Ok(())
}

pub fn default_path(port: u16) -> Result<PathBuf> {
    Ok(crate::journal::default_path()?
        .with_file_name("local-control")
        .join(format!("relay-{port}.sock")))
}

fn uid_matches(peer: libc::uid_t, owner: libc::uid_t) -> Result<()> {
    if peer != owner {
        return Err("local control requires the same OS user".into());
    }
    Ok(())
}

pub fn same_user(stream: &UnixStream) -> Result<()> {
    #[cfg(any(
        target_os = "macos",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd"
    ))]
    let uid = {
        let mut uid = 0;
        let mut gid = 0;
        if unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) } != 0 {
            return Err("cannot verify local control peer UID".into());
        }
        uid
    };
    #[cfg(target_os = "linux")]
    let uid = {
        let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
        let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        if unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut credentials as *mut libc::ucred).cast(),
                &mut length,
            )
        } != 0
            || length as usize != std::mem::size_of::<libc::ucred>()
        {
            return Err("cannot verify local control peer UID".into());
        }
        credentials.uid
    };
    #[cfg(not(any(
        target_os = "macos",
        target_os = "linux",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd"
    )))]
    return Err("local control peer credentials are unsupported on this platform".into());
    #[cfg(any(
        target_os = "macos",
        target_os = "linux",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd"
    ))]
    uid_matches(uid, unsafe { libc::geteuid() })
}

fn connect(path: &Path, deadline: Instant) -> Result<UnixStream> {
    let bytes = path.as_os_str().as_bytes();
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    if bytes.len() >= address.sun_path.len() || bytes.contains(&0) {
        return Err("local control socket path too long or invalid".into());
    }
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (target, byte) in address.sun_path.iter_mut().zip(bytes) {
        *target = *byte as libc::c_char;
    }
    let length = std::mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1;
    #[cfg(any(
        target_os = "macos",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd"
    ))]
    {
        address.sun_len = length as u8;
    }
    let raw = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    if raw < 0 {
        return Err("cannot create local control socket".into());
    }
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    if unsafe { libc::fcntl(raw, libc::F_SETFD, libc::FD_CLOEXEC) } < 0
        || unsafe { libc::fcntl(raw, libc::F_SETFL, libc::O_NONBLOCK) } < 0
    {
        return Err("cannot configure local control socket".into());
    }
    let result = unsafe {
        libc::connect(
            raw,
            (&address as *const libc::sockaddr_un).cast(),
            length as libc::socklen_t,
        )
    };
    if result != 0 {
        if std::io::Error::last_os_error().raw_os_error() != Some(libc::EINPROGRESS) {
            return Err("local relay control unavailable".into());
        }
        loop {
            if Instant::now() >= deadline {
                return Err("local relay control timed out".into());
            }
            let mut poll = libc::pollfd {
                fd: raw,
                events: libc::POLLOUT,
                revents: 0,
            };
            let result = unsafe {
                libc::poll(
                    &mut poll,
                    1,
                    deadline
                        .saturating_duration_since(Instant::now())
                        .as_millis()
                        .max(1) as i32,
                )
            };
            if result < 0
                && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
            {
                continue;
            }
            if result <= 0 {
                return Err("local relay control timed out".into());
            }
            let mut error: libc::c_int = 0;
            let mut size = std::mem::size_of_val(&error) as libc::socklen_t;
            if unsafe {
                libc::getsockopt(
                    raw,
                    libc::SOL_SOCKET,
                    libc::SO_ERROR,
                    (&mut error as *mut libc::c_int).cast(),
                    &mut size,
                )
            } != 0
                || error != 0
            {
                return Err("local relay control unavailable".into());
            }
            break;
        }
    }
    let stream = UnixStream::from(fd);
    stream.set_nonblocking(false)?;
    same_user(&stream)?;
    Ok(stream)
}

fn remaining(deadline: Instant) -> Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|d| !d.is_zero())
        .ok_or_else(|| "local control timed out".into())
}

fn read(stream: &mut UnixStream, deadline: Instant) -> Result<Value> {
    read_bounded(stream, deadline, crate::MAX_MESSAGE_BYTES)
}

fn read_bounded(stream: &mut UnixStream, deadline: Instant, limit: usize) -> Result<Value> {
    let mut bytes = Vec::new();
    loop {
        if let Some(end) = bytes.iter().position(|b| *b == b'\n') {
            if end > limit {
                return Err("local control frame too large".into());
            }
            return serde_json::from_slice(&bytes[..end])
                .map_err(|_| "invalid local control frame".into());
        }
        if bytes.len() > limit {
            return Err("local control frame too large".into());
        }
        // Poll the absolute deadline without resetting socket options after a
        // peer has sent its reply and closed (large replies need several reads).
        let mut poll = libc::pollfd {
            fd: stream.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let timeout = remaining(deadline)?
            .as_millis()
            .saturating_add(1)
            .min(i32::MAX as u128) as i32;
        let ready = unsafe { libc::poll(&mut poll, 1, timeout) };
        if ready < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error.into());
        }
        if ready == 0 {
            return Err("local control timed out".into());
        }
        let mut buffer = [0; 4096];
        let count = unsafe {
            libc::recv(
                stream.as_raw_fd(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                libc::MSG_DONTWAIT,
            )
        };
        if count < 0 {
            let error = std::io::Error::last_os_error();
            if matches!(
                error.kind(),
                std::io::ErrorKind::Interrupted | std::io::ErrorKind::WouldBlock
            ) {
                continue;
            }
            return Err(error.into());
        }
        if count == 0 {
            return Err("local control closed before response".into());
        }
        bytes.extend_from_slice(&buffer[..count as usize]);
    }
}

fn write(stream: &mut UnixStream, value: &Value, deadline: Instant) -> Result<()> {
    write_bounded(stream, value, deadline, crate::MAX_MESSAGE_BYTES)
}

fn write_bounded(
    stream: &mut UnixStream,
    value: &Value,
    deadline: Instant,
    limit: usize,
) -> Result<()> {
    let mut bytes = serde_json::to_vec(value)?;
    if bytes.len() > limit {
        return Err("local control frame too large".into());
    }
    bytes.push(b'\n');
    let mut pending = bytes.as_slice();
    while !pending.is_empty() {
        stream.set_write_timeout(Some(remaining(deadline)?))?;
        let count = stream.write(pending)?;
        if count == 0 {
            return Err("local control write closed".into());
        }
        pending = &pending[count..];
    }
    Ok(())
}

pub fn request(path: &Path, value: &Value) -> Result<Value> {
    request_with_timeout(path, value, TIMEOUT)
}

/// Directory discovery may query mesh peers; registration keeps its short deadline.
pub fn request_with_timeout(path: &Path, value: &Value, timeout: Duration) -> Result<Value> {
    let parent = path
        .parent()
        .ok_or("control socket needs a private directory")?;
    private_dir(parent)?;
    let meta = std::fs::symlink_metadata(path)?;
    if !meta.file_type().is_socket()
        || meta.uid() != unsafe { libc::geteuid() }
        || meta.permissions().mode() & 0o777 != 0o600
    {
        return Err("local control socket must be owned by this user and mode 600".into());
    }
    let deadline = Instant::now() + timeout;
    let mut stream = connect(path, deadline)?;
    write(&mut stream, value, deadline)?;
    let limit = if value["op"] == "directory" {
        DIRECTORY_LIMIT
    } else {
        crate::MAX_MESSAGE_BYTES
    };
    read_bounded(&mut stream, deadline, limit)
}

pub struct Server {
    path: PathBuf,
    running: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    _lock: std::fs::File,
}

impl Server {
    pub fn start(
        path: PathBuf,
        handler: Arc<dyn Fn(Value) -> Value + Send + Sync>,
        tick: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<Self> {
        private_dir(
            path.parent()
                .ok_or("control socket needs a private directory")?,
        )?;
        let lock = state::open_private(&path.with_extension("socket-lock"), true)?;
        lock.try_lock()
            .map_err(|_| "local control socket already in use")?;
        if let Ok(meta) = std::fs::symlink_metadata(&path) {
            if !meta.file_type().is_socket() || meta.uid() != unsafe { libc::geteuid() } {
                return Err("refusing to replace a non-owned control socket".into());
            }
            std::fs::remove_file(&path)?;
        }
        let listener = UnixListener::bind(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        let running = Arc::new(AtomicBool::new(true));
        let control = Arc::clone(&running);
        let thread = std::thread::spawn(move || {
            let workers = Arc::new(AtomicUsize::new(0));
            let mut children: Vec<std::thread::JoinHandle<()>> = Vec::new();
            while control.load(Ordering::Acquire) {
                let mut index = 0;
                while index < children.len() {
                    if children[index].is_finished() {
                        let _ = children.swap_remove(index).join();
                    } else {
                        index += 1;
                    }
                }
                tick();
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        if stream.set_nonblocking(false).is_err()
                            || same_user(&stream).is_err()
                            || workers.load(Ordering::Relaxed) >= 8
                        {
                            continue;
                        }
                        workers.fetch_add(1, Ordering::Relaxed);
                        let handler = Arc::clone(&handler);
                        let workers = Arc::clone(&workers);
                        children.push(std::thread::spawn(move || {
                            let mut deadline = Instant::now() + TIMEOUT;
                            let mut limit = crate::MAX_MESSAGE_BYTES;
                            let response = match read(&mut stream, deadline) {
                                Ok(value) => {
                                    if value["op"] == "directory" {
                                        deadline = Instant::now() + Duration::from_secs(5);
                                        limit = DIRECTORY_LIMIT;
                                    }
                                    handler(value)
                                }
                                Err(_) => {
                                    json!({"status":"invalid","error":"invalid local control request"})
                                }
                            };
                            let _ = write_bounded(&mut stream, &response, deadline, limit);
                            workers.fetch_sub(1, Ordering::Relaxed);
                        }));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10))
                    }
                    Err(_) => break,
                }
            }
            for child in children {
                let _ = child.join();
            }
        });
        Ok(Self {
            path,
            running,
            thread: Some(thread),
            _lock: lock,
        })
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn directory_reply_has_its_own_deadline_and_bounded_large_frame() {
        let root = std::env::temp_dir().join(format!(
            "ai-directory-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        ));
        state::parent(&root.join("placeholder")).unwrap();
        let socket = root.join("relay.sock");
        let server = Server::start(
            socket.clone(),
            Arc::new(|_| {
                std::thread::sleep(Duration::from_millis(300));
                json!({"agents":[], "padding":"x".repeat(80000)})
            }),
            Arc::new(|| {}),
        )
        .unwrap();
        let response =
            request_with_timeout(&socket, &json!({"op":"directory"}), Duration::from_secs(2))
                .unwrap();
        assert_eq!(response["padding"].as_str().unwrap().len(), 80000);
        assert!(request(&socket, &json!({"op":"heartbeat"})).is_err());
        drop(server);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn peer_credentials_match_real_socket_and_reject_another_uid() {
        let (a, b) = UnixStream::pair().unwrap();
        same_user(&a).unwrap();
        same_user(&b).unwrap();
        let uid = unsafe { libc::geteuid() };
        assert!(uid_matches(uid, uid.wrapping_add(1)).is_err());
    }
}
