//! The full device list, on demand: a self-contained HTML snapshot of the status file, opened
//! in the default browser. Searching and filtering happen in the page; closing it touches
//! nothing, and the tray (and meshd) never wait on it. No webview, no extra dependency.

use crate::status::{Status, INBOX};
use crate::inventory::Inventory;
use std::fmt::Write as _;

fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&#39;"),
            _ => o.push(c),
        }
    }
    o
}

fn handshake(h: Option<u64>) -> String {
    match h {
        None => "从未".into(),
        Some(s) if s < 120 => format!("{s} 秒前"),
        Some(s) if s < 7200 => format!("{} 分钟前", s / 60),
        Some(s) => format!("{} 小时前", s / 3600),
    }
}

struct Row<'a> {
    name: &'a str,
    ip: &'a str,
    online: bool,
    path: &'a str,
    handshake: String,
    inbox: Option<u16>,
    me: bool,
}

/// One table row; `data-*` attributes drive the page's filters.
fn row(out: &mut String, r: Row, inv: &Inventory) {
    let Row { name, ip, online, path, handshake: hs, inbox, me } = r;
    let state = if online { "在线" } else { "离线" };
    let owner = inv.owners.get(name).map(String::as_str).unwrap_or("未填写");
    let shared = inv.shared_hosts.contains(name);
    let agents = inv.rows(name);
    let mut search = format!("{name} {name}.mesh {ip} {path} {state} {owner}");
    let mut details = String::new();
    if shared {
        details.push_str("<div class=tag>共享 Agent: 请通过员工自己的 Agent 联系</div>");
        search.push_str(" 共享服务器 共享agent");
    }
    let available = online && inbox.is_some() && inv.available(name);
    let svc = match inbox {
        Some(p) => format!("<span class=ok>可用</span> · 端口 {p}"),
        None => "<span class=dim>未发现</span>".into(),
    };
    if available {
        let receiving = agents.iter().filter(|a| a["status"] == "online" && a["inbox_reachable"] != false && a["host_online"] != false).count();
        let _ = write!(details, "<details><summary>{receiving} 个可接收 / {} 个登记会话</summary><ul class=agents>", agents.len());
        for agent in agents {
            let agent_name = agent["name"].as_str().unwrap_or("?");
            let description = agent["description"].as_str().filter(|d| !d.is_empty()).unwrap_or("职责未填写");
            search.push_str(&format!(" {agent_name} {description}"));
            let receiving = agent["status"] == "online" && agent["inbox_reachable"] != false && agent["host_online"] != false;
            let label = if receiving { "可接收" } else { "当前不可接收" };
            let title = if shared { "复制名称, 交给自己的 Agent 联系" } else { "复制 Agent 名称" };
            let _ = write!(details, "<li><button class=copy data-copy=\"{}\" title=\"{title}\">{}</button> <span class=dim>{label}</span><div>{}</div></li>", esc(agent_name), esc(agent_name), esc(description));
        }
        details.push_str("</ul></details>");
    } else {
        let message = if !online { "设备离线, Agent 状态未知" } else if inbox.is_none() { "未发现 Agent 服务公告" } else { inv.error.as_deref().unwrap_or("名单暂不可用: 对端目录未响应") };
        let _ = write!(details, "<span class=dim>{}</span>", esc(message));
    }
    let tag = if me { " <span class=tag>本机</span>" } else { "" };
    let _ = writeln!(
        out,
        "<tr data-online=\"{}\" data-inbox=\"{}\" data-text=\"{}\"><td><button class=copy data-copy=\"{}.mesh\" title=\"复制 {}.mesh\">{}.mesh</button>{tag}<div class=dim>归属: {}</div></td><td>{}</td><td class=\"{}\">{state}</td><td>{}</td><td>{hs}</td><td>{svc}<div class=directory>{details}</div></td></tr>",
        online as u8,
        inbox.is_some() as u8,
        esc(&search.to_lowercase()),
        esc(name),
        esc(name),
        esc(name),
        esc(owner),
        esc(ip),
        if online { "ok" } else { "dim" },
        esc(path),
    );
}

/// `generated_ms`: Unix time in milliseconds, shown in the viewer's local time by the page.
#[cfg(test)]
pub fn html(s: &Status, generated_ms: u64) -> String {
    html_with_inventory(s, generated_ms, &Inventory::default())
}

