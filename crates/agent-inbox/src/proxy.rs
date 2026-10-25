use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use crate::Result;

/// The proxy relays raw WebSocket bytes. It is not a JSONL app-server.
pub struct ProxyStream {
    child: Child,
    stdin: ChildStdin,
    incoming: Receiver<io::Result<Vec<u8>>>,
    buffered: io::Cursor<Vec<u8>>,
    deadline: Instant,
}

impl ProxyStream {
    pub fn spawn(codex: &Path, socket: &Path, timeout: Duration) -> Result<Self> {
        check_socket_owner(socket)?;
        let mut child = Command::new(codex)
            .args(["app-server", "proxy", "--sock"])
            .arg(socket)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let stdin = child.stdin.take().ok_or("proxy stdin unavailable")?;
        let mut stdout = child.stdout.take().ok_or("proxy stdout unavailable")?;
        let (sender, incoming) = mpsc::sync_channel(16);
        std::thread::spawn(move || {
            loop {
                let mut bytes = vec![0; 8192];
                let result = match stdout.read(&mut bytes) {
                    Ok(0) => break,
                    Ok(n) => {
                        bytes.truncate(n);
                        Ok(bytes)
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => Err(e),
                };
                let failed = result.is_err();
                if sender.send(result).is_err() || failed {
                    break;
                }
            }
        });
        Ok(Self {
            child,
            stdin,
            incoming,
            buffered: io::Cursor::new(Vec::new()),
            deadline: Instant::now() + timeout,
        })
    }

    pub fn reset_deadline(&mut self, timeout: Duration) {
        self.deadline = Instant::now() + timeout;
    }
}

impl Read for ProxyStream {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        if Instant::now() >= self.deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "app-server timed out",
            ));
        }
        let n = self.buffered.read(output)?;
        if n > 0 {
            return Ok(n);
        }
        match self
            .incoming
            .recv_timeout(self.deadline.saturating_duration_since(Instant::now()))
        {
            Ok(bytes) => {
                self.buffered = io::Cursor::new(bytes?);
                self.buffered.read(output)
            }
            Err(mpsc::RecvTimeoutError::Timeout) => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "app-server timed out",
            )),
            Err(mpsc::RecvTimeoutError::Disconnected) => Ok(0),
        }
    }
}

impl Write for ProxyStream {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.stdin.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stdin.flush()
    }
}

impl Drop for ProxyStream {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub const CODEX_VERSION: &str = "0.162.1";

pub fn check_socket_owner(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{FileTypeExt, MetadataExt};
        let metadata = std::fs::metadata(path).map_err(|_| "Codex control socket unavailable")?;
        if !metadata.file_type().is_socket() || metadata.uid() != unsafe { libc::geteuid() } {
            return Err("Codex control socket must belong to this OS user".into());
        }
    }
    Ok(())
}

/// Warn on untested versions; validate the actual protocol during delivery.
pub fn checked_socket(codex: &Path, custom_socket: Option<&Path>) -> Result<PathBuf> {
    let version = Command::new(codex).arg("--version").output()?;
    if !version.status.success() {
        return Err("cannot inspect Codex CLI version".into());
    }
    let cli = String::from_utf8_lossy(&version.stdout);
    let cli_version = cli.trim().strip_prefix("codex-cli ").unwrap_or(cli.trim());
    if let Some(socket) = custom_socket {
        warn_versions(cli_version, None);
        return Ok(socket.to_owned());
    }
    let output = Command::new(codex)
        .args(["app-server", "daemon", "version"])
        .output()?;
    if !output.status.success() {
        return Err("cannot inspect app-server daemon; start the daemon or provide --sock".into());
    }
    let daemon: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    if daemon["status"] != "running" {
        return Err("app-server daemon is not running".into());
    }
    let server_version = daemon["appServerVersion"]
        .as_str()
        .ok_or("daemon appServerVersion missing")?;
    warn_versions(cli_version, Some(server_version));
    Ok(PathBuf::from(
        daemon["socketPath"]
            .as_str()
            .ok_or("daemon socketPath missing")?,
    ))
}

fn warn_versions(cli: &str, daemon: Option<&str>) {
    if cli != CODEX_VERSION || daemon.is_some_and(|version| version != CODEX_VERSION) {
        eprintln!(
            "agent-inbox: warning: tested with Codex {CODEX_VERSION}; CLI {cli}, daemon {}; validating protocol responses",
            daemon.unwrap_or("custom socket")
        );
    }
}
