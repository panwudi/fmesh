//! `fmesh`: one entry point for every install, desktop or headless.
//!
//! `status`, `devices` and `agents` read the same model as the desktop lists (fmesh-core),
//! so a server without a GUI sees exactly what the tray and the native window show, and
//! `--json` gives scripts a stable shape. `agent ...` and `net ...` hand over to the module
//! binaries (agent-inbox, meshd) unchanged; their names, paths and options stay as installed.

mod install;

use fmesh_core::view::{self, Agents, Device, Filter, View, SHARED_NOTE};
use fmesh_core::{directory, labels, modules, status};
use std::path::PathBuf;
use std::process::{Command, ExitCode};

const USAGE: &str = "fmesh status [--json]
fmesh devices [--json] [--online | --offline | --agent-service] [<search>]
fmesh agents [--json] [<search>]
fmesh modules [--json]                (modules on this machine: version, state, updates)
fmesh update check [--json]           (ask the relays for the current release now)
fmesh update desktop|cli|agent        (install a downloaded, verified module update)
fmesh update auto desktop|cli on|off  (root: let meshd install that module's updates itself)
fmesh agent <agent-inbox arguments>   (Agent Mesh: send, who, relay, ...)
fmesh net <meshd arguments>           (Network Mesh: status, up, ...)
fmesh desktop                         (native device and agent list)
fmesh install agent|network|desktop   (one module; --dry-run shows the plan)
fmesh version";

fn main() -> ExitCode {
    fmesh_core::keep_version_mark();
    let mut args = std::env::args().skip(1);
    let cmd = args.next();
    let rest: Vec<String> = args.collect();
    let result = match cmd.as_deref() {
        Some("status") => status_cmd(&rest),
        Some("devices") => devices_cmd(&rest),
        Some("agents") => agents_cmd(&rest),
        Some("modules") => modules_cmd(&rest),
        Some("update") => update_cmd(&rest),
        Some("agent") => hand_over(&agent_inbox(), &rest),
        Some("net") => hand_over(&meshd(), &rest),
        Some("desktop") => hand_over(&desktop(), &rest),
        Some("install") => install::run(&rest),
        Some("version" | "--version") => {
            println!("fmesh {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some("help" | "--help" | "-h") => {
            println!("{USAGE}");
            Ok(())
        }
        _ => Err(format!("usage:\n{USAGE}")),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("fmesh: {e}");
            ExitCode::from(2)
        }
    }
}

fn load() -> View {
    load_with_status().0
}

fn load_with_status() -> (View, status::Status) {
    let (s, _) = status::read(std::path::Path::new(status::STATUS_FILE));
    let l = labels::load();
    let d = directory::query(l.directory_command.as_deref());
    (view::build(&s, &l, &d), s)
}

fn modules_cmd(args: &[String]) -> Result<(), String> {
    let o = opts(args, false)?;
    let (v, s) = load_with_status();
    let list = modules::local(&s, v.devices.first().filter(|d| d.me), &modules::Disk);
    if o.json {
        return print_json(&list);
    }
    for m in &list {
        if m.installed {
            println!("{}  {}  {}", m.name, m.version_text, m.state);
        } else {
            println!("{}  未安装", m.name);
        }
        if m.installed {
            println!("    更新: {}{}", m.policy, if m.update.is_empty() { String::new() } else { format!(" · {}", m.update) });
        }
        if let Some(a) = &m.action {
            println!("    可以: {} (fmesh update {})", a.label, m.id);
        }
        if let Some(h) = &m.install_hint {
            println!("    安装: {h}");
        }
    }
    Ok(())
}

fn update_cmd(args: &[String]) -> Result<(), String> {
    let (sub, rest) = args.split_first().ok_or(format!("usage:\n{USAGE}"))?;
    match sub.as_str() {
        "check" => {}
        "auto" => return auto_cmd(rest),
        "desktop" | "cli" | "agent" => return install_update(sub),
        _ => return Err(format!("unknown update command {sub}\n{USAGE}")),
    }
    let o = opts(rest, false)?;
    if !o.json {
        println!("正在检查 (请组网服务立即问中继, 最多 10 秒)...");
    }
    let c = modules::check_now(std::time::Duration::from_secs(10));
    if o.json {
        let update = match &c {
            modules::Check::Fresh(s) | modules::Check::Timeout(s) | modules::Check::Unsupported(s) => s.update.clone(),
            _ => None,
        };
        print_json(&serde_json::json!({ "ok": c.ok(), "text": c.text(), "update": update }))?;
    } else {
        println!("{}", c.text());
    }
    if c.ok() {
        Ok(())
    } else {
        std::process::exit(1)
    }
}

struct Opts {
    json: bool,
    filter: Filter,
    query: String,
}

fn opts(args: &[String], filters: bool) -> Result<Opts, String> {
    let mut o = Opts { json: false, filter: Filter::All, query: String::new() };
    let mut words = vec![];
    for a in args {
        match a.as_str() {
            "--json" => o.json = true,
            "--online" if filters => o.filter = Filter::Online,
            "--offline" if filters => o.filter = Filter::Offline,
            "--agent-service" if filters => o.filter = Filter::AgentService,
            x if x.starts_with("--") => return Err(format!("unknown option {x}\n{USAGE}")),
            x => words.push(x),
        }
    }
    o.query = words.join(" ");
    Ok(o)
}

fn print_json(v: &impl serde::Serialize) -> Result<(), String> {
    println!("{}", serde_json::to_string_pretty(v).map_err(|e| e.to_string())?);
    Ok(())
}

fn status_cmd(args: &[String]) -> Result<(), String> {
    let o = opts(args, false)?;
    let v = load();
    if o.json {
        return print_json(&serde_json::json!({ "summary": v.summary, "labels_error": v.labels_error }));
    }
    for line in v.summary.lines() {
        println!("{line}");
    }
    if v.summary.network {
        println!("名单 {} · 版本 {}", v.summary.roster, v.summary.release);
    }
    let me = &v.devices[0];
    println!("本机 Agent: {}", me.agents_text());
    if v.labels_error {
        println!("注意: 设备归属配置 ({}) 读取失败", labels::path().display());
    }
    Ok(())
}

/// Installs a downloaded module update (the same action as the window's button).
fn install_update(id: &str) -> Result<(), String> {
    let (v, s) = load_with_status();
    let list = modules::local(&s, v.devices.first().filter(|d| d.me), &modules::Disk);
    let m = list.iter().find(|m| m.id == id).ok_or("unknown module")?;
    let Some(action) = &m.action else {
        println!("{}: 没有可安装的更新. {}", m.name, m.update);
        return Ok(());
    };
    println!("{}: {}...", m.name, action.label);
    let text = modules::run_action(action, m.path.as_deref().map(std::path::Path::new))?;
    println!("{text}");
    Ok(())
}

/// `fmesh update auto <group> on|off`: the root-owned list of groups meshd installs itself.
fn auto_cmd(args: &[String]) -> Result<(), String> {
    let usage = || format!("usage: fmesh update auto desktop|cli on|off [--dir <meshd dir>]\n{USAGE}");
    let (group, on) = match args {
        [g, v, ..] if (g == "desktop" || g == "cli") && (v == "on" || v == "off") => (g.as_str(), v == "on"),
        _ => return Err(usage()),
    };
    let dir = match args.get(2..) {
        Some([flag, d]) if flag == "--dir" => PathBuf::from(d),
        Some([]) | None => PathBuf::from(if cfg!(windows) { r"C:\ProgramData\flytoMesh" } else { "/etc/flytomesh" }),
        _ => return Err(usage()),
    };
    let file = dir.join("auto-update");
    let mut groups: Vec<String> = std::fs::read_to_string(&file).unwrap_or_default().split_whitespace().map(str::to_string).collect();
    groups.retain(|g| g != group);
    if on {
        groups.push(group.to_string());
    }
    std::fs::write(&file, groups.join("\n") + "\n").map_err(|e| format!("cannot write {} (needs root): {e}", file.display()))?;
    println!("{group}: {} (meshd reads {} every 10 s)", if on { "自动更新已开启" } else { "自动更新已关闭, 改为一键更新" }, file.display());
    Ok(())
}

fn devices_cmd(args: &[String]) -> Result<(), String> {
    let o = opts(args, true)?;
    let v = load();
    let shown = v.visible(&o.query, o.filter);
    if o.json {
        return print_json(&shown);
    }
    for d in &shown {
        print_device(d, &o.query);
    }
    println!("显示 {} / {} 台. \"未发现\" 不代表没装; \"可接收\" 不代表有人在电脑前或模型已处理消息.", shown.len(), v.devices.len());
    if v.labels_error {
        println!("注意: 设备归属配置 ({}) 读取失败", labels::path().display());
    }
    Ok(())
}

fn print_device(d: &Device, query: &str) {
    let me = if d.me { " (本机)" } else { "" };
    println!("{}{me}  {}  {}  {}  握手 {}  归属: {}", d.mesh_name, d.ip, d.state_text(), d.path, d.handshake_text(), d.owner_text());
    if d.shared {
        println!("    {SHARED_NOTE}");
    }
    println!("    {} · {}", d.service_text(), d.agents_text());
    println!("    模块: {}", d.modules_text());
    if let Agents::Listed { agents } = &d.agents {
        for a in agents {
            let hit = if d.matches_agent(query) && format!("{} {}", a.name, a.role).to_lowercase().contains(&query.to_lowercase()) { "*" } else { " " };
            let state = if a.receiving { "可接收" } else { "当前不可接收" };
            let role = if a.role.is_empty() { "职责未填写" } else { &a.role };
            println!("   {hit}{}  {state}  {role}", a.name);
        }
    }
}

fn agents_cmd(args: &[String]) -> Result<(), String> {
    let o = opts(args, false)?;
    let v = load();
    let q = o.query.to_lowercase();
    let mut rows = vec![];
    let mut unknown = vec![];
    for d in &v.devices {
        match &d.agents {
            Agents::Listed { agents } => {
                for a in agents.iter().filter(|a| q.is_empty() || format!("{} {} {}", a.name, a.role, d.owner_text()).to_lowercase().contains(&q)) {
                    rows.push((d, a));
                }
            }
            Agents::Unavailable { reason } if d.agent_port.is_some() || d.me => unknown.push((d.name.as_str(), reason.as_str())),
            _ => {}
        }
    }
    if o.json {
        let list: Vec<_> = rows
            .iter()
            .map(|(d, a)| serde_json::json!({ "name": a.name, "role": a.role, "receiving": a.receiving, "device": d.name, "shared": d.shared }))
            .collect();
        let unavailable: Vec<_> = unknown.iter().map(|(d, r)| serde_json::json!({ "device": d, "reason": r })).collect();
        return print_json(&serde_json::json!({ "agents": list, "unavailable": unavailable }));
    }
    for (d, a) in &rows {
        let state = if a.receiving { "可接收" } else { "当前不可接收" };
        let shared = if d.shared { "  [共享: 经成员自己的 Agent 联系]" } else { "" };
        println!("{}  {state}  {}{shared}", a.name, if a.role.is_empty() { "职责未填写" } else { &a.role });
    }
    for (d, r) in &unknown {
        println!("{d}: {r}");
    }
    println!("共 {} 个会话{}", rows.len(), if unknown.is_empty() { String::new() } else { format!(", {} 台名单不可用 (不是 0 个)", unknown.len()) });
    Ok(())
}

/// A binary next to this one, if installed there.
fn sibling(name: &str) -> PathBuf {
    let exe = if cfg!(windows) { format!("{name}.exe") } else { name.to_owned() };
    std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join(&exe))).filter(|p| p.exists()).unwrap_or_else(|| PathBuf::from(exe))
}

