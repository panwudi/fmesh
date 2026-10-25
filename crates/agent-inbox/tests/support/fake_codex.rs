#[cfg(unix)]
fn main() -> std::io::Result<()> {
    use std::io::{self, Read, Write};
    use std::os::unix::net::UnixStream;
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(root) = std::env::var_os("AGENT_INBOX_TEST_DAEMON") {
        let root = std::path::PathBuf::from(root);
        if args == ["test-latest"] {
            print!("{}", std::fs::read_to_string(root.join("latest.json"))?);
            return Ok(());
        }
        if args == ["test-notify"] {
            let mut file = std::fs::OpenOptions::new().create(true).append(true).open(root.join("notices.jsonl"))?;
            std::io::copy(&mut std::io::stdin().lock(), &mut file)?;
            return Ok(());
        }
        if args == ["test-blocking-notify"] {
            std::fs::write(root.join("notify.pid"),std::process::id().to_string())?;
            loop { std::thread::sleep(std::time::Duration::from_secs(1)); }
        }
        if args == ["app-server", "daemon", "version"] {
            let file = if root.join("updated").exists() { "after.json" } else { "before.json" };
            print!("{}", std::fs::read_to_string(root.join(file))?);
            return Ok(());
        }
        if args == ["app-server", "daemon", "update", "--yes"] {
            std::fs::write(root.join("update-requested"), "1")?;
            if root.join("fail-update").exists() { std::process::exit(1); }
            while root.join("wait-update").exists() && !root.join("continue-update").exists() {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            std::fs::write(root.join("updated"), "1")?;
            println!("{{\"status\":\"updated\"}}");
            return Ok(());
        }
    }
    if args == ["--version"] {
        println!(
            "codex-cli {}",
            std::env::var("AGENT_INBOX_TEST_VERSION").unwrap_or_else(|_| "0.162.1".into())
        );
        return Ok(());
    }
    assert_eq!(&args[..3], ["app-server", "proxy", "--sock"]);
    let mut socket = UnixStream::connect(&args[3])?;
    let mut writer = socket.try_clone()?;
    std::thread::spawn(move || {
        let _ = io::copy(&mut io::stdin().lock(), &mut writer);
        let _ = writer.shutdown(std::net::Shutdown::Write);
    });
    let mut stdout = io::stdout().lock();
    let mut buffer = [0; 8192];
    loop {
        let n = socket.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        stdout.write_all(&buffer[..n])?;
        stdout.flush()?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn main() {
    panic!("fake app-server proxy requires Unix sockets");
}
