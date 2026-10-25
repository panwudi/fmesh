//! `meshd status --summary`: the tray's view for machines without a desktop. Reads the same
//! status file (docs/network.md section 5) and prints this node, the online count, which nodes
//! announce agent-inbox, and one line per device.

use std::fmt::Write as _;

const ONLINE_HANDSHAKE_SECS: u64 = 180;
const INBOX: &str = "agent-inbox";

fn after<'a>(t: &[&'a str], key: &str) -> Option<&'a str> {
    t.iter().position(|x| *x == key).and_then(|i| t.get(i + 1)).copied()
}

struct Device {
    name: String,
    ip: String,
    online: bool,
    path: String,
    handshake: String,
    services: Vec<String>,
}

pub fn render(text: &str, fresh: bool) -> String {
    let (mut me, mut relays, mut up) = (None::<Device>, 0, 0);
    let mut head = String::new();
    let mut peers: Vec<Device> = vec![];
    let mut services: Vec<(String, String, bool)> = vec![]; // (node, "name:port", local)
    for line in text.lines() {
        let t: Vec<&str> = line.split_whitespace().collect();
        match t.as_slice() {
            ["node", name, ip, rest @ ..] => {
                head = format!(
                    "{name} {ip}  meshd {}  roster {}  release {}",
                    if fresh { "running" } else { "NOT RUNNING (status file is stale)" },
                    after(rest, "roster").unwrap_or("?"),
                    after(rest, "release").unwrap_or("?")
                );
                me = Some(Device { name: name.to_string(), ip: ip.to_string(), online: fresh, path: "this node".into(), handshake: "-".into(), services: vec![] });
            }
            ["relay", _, _, state, ..] => {
                relays += 1;
                up += (fresh && *state == "up") as usize;
            }
            ["peer", name, ip, kind, rest @ ..] => {
                let hs = after(rest, "handshake").unwrap_or("never");
                // A stale file is what meshd saw before it stopped: claim nothing from it.
                let online = fresh && hs.strip_suffix('s').and_then(|h| h.parse::<u64>().ok()).is_some_and(|h| h < ONLINE_HANDSHAKE_SECS);
                let path = match *kind {
                    "direct" | "relay" if online => format!("{kind} {}", rest.first().unwrap_or(&"?")),
                    _ => "-".into(),
                };
                peers.push(Device { name: name.to_string(), ip: ip.to_string(), online, path, handshake: hs.to_string(), services: vec![] });
            }
            ["service", node, _ip, rest @ ..] => {
                let live = fresh && after(rest, "expires").and_then(|e| e.strip_suffix('s')).and_then(|e| e.parse::<u64>().ok()).is_some_and(|e| e > 0);
                if let (true, Some(name), Some(port)) = (live, after(rest, "name"), after(rest, "port")) {
                    services.push((node.to_string(), format!("{name}:{port}"), after(rest, "uid").is_some()));
                }
            }
            _ => {}
        }
    }
    let Some(mut me) = me else { return "meshd: no status yet (is meshd running?)\n".into() };
    for (node, svc, local) in services {
        let d = if local { Some(&mut me) } else { peers.iter_mut().find(|p| p.name == node && p.online) };
        if let Some(d) = d {
            d.services.push(svc);
        }
    }
    peers.sort_by(|a, b| b.online.cmp(&a.online).then(a.name.cmp(&b.name)));
    let online = peers.iter().filter(|p| p.online).count();
    let inbox: Vec<String> = std::iter::once(&me)
        .chain(&peers)
        .filter(|d| d.services.iter().any(|s| s.split(':').next() == Some(INBOX)))
        .map(|d| if std::ptr::eq(d, &me) { format!("{} (this node)", d.name) } else { d.name.clone() })
        .collect();
    let mut o = String::new();
    let _ = writeln!(o, "{head}");
    let _ = writeln!(o, "relays {up}/{relays} up, other nodes online {online}/{}", peers.len());
    let _ = match inbox.len() {
        0 => writeln!(o, "{INBOX}: not announced by any online node"),
        n => writeln!(o, "{INBOX}: announced by {n}: {}", inbox.join(", ")),
    };
    let _ = writeln!(o);
    let all = || std::iter::once(&me).chain(&peers);
    let w = all().map(|d| d.name.len() + 5).max().unwrap_or(0).max(6);
    let pw = all().map(|d| d.path.len()).max().unwrap_or(0).max(4);
    let _ = writeln!(o, "{:w$}  {:<15} {:<8} {:pw$}  {:<9} SERVICES", "DEVICE", "IP", "STATE", "PATH", "HANDSHAKE");
    for d in all() {
        let svc = if d.services.is_empty() { "-".to_string() } else { d.services.join(",") };
        let state = if d.online { "online" } else { "offline" };
        let _ = writeln!(o, "{:w$}  {:<15} {:<8} {:pw$}  {:<9} {svc}", format!("{}.mesh", d.name), d.ip, state, d.path, d.handshake);
    }
    o
}

#[cfg(test)]
mod tests {
    use super::render;

    #[test]
    fn summary_matches_the_tray_rules() {
        let out = render(
            "node laptop1 10.249.0.1 id 1 roster v14 release v8 (macos-aarch64) key K=\n\
             relay sh 1.2.3.4:41900 up rtt 42ms sees-us -\n\
             relay hk 1.2.3.5:41900 down rtt - sees-us -\n\
             peer laptop2 10.249.0.2 direct 10.0.0.1:41820 handshake 5s tx 0 rx 0 id 2 key J=\n\
             peer desk 10.249.0.10 relay hk handshake never tx 0 rx 0 id 10 key D=\n\
             service laptop1 10.249.0.1 id 1 name agent-inbox port 8832 ident - expires 60s uid 501\n\
             service laptop2 10.249.0.2 id 2 name agent-inbox port 8832 ident - expires 100s\n\
             service desk 10.249.0.10 id 10 name agent-inbox port 8832 ident - expires 100s\n",
            true,
        );
        assert!(out.contains("relays 1/2 up, other nodes online 1/2"), "{out}");
        assert!(out.contains("agent-inbox: announced by 2: laptop1 (this node), laptop2"), "{out}");
        let desk = out.lines().find(|l| l.starts_with("desk.mesh")).unwrap();
        assert!(desk.contains("offline") && desk.trim_end().ends_with('-'), "an offline node's entry is not shown: {desk}");
        assert!(out.lines().any(|l| l.starts_with("laptop2.mesh") && l.contains("direct 10.0.0.1:41820") && l.ends_with("agent-inbox:8832")));
    }

    #[test]
    fn stale_or_missing_status() {
        let stale = render(
            "node a 10.249.0.3 id 3 roster v1 release v7 (x)\nrelay sh 1.2.3.4:41900 up rtt 1ms sees-us -\n\
             peer b 10.249.0.4 direct 1.2.3.4:41820 handshake 5s tx 0 rx 0\n\
             service b 10.249.0.4 id 4 name agent-inbox port 8832 ident - expires 100s\n",
            false,
        );
        assert!(stale.contains("NOT RUNNING"), "{stale}");
        assert!(stale.contains("relays 0/1 up, other nodes online 0/1") && stale.contains("not announced"), "{stale}");
        assert!(render("", true).contains("no status yet"));
    }
}
