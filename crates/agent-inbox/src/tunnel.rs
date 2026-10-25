//! Owner-managed deployment helpers. Route selection never contacts either endpoint.

use std::fs::{DirBuilder, File, OpenOptions, TryLockError};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::Ipv4Addr;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::io::FromRawFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::Result;

static STOP: AtomicBool = AtomicBool::new(false);
const POLL: Duration = Duration::from_millis(100);

extern "C" fn stop_signal(_: libc::c_int) {
    STOP.store(true, Ordering::Relaxed);
}

fn signals() -> Result<()> {
    for signal in [libc::SIGHUP, libc::SIGINT, libc::SIGTERM] {
        // The handler only stores a lock-free atomic; it does not perform IO or allocate.
        if unsafe { libc::signal(signal, stop_signal as *const () as libc::sighandler_t) }
            == libc::SIG_ERR
        {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    Ok(())
}

fn value(name: &str) -> Result<String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("missing {name} in tunnel configuration").into())
}

fn number(name: &str, default: u64) -> Result<u64> {
    match std::env::var(name) {
        Ok(value) => value.parse().map_err(|_| format!("invalid {name}").into()),
        Err(_) => Ok(default),
    }
}

fn positive(name: &str, default: u64) -> Result<u64> {
    let n = number(name, default)?;
    if !(1..=65535).contains(&n) {
        return Err(format!("{name} must be between 1 and 65535").into());
    }
    Ok(n)
}

#[derive(Clone, Copy)]
struct Prefix {
    network: u32,
    mask: u32,
}
impl Prefix {
    fn parse(value: &str) -> Result<Self> {
        let (address, bits) = value
            .split_once('/')
            .ok_or("INBOX_INTR_PREFIX must be IPv4 CIDR")?;
        let address: Ipv4Addr = address
            .parse()
            .map_err(|_| "invalid INBOX_INTR_PREFIX address")?;
        let bits: u32 = bits
            .parse()
            .map_err(|_| "invalid INBOX_INTR_PREFIX length")?;
        if bits > 32 {
            return Err("INBOX_INTR_PREFIX length exceeds 32".into());
        }
        let mask = if bits == 0 {
            0
        } else {
            u32::MAX << (32 - bits)
        };
        Ok(Self {
            network: u32::from(address) & mask,
            mask,
        })
    }
    fn contains(self, address: Ipv4Addr) -> bool {
        u32::from(address) & self.mask == self.network
    }
}

#[derive(Default)]
struct Facts {
    addresses: Vec<Ipv4Addr>,
    gateways: Vec<Ipv4Addr>,
}

fn output(program: &str, args: &[&str]) -> String {
    Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .ok()
        .filter(|result| result.status.success())
        .map(|result| String::from_utf8_lossy(&result.stdout).into_owned())
        .unwrap_or_default()
}

fn facts() -> Facts {
    let mut facts = Facts::default();
    if let Ok(program) = std::env::var("INBOX_LOCAL_FACTS_BIN") {
        for line in output(&program, &["facts"]).lines() {
            let mut words = line.split_whitespace();
            let kind = words.next();
            if let Some(address) = words.next().and_then(|word| word.parse().ok()) {
                match kind {
                    Some("address") => facts.addresses.push(address),
                    Some("gateway") => facts.gateways.push(address),
                    _ => {}
                }
            }
        }
    } else if cfg!(target_os = "macos") {
        for line in output("/sbin/ifconfig", &["-a"]).lines() {
            let mut words = line.split_whitespace();
            if words.next() == Some("inet")
                && let Some(address) = words.next().and_then(|word| word.parse().ok())
            {
                facts.addresses.push(address);
            }
        }
        for line in output("/sbin/route", &["-n", "get", "default"]).lines() {
            let mut words = line.split_whitespace();
            if words.next() == Some("gateway:")
                && let Some(address) = words.next().and_then(|word| word.parse().ok())
            {
                facts.gateways.push(address);
            }
        }
    } else {
        for line in output("ip", &["-o", "-4", "addr", "show"]).lines() {
            let words: Vec<_> = line.split_whitespace().collect();
            if let Some(address) = words
                .windows(2)
                .find(|pair| pair[0] == "inet")
                .and_then(|pair| pair[1].split('/').next())
                .and_then(|word| word.parse().ok())
            {
                facts.addresses.push(address);
            }
        }
        for line in output("ip", &["-4", "route", "show", "default"])
            .lines()
            .take(1)
        {
            let words: Vec<_> = line.split_whitespace().collect();
            if let Some(address) = words
                .windows(2)
                .find(|pair| pair[0] == "via")
                .and_then(|pair| pair[1].parse().ok())
            {
                facts.gateways.push(address);
            }
        }
    }
    facts
}