pub fn html_with_inventory(s: &Status, generated_ms: u64, inv: &Inventory) -> String {
    let mut rows = String::new();
    let inbox_port = |node: &str| s.service(node, INBOX).map(|x| x.port);
    let path = if s.running { "-" } else { "meshd 未运行" };
    row(&mut rows, Row { name: &s.me, ip: &s.ip, online: s.running, path, handshake: "-".into(), inbox: inbox_port(&s.me), me: true }, inv);
    let mut peers: Vec<_> = s.peers.iter().collect();
    peers.sort_by(|a, b| b.online.cmp(&a.online).then(a.name.cmp(&b.name)));
    for p in peers {
        let hs = handshake(p.handshake);
        row(&mut rows, Row { name: &p.name, ip: &p.ip, online: p.online, path: &p.path, handshake: hs, inbox: inbox_port(&p.name), me: false }, inv);
    }
    let relays: Vec<String> =
        s.relays.iter().map(|r| if r.up { format!("{} {}", esc(&r.name), esc(&r.rtt)) } else { format!("{} 断开", esc(&r.name)) }).collect();
    let summary = format!(
        "本机 <b>{}</b> {} · 其他设备在线 {}/{} 台 · 中继 {}/{} ({}) · agent-inbox 可用 {} 台 · 名单 {} · meshd {}",
        esc(&s.me),
        esc(&s.ip),
        s.online(),
        s.peers.len(),
        s.relays_up(),
        s.relays.len(),
        relays.join(", "),
        s.inbox_nodes(),
        esc(&s.roster),
        esc(&s.release),
    );
    let warning = if inv.labels_error { "设备归属配置读取失败, 请检查 devices.json." } else { "" };
    TEMPLATE.replace("{{GENERATED}}", &generated_ms.to_string()).replace("{{SUMMARY}}", &summary).replace("{{ROWS}}", &rows).replace("{{WARNING}}", warning)
}

