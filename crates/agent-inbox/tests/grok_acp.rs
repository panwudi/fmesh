#![cfg(unix)]

use agent_inbox::grok_acp::Client;
use std::net::Shutdown;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[test]
fn grok_acp_lists_only_resident_sessions_and_interjects_with_sender_context() {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = PathBuf::from(format!(
        "/tmp/agent-inbox-grok-acp-{}-{stamp}",
        std::process::id()
    ));
    std::fs::create_dir(&root).unwrap();
    let socket_path = root.join("leader.sock");
    let listener = UnixListener::bind(&socket_path).unwrap();
    let fake = root.join("fake-grok");
    let output = Command::new(std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()))
        .arg("--edition=2024")
        .arg(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/support/fake_grok.rs"))
        .arg("-o")
        .arg(&fake)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        Command::new(&fake)
            .arg("--version")
            .output()
            .unwrap()
            .status
            .success()
    );
    let log = root.join("interject.json");
    unsafe {
        std::env::set_var("AGENT_INBOX_TEST_GROK_LOG", &log);
    }
    let mut client = Client::connect(&fake, &socket_path, Duration::from_secs(5)).unwrap();
    let roster = client.roster().unwrap();
    assert_eq!(roster.len(), 1);
    assert_eq!(roster[0].cwd, "/tmp/project");
    assert_eq!(roster[0].activity, "working");
    client
        .interject(
            &roster[0].id,
            "alpha.claude",
            "Please inspect the test",
            "message-id",
        )
        .unwrap();
    let sent = std::fs::read_to_string(&log).unwrap();
    assert!(sent.contains("_x.ai/interject"));
    assert!(sent.contains("alpha.claude"));
    assert!(sent.contains("message-id"));
    assert!(sent.contains("Please inspect the test"));
    drop(client);
    let _ = listener.set_nonblocking(true);
    let _ = std::os::unix::net::UnixStream::connect(&socket_path)
        .and_then(|s| s.shutdown(Shutdown::Both));
    drop(listener);
    unsafe {
        std::env::remove_var("AGENT_INBOX_TEST_GROK_LOG");
    }
    std::fs::remove_dir_all(root).unwrap();
}