fn route() -> Result<(String, u64)> {
    let prefix = std::env::var("INBOX_INTR_PREFIX")
        .ok()
        .filter(|s| !s.is_empty())
        .map(|s| Prefix::parse(&s))
        .transpose()?;
    let gateway: Option<Ipv4Addr> = std::env::var("INBOX_INTR_GATEWAY")
        .ok()
        .filter(|s| !s.is_empty())
        .map(|s| {
            s.parse()
                .map_err(|_| "INBOX_INTR_GATEWAY must be an IPv4 address")
        })
        .transpose()?;
    let facts = facts();
    let internal = prefix.is_some_and(|prefix| {
        facts
            .addresses
            .iter()
            .any(|address| prefix.contains(*address))
    }) || gateway.is_some_and(|gateway| facts.gateways.contains(&gateway));
    let kind = if internal { "INTR" } else { "PUBLIC" };
    let host = value(&format!("INBOX_{kind}_HOST"))?;
    if host.starts_with('-') || host.contains(char::is_whitespace) {
        return Err("invalid tunnel endpoint".into());
    }
    Ok((host, positive(&format!("INBOX_{kind}_PORT"), 22)?))
}

pub fn print_route() -> Result<()> {
    let (host, port) = route()?;
    println!("{host} {port}");
    Ok(())
}

struct Backoff {
    initial: u64,
    maximum: u64,
    stable: u64,
    current: u64,
}
impl Backoff {
    fn load() -> Result<Self> {
        let initial = number("INBOX_BACKOFF_INITIAL", 15)?;
        let maximum = number("INBOX_BACKOFF_MAX", 300)?;
        let stable = number("INBOX_STABLE_RESET", 300)?;
        if initial < 15 || maximum > 300 || initial > maximum || stable < 300 {
            return Err("unsafe retry settings: initial >= 15, initial <= max <= 300, stable reset >= 300 seconds required".into());
        }
        Ok(Self {
            initial,
            maximum,
            stable,
            current: initial,
        })
    }
    fn delay(&mut self, authenticated_for: Option<Duration>, random: u32) -> Duration {
        if authenticated_for.is_some_and(|duration| duration.as_secs() >= self.stable) {
            self.current = self.initial;
        }
        let low = self.current * 800;
        let high = (self.current * 1200).min(self.maximum * 1000);
        let millis = low + u64::from(random) % (high - low + 1);
        self.current = self.current.saturating_mul(2).min(self.maximum);
        Duration::from_millis(millis)
    }
}

fn random() -> Result<u32> {
    let mut bytes = [0; 4];
    File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(u32::from_ne_bytes(bytes))
}

struct SocketDir(PathBuf);
impl SocketDir {
    fn new() -> Result<Self> {
        for _ in 0..10 {
            let path = PathBuf::from(format!(
                "/tmp/agent-inbox-tunnel-{}-{:08x}",
                std::process::id(),
                random()?
            ));
            match DirBuilder::new().mode(0o700).create(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
        }
        Err("cannot create private tunnel control directory".into())
    }
}
impl Drop for SocketDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn stopping(parent: Option<libc::pid_t>) -> bool {
    STOP.load(Ordering::Relaxed)
        || parent.is_some_and(|parent| unsafe { libc::getppid() } != parent)
}

fn dial_lock(parent: Option<libc::pid_t>) -> Result<File> {
    let path = PathBuf::from(value("INBOX_DIAL_LOCK_FILE")?);
    if !path.is_absolute() {
        return Err("INBOX_DIAL_LOCK_FILE must be absolute".into());
    }
    if let Some(parent) = path.parent() {
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)?;
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let metadata = file.metadata()?;
    // A persistent inode is shared by all sessions. Never unlink or replace this file.
    if !metadata.is_file()
        || metadata.permissions().mode() & 0o777 != 0o600
        || metadata.uid() != unsafe { libc::geteuid() }
    {
        return Err("dial lock must be an owner-controlled regular mode-600 file".into());
    }
    let mut waiting = false;
    loop {
        if stopping(parent) {
            return Err("tunnel stopped while waiting for dial lock".into());
        }
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(TryLockError::WouldBlock) => {
                if !waiting {
                    eprintln!("agent-inbox tunnels: waiting for shared relay dial lock");
                    waiting = true;
                }
                std::thread::sleep(POLL);
            }
            Err(error) => return Err(error.into()),
        }
    }
}

