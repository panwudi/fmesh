use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

static STOP: AtomicBool = AtomicBool::new(false);
extern "C" fn stop(_: i32) { STOP.store(true, Ordering::Release); }
unsafe extern "C" { fn signal(sig: i32, handler: usize) -> usize; }

fn main() -> std::io::Result<()> {
    if std::env::args().nth(1).as_deref() == Some("--version") { return Ok(()); }
    unsafe { signal(15, stop as *const () as usize); }
    let root = PathBuf::from(std::env::var_os("AGENT_INBOX_FAKE_CLAUDE_ROOT").unwrap());
    let id = std::env::var("CLAUDE_CODE_SESSION_ID").unwrap();
    let socket = root.join(format!("{id}.sock"));
    let listener = UnixListener::bind(&socket)?;
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    let record = std::fs::read_to_string(root.join(format!("{id}.template")))?
        .replace("\"pid\":0", &format!("\"pid\":{}", std::process::id()));
    std::fs::create_dir_all(root.join("sessions"))?;
    let record_path = root.join("sessions").join(format!("{}.json", std::process::id()));
    std::fs::write(&record_path, record)?;
    let running = Arc::new(AtomicBool::new(true));
    let worker = running.clone();
    let capture = root.join(format!("{id}.capture"));
    let expected = std::env::var("CLAUDE_CODE_MESSAGING_TOKEN").unwrap_or_default();
    let socket_thread = std::thread::spawn(move || {
        while worker.load(Ordering::Acquire) {
            match listener.accept() {
                Ok((stream, _)) => {
                    let _ = stream.set_nonblocking(false);
                    // On macOS an already closed peer can reject SO_RCVTIMEO while its bytes remain readable.
                    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
                    let mut lines = BufReader::new(stream).lines();
                    let Some(Ok(auth)) = lines.next() else { continue };
                    assert!(auth.contains(&expected));
                    let Some(Ok(message)) = lines.next() else { continue };
                    // Capture only the message frame, never its authentication line.
                    let mut output = std::fs::OpenOptions::new().create(true).append(true).open(&capture).unwrap();
                    writeln!(output, "{message}").unwrap();
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(10)),
                Err(e) => panic!("fake Claude socket failed: {e}"),
            }
        }
    });
    let mut command = Command::new(std::env::args_os().nth(1).unwrap());
    command.args(std::env::args_os().skip(2)).env("CLAUDE_CODE_MESSAGING_SOCKET", &socket)
        .stdin(Stdio::inherit()).stdout(Stdio::inherit()).stderr(Stdio::inherit());
    if std::env::var_os("AGENT_INBOX_FAKE_DISABLE_NATIVE").is_some() {
        command.env_remove("CLAUDE_CODE_MESSAGING_SOCKET").env_remove("CLAUDE_CODE_MESSAGING_TOKEN");
    }
    let mut child = command.spawn()?;
    loop {
        if child.try_wait()?.is_some() { break; }
        if STOP.load(Ordering::Acquire) { let _ = child.kill(); let _ = child.wait(); break; }
        std::thread::sleep(Duration::from_millis(10));
    }
    running.store(false, Ordering::Release);
    socket_thread.join().unwrap();
    let _ = std::fs::remove_file(&socket);
    let _ = std::fs::remove_file(&record_path);
    Ok(())
}
