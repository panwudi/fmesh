//! Bounded subprocess output and lifetime, without a shell.
use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::Result;

struct Child(std::process::Child);
impl Drop for Child {
    fn drop(&mut self) {
        #[cfg(unix)]
        unsafe {
            libc::kill(-(self.0.id() as libc::pid_t), libc::SIGKILL);
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

pub fn output(command: &mut Command, timeout: Duration) -> Result<Vec<u8>> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = Child(command.spawn()?);
    let stdout = child.0.stdout.take().ok_or("command stdout unavailable")?;
    let (sender, receiver) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut data = Vec::new();
        let result = stdout
            .take(1024 * 1024 + 1)
            .read_to_end(&mut data)
            .map(|_| data);
        let _ = sender.send(result);
    });
    let deadline = Instant::now() + timeout;
    let result = loop {
        if let Some(status) = child.0.try_wait()? {
            if !status.success() {
                break Err("command failed (output suppressed)".into());
            }
            match receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(Ok(bytes)) if bytes.len() <= 1024 * 1024 => break Ok(bytes),
                _ => break Err("command output unavailable, excessive or timed out".into()),
            }
        }
        if Instant::now() >= deadline {
            break Err("command timed out".into());
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    drop(child);
    let _ = reader.join();
    result
}