fn kill_group(pid: u32, signal: libc::c_int) {
    // The worker owns a new process group; this never targets another session's SSH.
    unsafe {
        libc::kill(-(pid as libc::pid_t), signal);
    }
}

struct WorkerChild(Child);
impl Drop for WorkerChild {
    fn drop(&mut self) {
        kill_group(self.0.id(), libc::SIGKILL);
        let _ = self.0.wait();
    }
}

pub fn supervise() -> Result<()> {
    signals()?;
    let parent = std::env::var("INBOX_SUPERVISOR_PARENT")
        .ok()
        .map(|value| value.parse::<libc::pid_t>())
        .transpose()?;
    if parent.is_some_and(|parent| parent <= 1) {
        return Err("invalid supervisor parent".into());
    }
    let mut backoff = Backoff::load()?;
    while !stopping(parent) {
        let lock = match dial_lock(parent) {
            Ok(lock) => lock,
            Err(_) if stopping(parent) => break,
            Err(error) => return Err(error),
        };
        let directory = SocketDir::new()?;
        let mut command = Command::new(std::env::current_exe()?);
        command
            .args(["tunnel-worker", "--control"])
            .arg(directory.0.join("control"))
            .arg("--parent")
            .arg(std::process::id().to_string())
            .stdin(Stdio::from(lock.try_clone()?))
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .process_group(0);
        let mut worker = WorkerChild(command.spawn()?);
        // Command owns another duplicate of the inherited stdin file until it is dropped.
        drop(command);
        let child = &mut worker.0;
        let mut lock = Some(lock);
        let (ready, received) = mpsc::channel();
        let stdout = child.stdout.take().unwrap();
        let reader = std::thread::spawn(move || {
            for line in BufReader::new(stdout)
                .lines()
                .map_while(std::result::Result::ok)
            {
                if line == "authenticated" {
                    let _ = ready.send(Instant::now());
                }
            }
        });
        let mut authenticated = None;
        let status = loop {
            if let Ok(when) = received.try_recv() {
                authenticated = Some(when);
                lock.take();
            }
            if stopping(parent) {
                kill_group(child.id(), libc::SIGTERM);
            }
            if let Some(status) = child.try_wait()? {
                break status;
            }
            std::thread::sleep(POLL);
        };
        let ended = Instant::now();
        // Retain our duplicate lock until any orphan from a crashed worker is stopped.
        kill_group(child.id(), libc::SIGKILL);
        let _ = reader.join();
        drop(worker);
        drop(lock);
        drop(directory);
        if stopping(parent) {
            break;
        }
        let delay = backoff.delay(
            authenticated.map(|when| ended.saturating_duration_since(when)),
            random()?,
        );
        eprintln!(
            "agent-inbox tunnels: SSH ended ({status}), retrying in {:.3}s; a stale reverse port may still be bound",
            delay.as_secs_f64()
        );
        let deadline = Instant::now() + delay;
        while Instant::now() < deadline && !stopping(parent) {
            std::thread::sleep(POLL.min(deadline.saturating_duration_since(Instant::now())));
        }
    }
    Ok(())
}

