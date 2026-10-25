use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::os::unix::net::{UnixListener, UnixStream};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn log(kind: &str, args: &[String]) {
    let path = std::env::var("INBOX_FAKE_SSH_LOG").unwrap();
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis();
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)
        .unwrap();
    let parent = std::process::Command::new("ps")
        .args(["-o", "ppid=", "-p", &std::process::id().to_string()])
        .output()
        .unwrap();
    let parent = String::from_utf8(parent.stdout).unwrap();
    writeln!(
        file,
        "{kind}\t{millis}\t{}\t{}\t{}",
        std::process::id(),
        parent.trim(),
        args.join("\t")
    )
    .unwrap();
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args == ["--warmup"] {
        return;
    }
    if args == ["facts"] {
        log("facts", &args);
        print!(
            "{}",
            std::fs::read_to_string(std::env::var("INBOX_FAKE_FACTS_FILE").unwrap()).unwrap()
        );
        return;
    }
    if args.windows(2).any(|pair| pair == ["-O", "forward"]) {
        log("forward", &args);
        assert!(args.windows(2).any(|pair| pair == ["-F", "/dev/null"]));
        let control = args.windows(2).find(|pair| pair[0] == "-S").unwrap();
        // A missing master must fail without creating a replacement transport.
        let Ok(mut stream) = UnixStream::connect(&control[1]) else {
            std::process::exit(255);
        };
        stream.write_all(b"forward\n").unwrap();
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).unwrap();
        std::process::exit(if line == "ok\n" { 0 } else { 255 });
    }
    if !args.iter().any(|arg| arg == "-N") {
        log("transport", &args);
        print!("raw transport ready");
        return;
    }
    let path = std::env::var("INBOX_FAKE_SSH_LOG").unwrap();
    let previous = std::fs::read_to_string(path).unwrap_or_default();
    log("dial", &args);
    let behavior = std::env::var("INBOX_FAKE_SSH_BEHAVIOR").unwrap_or_else(|_| "hold".into());
    if behavior == "fail"
        || behavior == "fail-then-hold" && !previous.lines().any(|line| line.starts_with("dial\t"))
    {
        std::process::exit(255);
    }
    if behavior == "hang-auth" {
        std::thread::sleep(Duration::from_secs(120));
    }
    if behavior == "slow-auth" {
        std::thread::sleep(Duration::from_secs(1));
    }
    if behavior == "serve-hub" {
        std::thread::sleep(Duration::from_millis(500));
    }
    let busy = std::env::var_os("INBOX_FAKE_REVERSE_BUSY").map(std::path::PathBuf::from);
    // Reproduce the old all-or-nothing startup when a previous sshd retains -R.
    if args.contains(&"-R".into()) && busy.as_ref().is_some_and(|path| path.exists()) {
        log("initial-reverse-collision", &args);
        std::process::exit(255);
    }
    let control = args.windows(2).find(|pair| pair[0] == "-S").unwrap();
    let socket = UnixListener::bind(&control[1]).unwrap();
    socket.set_nonblocking(true).unwrap();
    if behavior == "serve-hub" {
        let forward = args.windows(2).find(|pair| pair[0] == "-L").unwrap();
        let words: Vec<_> = forward[1].split(':').collect();
        let listener = TcpListener::bind(format!("{}:{}", words[0], words[1])).unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut bytes = Vec::new();
                loop {
                    let mut chunk = [0; 4096];
                    let count = stream.read(&mut chunk).unwrap_or(0);
                    if count == 0 {
                        break;
                    }
                    bytes.extend_from_slice(&chunk[..count]);
                    if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                        let header = String::from_utf8_lossy(&bytes[..end]);
                        let length = header
                            .lines()
                            .find_map(|line| line.strip_prefix("Content-Length: "))
                            .and_then(|n| n.parse::<usize>().ok())
                            .unwrap_or(0);
                        if bytes.len() >= end + 4 + length {
                            break;
                        }
                    }
                }
                let _ = stream.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                );
                log("report", &[]);
            }
        });
    }
    log("auth", &args);
    let deadline = Instant::now() + Duration::from_secs(120);
    while Instant::now() < deadline {
        if std::env::var_os("INBOX_FAKE_DROP")
            .is_some_and(|path| std::path::Path::new(&path).exists())
        {
            log("drop", &args);
            std::process::exit(255);
        }
        if let Ok((mut stream, _)) = socket.accept() {
            let mut request = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut request)
                .unwrap();
            assert_eq!(request, "forward\n");
            if behavior == "hang-forward" {
                std::thread::sleep(Duration::from_secs(120));
            }
            let held = busy.as_ref().is_some_and(|path| path.exists());
            log(
                if held {
                    "reverse-busy"
                } else {
                    "reverse-ready"
                },
                &args,
            );
            stream
                .write_all(if held { b"busy\n" } else { b"ok\n" })
                .unwrap();
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}
