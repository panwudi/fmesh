use std::io::{self, BufRead, Write};

fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args == ["--version"] {
        println!("grok 1.0.70 (fake)");
        return Ok(());
    }
    assert_eq!(args.first().map(String::as_str), Some("agent"));
    assert!(args.iter().any(|arg| arg == "--leader"));
    assert_eq!(args.last().map(String::as_str), Some("stdio"));
    for line in io::stdin().lock().lines() {
        let line = line?;
        let id = line
            .split("\"id\":")
            .nth(1)
            .and_then(|part| part.split(|c: char| !c.is_ascii_digit()).next())
            .unwrap_or("0");
        let result = if line.contains("\"method\":\"initialize\"") {
            r#"{"protocolVersion":1,"authMethods":[{"id":"cached_token"}]}"#
        } else if line.contains("\"method\":\"authenticate\"") {
            r#"{}"#
        } else if line.contains("\"method\":\"_x.ai/sessions/list\"") {
            r#"{"result":{"sessions":[{"sessionId":"01a16c01-a2c4-7630-abfe-8b347ab7061b","cwd":"/tmp/project","activity":"working","resident":true,"origin":{"kind":"local"}},{"sessionId":"01a16c0b-7c1c-7350-8d79-a1d0d0f2e944","cwd":"/tmp/old","activity":"dormant","resident":false,"origin":{"kind":"local"}}]}}"#
        } else if line.contains("\"method\":\"_x.ai/interject\"") {
            if let Some(path) = std::env::var_os("AGENT_INBOX_TEST_GROK_LOG") {
                let mut file = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
                writeln!(file, "{line}")?;
            }
            if std::env::var_os("AGENT_INBOX_TEST_GROK_FAIL")
                .is_some_and(|path| std::path::Path::new(&path).exists())
            {
                println!("{{\"jsonrpc\":\"2.0\",\"id\":{id},\"error\":{{\"code\":-32602,\"message\":\"interjection rejected\"}}}}");
                io::stdout().flush()?;
                continue;
            }
            if std::env::var_os("AGENT_INBOX_TEST_GROK_MISSING")
                .is_some_and(|path| std::path::Path::new(&path).exists())
            {
                println!("{{\"jsonrpc\":\"2.0\",\"id\":{id},\"error\":{{\"code\":-32602,\"message\":\"Invalid params\",\"data\":\"session not found: test\"}}}}");
                io::stdout().flush()?;
                continue;
            }
            r#"{"result":{"status":"queued"}}"#
        } else {
            r#"{}"#
        };
        println!("{{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{result}}}");
        io::stdout().flush()?;
    }
    Ok(())
}