struct SshChild(Child);
impl Drop for SshChild {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_some() {
            return;
        }
        // Give OpenSSH a chance to send its disconnect and release remote forwards.
        // A broken transport may still leave sshd holding its port until server keepalive expiry.
        unsafe { libc::kill(self.0.id() as libc::pid_t, libc::SIGTERM) };
        let deadline = Instant::now() + Duration::from_secs(1);
        while matches!(self.0.try_wait(), Ok(None)) && Instant::now() < deadline {
            std::thread::sleep(POLL);
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn reverse_forward(ssh: &str, control: &Path, host: &str, forward: &str) -> Result<SshChild> {
    // -O requires this private master and fails if it is gone. It never authenticates
    // another connection. Ignore SSH config so its own forward/session directives cannot run.
    Ok(SshChild(
        Command::new(ssh)
            .args(["-F", "/dev/null", "-S"])
            .arg(control)
            .args(["-O", "forward", "-R", forward, host])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()?,
    ))
}

fn forward_retry(random: u32) -> Duration {
    // These requests share the authenticated transport and consume no pre-auth slots.
    Duration::from_millis(2400 + u64::from(random % 1201))
}

struct ControlCleanup(PathBuf);
impl Drop for ControlCleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
        if let Some(parent) = self.0.parent() {
            let _ = std::fs::remove_dir(parent);
        }
    }
}