fn first_existing(candidates: &[PathBuf], fallback: &str) -> PathBuf {
    candidates.iter().find(|p| p.exists()).cloned().unwrap_or_else(|| sibling(fallback))
}

fn agent_inbox() -> PathBuf {
    let mut c = vec![sibling("agent-inbox")];
    if let Some(home) = std::env::var_os("HOME") {
        c.push(PathBuf::from(home).join(".local/bin/agent-inbox"));
    }
    first_existing(&c, "agent-inbox")
}

fn desktop() -> PathBuf {
    let installed = if cfg!(windows) { r"C:\Program Files\flytoMesh\fmesh-desktop.exe" } else { "/opt/flytomesh/fmesh-desktop" };
    first_existing(&[sibling("fmesh-desktop"), PathBuf::from(installed)], "fmesh-desktop")
}

fn meshd() -> PathBuf {
    let installed = if cfg!(windows) { r"C:\Program Files\flytoMesh\meshd.exe" } else { "/opt/flytomesh/meshd" };
    first_existing(&[sibling("meshd"), PathBuf::from(installed)], "meshd")
}

fn hand_over(exe: &PathBuf, args: &[String]) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let e = Command::new(exe).args(args).exec();
        Err(format!("cannot run {}: {e} (is that module installed?)", exe.display()))
    }
    #[cfg(not(unix))]
    {
        let status = Command::new(exe).args(args).status().map_err(|e| format!("cannot run {}: {e} (is that module installed?)", exe.display()))?;
        std::process::exit(status.code().unwrap_or(1))
    }
}