const TEMPLATE: &str = r#"<!doctype html>
<html lang="zh-CN"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>flytoMesh 设备</title>
<style>
:root { --bg: #fff; --fg: #1d1d1f; --dim: #86868b; --line: #e5e5ea; --ok: #1a7f37; --tag: #e8f0fe; --tagfg: #1a56c4; }
@media (prefers-color-scheme: dark) { :root { --bg: #1c1c1e; --fg: #f2f2f7; --dim: #98989d; --line: #38383a; --ok: #4cd964; --tag: #1f3a5f; --tagfg: #9cc3ff; } }
body { margin: 0; padding: 16px; background: var(--bg); color: var(--fg); font: 14px/1.5 -apple-system, "Segoe UI", "PingFang SC", "Microsoft YaHei", sans-serif; }
h1 { font-size: 18px; margin: 0 0 4px; }
.meta { color: var(--dim); font-size: 12px; margin-bottom: 12px; }
.bar { display: flex; flex-wrap: wrap; gap: 8px; align-items: center; margin: 12px 0; }
input, select { font: inherit; padding: 6px 8px; border: 1px solid var(--line); border-radius: 6px; background: var(--bg); color: var(--fg); }
input { flex: 1; min-width: 180px; }
.wrap { overflow-x: auto; }
table { border-collapse: collapse; width: 100%; }
th, td { text-align: left; padding: 6px 10px; border-bottom: 1px solid var(--line); white-space: nowrap; }
th { color: var(--dim); font-weight: 500; font-size: 12px; }
.ok { color: var(--ok); } .dim { color: var(--dim); }
.tag { background: var(--tag); color: var(--tagfg); border-radius: 4px; padding: 0 6px; font-size: 12px; }
.copy { font: inherit; color: inherit; background: none; border: 0; padding: 0; cursor: pointer; text-decoration: underline dotted; }
#toast { position: fixed; left: 50%; bottom: 24px; transform: translateX(-50%); background: var(--fg); color: var(--bg); padding: 8px 14px; border-radius: 8px; opacity: 0; transition: opacity .2s; pointer-events: none; }
#toast.show { opacity: .92; }
.note { color: var(--dim); font-size: 12px; margin-top: 12px; }
.directory { margin-top: 6px; white-space: normal; min-width: 260px; max-width: 600px; }
summary { cursor: pointer; } .agents { padding-left: 18px; } .agents li { padding: 5px 0; overflow-wrap: anywhere; }
</style></head><body>
<h1>flytoMesh 设备与 Agent</h1>
<div class="meta">快照生成于 <span id="gen"></span>. 这是静态快照: 从托盘菜单重新打开即刷新. 关闭此页不影响组网.</div>
<div>{{SUMMARY}}</div>
<div class="bar">
<input id="q" type="search" placeholder="搜索设备, 归属, Agent 或项目" autofocus>
<select id="f"><option value="all">全部</option><option value="online">只看在线</option><option value="offline">只看离线</option><option value="inbox">agent-inbox 可用</option></select>
<span id="count" class="dim"></span>
</div>
<div class="wrap"><table>
<thead><tr><th>设备与归属</th><th>地址</th><th>状态</th><th>路径</th><th>最近握手</th><th>Agent 服务与会话</th></tr></thead>
<tbody id="rows">
{{ROWS}}</tbody></table></div>
<div class="note">服务公告来自 FlytoMesh, 会话名单来自 agent-inbox 的只读目录. "未发现" 不代表没装; "可接收" 不代表有人在电脑前, 也不代表模型已经处理消息. 归属来自本机明确登记的 devices.json, 未登记显示 "未填写". 职责为会话自述. {{WARNING}}</div>
<div id="toast"></div>
<script>
document.getElementById('gen').textContent = new Date({{GENERATED}}).toLocaleString();
const q = document.getElementById('q'), f = document.getElementById('f'), count = document.getElementById('count');
const rows = Array.from(document.querySelectorAll('#rows tr'));
function apply() {
  const t = q.value.trim().toLowerCase(), m = f.value;
  let n = 0;
  for (const r of rows) {
    const ok = (!t || r.dataset.text.includes(t)) &&
      (m === 'all' || (m === 'online' && r.dataset.online === '1') || (m === 'offline' && r.dataset.online === '0') || (m === 'inbox' && r.dataset.inbox === '1'));
    r.style.display = ok ? '' : 'none';
    const details = r.querySelector('details');
    if (details && t) details.open = ok;
    if (ok) n++;
  }
  count.textContent = '显示 ' + n + ' / ' + rows.length + ' 台';
}
q.addEventListener('input', apply); f.addEventListener('change', apply); apply();
const toast = document.getElementById('toast');
let timer;
function show(msg) { toast.textContent = msg; toast.classList.add('show'); clearTimeout(timer); timer = setTimeout(() => toast.classList.remove('show'), 1800); }
function fallback(text) {
  const a = document.createElement('textarea'); a.value = text; document.body.appendChild(a); a.select();
  let ok = false; try { ok = document.execCommand('copy'); } catch (e) {}
  a.remove(); return ok;
}
document.addEventListener('click', e => {
  const b = e.target.closest('.copy'); if (!b) return;
  const text = b.dataset.copy;
  const done = ok => show(ok ? '已复制 ' + text : '复制失败, 请手动选中复制');
  if (navigator.clipboard && window.isSecureContext) navigator.clipboard.writeText(text).then(() => done(true), () => done(fallback(text)));
  else done(fallback(text));
});
</script>
</body></html>
"#;

const PREFIX: &str = "flytomesh-devices-";
/// Older snapshots of ours are removed when a new one is written.
const KEEP: std::time::Duration = std::time::Duration::from_secs(3600);

/// Writes the snapshot to a new file in this user's temporary directory and opens it in the
/// browser; returns how many devices it lists. The file is created exclusively under a fresh
/// name (an existing file or link there is never followed or overwritten) and readable only
/// by its owner.
pub fn open(s: &Status) -> std::io::Result<usize> {
    let path = save(s)?;
    #[cfg(target_os = "macos")]
    if !std::process::Command::new("open").arg(&path).status()?.success() {
        return Err(std::io::Error::other("open failed"));
    }
    // explorer exits 1 even when it opened the file, so only a failure to start counts.
    #[cfg(windows)]
    std::process::Command::new("explorer").arg(&path).spawn()?;
    Ok(s.peers.len() + 1)
}

/// Export the same private snapshot without launching a browser, including over SSH.
pub fn save(s: &Status) -> std::io::Result<std::path::PathBuf> {
    let dir = std::env::temp_dir();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    let body = html_with_inventory(s, now.as_millis() as u64, &crate::inventory::load());
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut path = dir.join(format!("{PREFIX}{}-{}.html", std::process::id(), now.as_nanos()));
    let mut file = None;
    for k in 0..8 {
        match opts.open(&path) {
            Ok(f) => {
                file = Some(f);
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                path = dir.join(format!("{PREFIX}{}-{}-{k}.html", std::process::id(), now.as_nanos()));
            }
            Err(e) => return Err(e),
        }
    }
    let mut file = file.ok_or_else(|| std::io::Error::other("no free snapshot name"))?;
    use std::io::Write;
    file.write_all(body.as_bytes())?;
    drop(file);
    clean_old(&dir, &path);
    Ok(path)
}

/// Removes our own snapshots older than `KEEP`: plain files with our prefix, and on Unix only
/// those owned by the same user as the one just written (the directory may be shared).
fn clean_old(dir: &std::path::Path, mine: &std::path::Path) {
    let Ok(me) = std::fs::symlink_metadata(mine) else { return };
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let name = e.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with(PREFIX) || !name.ends_with(".html") || e.path() == mine {
            continue;
        }
        let Ok(m) = std::fs::symlink_metadata(e.path()) else { continue };
        #[cfg(unix)]
        let same_owner = {
            use std::os::unix::fs::MetadataExt;
            m.uid() == me.uid()
        };
        #[cfg(windows)]
        let same_owner = {
            let _ = &me;
            true // %TEMP% is per user on Windows
        };
        let old = m.modified().ok().and_then(|t| t.elapsed().ok()).is_some_and(|a| a > KEEP);
        if m.is_file() && same_owner && old {
            std::fs::remove_file(e.path()).ok();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::parse;

    #[test]
    fn directory_groups_agents_and_distinguishes_failure_from_zero() {
        let s = parse("node local 10.249.0.1 roster v14 release v8\nservice local 10.249.0.1 name agent-inbox port 8832 expires 60s uid 501\n", true);
        let mut inv = Inventory::default();
        inv.owners.insert("local".into(), "Owner <&>".into());
        inv.shared_hosts.insert("local".into());
        inv.agents.push(serde_json::json!({"name":"local.project-codex","description":"Maintains <project>","status":"online","inbox_reachable":true}));
        inv.agents.push(serde_json::json!({"name":"other.unrelated","description":"must not leak across hosts","status":"online"}));
        let h = html_with_inventory(&s, 0, &inv);
        assert!(h.contains("Owner &lt;&amp;&gt;"));
        assert!(h.contains("共享 Agent: 请通过员工自己的 Agent 联系"));
        assert!(h.contains("交给自己的 Agent 联系"));
        assert!(h.contains("local.project-codex") && h.contains("Maintains &lt;project&gt;"));
        assert!(h.contains("1 个可接收 / 1 个登记会话"));
        assert!(!h.contains("other.unrelated"));
        inv.error = Some("名单暂不可用".into());
        let h = html_with_inventory(&s, 0, &inv);
        assert!(h.contains("名单暂不可用"));
        assert!(!h.contains("0 个登记会话"));
        inv.error = None;
        inv.agents = vec![serde_json::json!({"name":"local.*","host_marker":true})];
        assert!(html_with_inventory(&s, 0, &inv).contains("对端目录未响应"));
    }

    #[test]
    fn rows_are_escaped_and_marked_for_the_filters() {
        let s = parse(
            "node laptop1 10.249.0.1 id 1 roster v14 release v8 (macos-aarch64) key K=\n\
             peer <x>&y 10.249.0.2 direct 10.0.0.1:41820 handshake 5s tx 0 rx 0 id 2 key J=\n\
             peer desk 10.249.0.10 relay hk handshake never tx 0 rx 0 id 10 key D=\n\
             service <x>&y 10.249.0.2 id 2 name agent-inbox port 8832 ident - expires 100s\n",
            true,
        );
        let h = html(&s, 1_792_000_000_000);
        assert!(!h.contains("<x>&y"), "names are escaped");
        assert!(h.contains("&lt;x&gt;&amp;y.mesh"));
        assert_eq!(h.matches("<tr data-online=").count(), 3);
        assert!(h.contains(r#"data-online="1" data-inbox="1""#), "the announcing peer");
        assert!(h.contains(r#"data-online="0" data-inbox="0""#), "desk: offline, not found");
        assert!(h.contains("未发现") && !h.contains("未安装"));
        assert!(h.contains("其他设备在线 1/2 台") && h.contains("agent-inbox 可用 1 台"));
        assert!(h.contains("new Date(1792000000000)"));
    }

    #[test]
    fn large_lists_render_every_device() {
        let mut text = String::from("node me 10.249.0.1 id 1 roster v9 release v8 (x) key K=\n");
        for i in 0..400 {
            text.push_str(&format!("peer n{i} 10.249.{}.{} relay sh handshake {}s tx 0 rx 0 id {} key K=\n", i / 200 + 1, i % 200 + 2, i % 300, i + 2));
        }
        let h = html(&parse(&text, true), 0);
        assert_eq!(h.matches("<tr data-online=").count(), 401);
        assert!(h.contains("其他设备在线 280/400 台"));
    }
}