pub fn worker(mut args: impl Iterator<Item = String>) -> Result<()> {
    signals()?;
    if args.next().as_deref() != Some("--control") {
        return Err("worker requires --control".into());
    }
    let control = PathBuf::from(args.next().ok_or("worker requires control path")?);
    if args.next().as_deref() != Some("--parent") {
        return Err("worker requires supervisor parent".into());
    }
    let parent: libc::pid_t = args.next().ok_or("worker requires parent PID")?.parse()?;
    let directory = control.parent().ok_or("invalid control directory")?;
    if control.file_name().and_then(|name| name.to_str()) != Some("control")
        || directory.parent() != Some(Path::new("/tmp"))
        || !directory
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("agent-inbox-tunnel-"))
    {
        return Err("worker requires a private tunnel control directory".into());
    }
    let _cleanup = ControlCleanup(control.clone());
    if parent <= 1 || unsafe { libc::getppid() } != parent {
        return Ok(());
    }
    // The supervisor supplies its locked file as stdin. The shared open-file description
    // keeps the flock alive if either process crashes before authentication.
    let mut lock = Some(unsafe { File::from_raw_fd(0) });
    if !lock.as_ref().unwrap().metadata()?.is_file() {
        return Err("worker requires inherited dial lock".into());
    }
    let (host, port) = route()?;
    let timeout = positive("INBOX_DIAL_TIMEOUT", 45)?;
    let ssh = value("INBOX_SSH_BIN")?;
    let reverse = format!(
        "127.0.0.1:{}:127.0.0.1:{}",
        positive("INBOX_REVERSE_PORT", 0)?,
        positive("INBOX_LOCAL_INBOX_PORT", 0)?
    );
    let mut command = Command::new(&ssh);
    command
        .arg("-F")
        .arg(value("INBOX_SSH_CONFIG")?)
        .args(["-M", "-N", "-T", "-n", "-S"])
        .arg(&control)
        .args([
            "-o",
            "ProxyCommand=none",
            "-o",
            "ProxyJump=none",
            "-o",
            "ControlMaster=yes",
            "-o",
            "ControlPersist=no",
            "-o",
            "ForkAfterAuthentication=no",
            "-o",
            "BatchMode=yes",
            "-o",
            "IdentitiesOnly=yes",
            "-o",
            "StrictHostKeyChecking=yes",
            "-o",
            "ExitOnForwardFailure=yes",
            "-o",
            "ConnectionAttempts=1",
        ])
        .arg("-o")
        .arg(format!("HostKeyAlias={}", value("INBOX_HOST_KEY_ALIAS")?))
        .arg("-o")
        .arg(format!(
            "UserKnownHostsFile={}",
            value("INBOX_KNOWN_HOSTS")?
        ))
        .arg("-o")
        .arg(format!(
            "ConnectTimeout={}",
            positive("INBOX_OUTER_CONNECT_TIMEOUT", 45)?
        ))
        .arg("-o")
        .arg(format!(
            "ServerAliveInterval={}",
            positive("INBOX_ALIVE_INTERVAL", 15)?
        ))
        .arg("-o")
        .arg(format!(
            "ServerAliveCountMax={}",
            positive("INBOX_ALIVE_COUNT", 3)?
        ))
        .arg("-i")
        .arg(value("INBOX_SSH_KEY")?)
        .arg("-l")
        .arg(value("INBOX_SSH_USER")?)
        .arg("-p")
        .arg(port.to_string())
        .arg("-L")
        .arg(format!(
            "127.0.0.1:{}:127.0.0.1:{}",
            positive("INBOX_LOCAL_SEND_PORT", 0)?,
            positive("INBOX_HUB_PORT", 0)?
        ))
        .arg(&host)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    eprintln!("agent-inbox tunnels: connecting local hub forward to {host}:{port}");
    let mut child = SshChild(command.spawn()?);
    let mut forwarding: Option<SshChild> = None;
    let mut forwarded = false;
    let mut next_forward = Instant::now();
    let deadline = Instant::now() + Duration::from_secs(timeout);
    loop {
        if STOP.load(Ordering::Relaxed) || unsafe { libc::getppid() } != parent {
            return Ok(());
        }
        if let Some(status) = child.0.try_wait()? {
            if status.success() {
                return Ok(());
            }
            return Err("SSH connection ended".into());
        }
        if lock.is_some()
            && std::fs::symlink_metadata(&control)
                .is_ok_and(|metadata| metadata.file_type().is_socket())
        {
            // This is the real tunnel's private master socket, created after authentication.
            println!("authenticated");
            std::io::stdout().flush()?;
            lock.take();
        }
        if lock.is_some() && Instant::now() >= deadline {
            return Err("SSH authentication deadline exceeded".into());
        }
        if let Some(request) = &mut forwarding
            && let Some(status) = request.0.try_wait()?
        {
            forwarding.take();
            if status.success() {
                forwarded = true;
                eprintln!("agent-inbox tunnels: both loopback forwards ready on existing SSH");
            } else {
                let delay = forward_retry(random()?);
                next_forward = Instant::now() + delay;
                eprintln!(
                    "agent-inbox tunnels: reverse forward unavailable ({status}); retrying in {:.3}s on existing SSH; retaining local hub forward",
                    delay.as_secs_f64()
                );
            }
        }
        if lock.is_none() && !forwarded && forwarding.is_none() && Instant::now() >= next_forward {
            forwarding = Some(reverse_forward(&ssh, &control, &host, &reverse)?);
        }
        std::thread::sleep(POLL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn forwarding_retries_stay_short_without_another_authentication() {
        assert_eq!(forward_retry(0), Duration::from_millis(2400));
        assert_eq!(forward_retry(1200), Duration::from_millis(3600));
        assert!((2400..=3600).contains(&forward_retry(u32::MAX).as_millis()));
    }
    #[test]
    fn prefix_matches_both_halves_of_a_23_and_boundary_lengths() {
        let prefix = Prefix::parse("192.168.110.0/23").unwrap();
        for ip in ["192.168.110.1", "192.168.111.254"] {
            assert!(prefix.contains(ip.parse().unwrap()));
        }
        for ip in ["192.168.109.255", "192.168.112.0"] {
            assert!(!prefix.contains(ip.parse().unwrap()));
        }
        assert!(
            Prefix::parse("0.0.0.0/0")
                .unwrap()
                .contains(Ipv4Addr::LOCALHOST)
        );
        assert!(
            !Prefix::parse("127.0.0.1/32")
                .unwrap()
                .contains("127.0.0.2".parse().unwrap())
        );
        assert!(Prefix::parse("192.168.110.0/33").is_err());
    }
    #[test]
    fn jitter_growth_cap_and_reset_require_five_authenticated_minutes() {
        let mut retry = Backoff {
            initial: 15,
            maximum: 300,
            stable: 300,
            current: 15,
        };
        assert_eq!(retry.delay(None, 0), Duration::from_secs(12));
        assert_eq!(
            retry.delay(Some(Duration::from_secs(299)), 0),
            Duration::from_secs(24)
        );
        assert_eq!(
            retry.delay(Some(Duration::from_secs(300)), 6000),
            Duration::from_secs(18)
        );
        retry.current = 300;
        for random in [0, 1, 60_000, u32::MAX] {
            let delay = retry.delay(None, random);
            assert!((Duration::from_secs(240)..=Duration::from_secs(300)).contains(&delay));
        }
        assert_eq!(retry.current, 300);
    }
}
