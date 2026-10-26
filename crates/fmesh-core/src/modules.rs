//! The modules on this machine: which are installed, their versions, whether they run and how
//! they are updated. Same model for `fmesh modules` and the desktop window.
//!
//! Versions come from what each program reports without running GUI programs: meshd from its
//! status file, agent-inbox and fmesh from `--version`, the tray and the list window from the
//! version mark compiled into them (`VERSION_MARK`). Builds before 0.11 carry no mark and
//! report no update state; they show as "unknown", never as a guessed version.

use crate::status::{ModuleFile, Status, Update};
use crate::view::{Agents, Device};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

/// Network Mesh, desktop and CLI binaries.
#[cfg(windows)]
pub const SYSTEM_DIR: &str = r"C:\Program Files\flytoMesh";
#[cfg(not(windows))]
pub const SYSTEM_DIR: &str = "/opt/flytomesh";

/// meshd's service registration directory, which also takes local requests (meshd services.rs).
#[cfg(windows)]
pub const SERVICES_DIR: &str = r"C:\ProgramData\flytoMesh\services";
#[cfg(not(windows))]
pub const SERVICES_DIR: &str = "/var/run/flytomesh.services";

const MARK_PREFIX: &[u8] = b"fmesh-version ";

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Module {
    /// `network`, `agent`, `desktop` or `cli`.
    pub id: &'static str,
    pub name: &'static str,
    pub installed: bool,
    pub path: Option<String>,
    /// `None`: installed but the version is not known (older build or no report).
    pub version: Option<String>,
    /// The version for display: `v0.11.0`, `v0.11.0 · 发布 v11`, `发布 v10`, `版本未知` or
    /// `未安装`.
    pub version_text: String,
    /// Running state in words ("运行中 · 中继 3/3", "未运行", ...).
    pub state: String,
    pub ok: bool,
    /// How this module is updated.
    pub policy: String,
    /// Update state in words.
    pub update: String,
    /// What the user can do now (e.g. install a downloaded update).
    pub action: Option<Action>,
    /// What to run to install it, when it is not installed.
    pub install_hint: Option<String>,
}

/// What `local` needs to know about the files on disk; tests replace it.
pub trait Probe {
    /// The first existing candidate for a module binary.
    fn find(&self, binary: &str) -> Option<PathBuf>;
    /// The version a binary reports, if it can tell.
    fn version(&self, binary: &str, path: &Path) -> Option<String>;
}

pub struct Disk;

fn exe(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

impl Probe for Disk {
    fn find(&self, binary: &str) -> Option<PathBuf> {
        let mut c = vec![PathBuf::from(SYSTEM_DIR).join(exe(binary))];
        if let Some(home) = std::env::var_os("HOME") {
            c.push(PathBuf::from(home).join(".local/bin").join(exe(binary)));
        }
        if binary == "fmesh" && cfg!(unix) {
            c.push(PathBuf::from("/usr/local/bin/fmesh"));
        }
        c.into_iter().find(|p| p.is_file())
    }

    fn version(&self, binary: &str, path: &Path) -> Option<String> {
        match binary {
            // Command-line programs answer `--version` in every release.
            "agent-inbox" | "fmesh" => version_flag(path),
            // GUI programs before 0.11 ignore unknown flags and would open a second tray or
            // window, so they are never run: their version is read from the file.
            _ => version_mark(path),
        }
    }
}

/// `<name> <version>` from `path --version`, within two seconds.
fn version_flag(path: &Path) -> Option<String> {
    let mut cmd = Command::new(path);
    cmd.arg("--version").stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW: no console flash from the GUI
    }
    let mut child = cmd.spawn().ok()?;
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match child.try_wait().ok()? {
            Some(st) if st.success() => break,
            Some(_) => return None,
            None if Instant::now() > deadline => {
                child.kill().ok();
                child.wait().ok();
                return None;
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    }
    let mut out = String::new();
    std::io::Read::read_to_string(&mut child.stdout.take()?, &mut out).ok()?;
    let v = out.split_whitespace().nth(1)?;
    valid_version(v).then(|| v.to_string())
}

/// The `VERSION_MARK` compiled into a binary.
fn version_mark(path: &Path) -> Option<String> {
    let data = std::fs::read(path).ok()?;
    find_mark(&data)
}

pub fn find_mark(data: &[u8]) -> Option<String> {
    let at = data.windows(MARK_PREFIX.len()).position(|w| w == MARK_PREFIX)? + MARK_PREFIX.len();
    let rest = &data[at..data.len().min(at + 24)];
    let end = rest.iter().position(|&b| b == b';')?;
    let v = std::str::from_utf8(&rest[..end]).ok()?;
    valid_version(v).then(|| v.to_string())
}

fn valid_version(v: &str) -> bool {
    let parts: Vec<&str> = v.split('.').collect();
    parts.len() == 3 && parts.iter().all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

/// Every module, in a fixed order. `me` is this machine's entry in the device view.
pub fn local(s: &Status, me: Option<&Device>, probe: &dyn Probe) -> Vec<Module> {
    let mut list = vec![network(s, probe), agent(s, me, probe), desktop(s, probe), cli(s, probe)];
    for m in &mut list {
        if m.version_text.is_empty() {
            m.version_text = match (&m.version, m.installed) {
                (Some(v), _) if v.starts_with(|c: char| c.is_ascii_digit()) => format!("v{v}"),
                (Some(v), _) => v.clone(),
                (None, true) => "版本未知".into(),
                (None, false) => "未安装".into(),
            };
        }
    }
    list
}

fn network(s: &Status, probe: &dyn Probe) -> Module {
    let path = probe.find("meshd");
    let installed = path.is_some() || s.running;
    let version = s.update.as_ref().map(|u| u.version.clone()).filter(|v| !v.is_empty());
    let state = if s.running {
        format!("运行中 · 中继 {}/{}", s.relays_up(), s.relays.len())
    } else if installed {
        "未运行".into()
    } else {
        "未安装".into()
    };
    let update = if !installed {
        String::new()
    } else if !s.running {
        "组网服务未运行, 无法检查更新".into()
    } else {
        match &s.update {
            Some(u) => network_update(u),
            None => format!("当前版本 {} 不报告更新状态 (0.11 之前), 仍会自动更新", s.release),
        }
    };
    let release = s.release.strip_prefix('v').filter(|r| !r.is_empty() && *r != "0");
    let version_text = match (&version, release) {
        (Some(v), Some(r)) => format!("v{v} · 发布 v{r}"),
        (None, Some(r)) if installed => format!("发布 v{r}"),
        _ => String::new(),
    };
    Module {
        id: "network",
        name: "组网",
        version_text,
        installed,
        path: path.map(|p| p.display().to_string()),
        version,
        ok: s.running,
        state,
        policy: "自动更新 (管理员统一发版)".into(),
        update,
        action: None,
        install_hint: (!installed).then(|| {
            if cfg!(windows) { "以管理员身份运行 install.ps1 -Module network -Name <名字>" } else { "sudo fmesh install network --name <名字>" }.into()
        }),
    }
}

/// One short line for the tray menu: the network module's version and update state.
pub fn update_short(s: &Status) -> String {
    if !s.running {
        return "组网未运行, 无法检查更新".into();
    }
    let Some(u) = &s.update else { return format!("组网发布 {} · 自动更新", s.release) };
    match (u.state.as_str(), u.latest, u.progress) {
        ("downloading", Some(v), Some((a, b))) => format!("正在更新到 v{v}: {a}/{b}"),
        ("paused", Some(v), _) => format!("v{v} 下载暂停, 稍后继续"),
        ("trial", _, _) => format!("已更新到 v{}, 正在启动检查", u.release),
        (_, Some(v), _) if u.bad == Some(v) => format!("v{v} 启动失败, 已退回 v{}", u.release),
        (_, Some(v), _) if v > u.release => format!("发现新版本 v{v}"),
        (_, Some(_), _) => format!("v{} (发布 v{}) · 已是最新", u.version, u.release),
        _ => format!("v{} · 暂时问不到中继", u.version),
    }
}

/// The update state of meshd in words.
pub fn network_update(u: &Update) -> String {
    let heard = match (u.latest, u.age) {
        (Some(v), Some(age)) => format!("中继 {} {} 秒前宣告最新版本 v{v}", u.via, age),
        _ => "还没有中继宣告版本 (中继都连不上?)".into(),
    };
    let mut text = match (u.state.as_str(), u.latest, u.progress) {
        ("downloading", Some(v), Some((a, b))) => format!("正在下载 v{v}: {a}/{b}"),
        ("paused", Some(v), Some((a, b))) => format!("v{v} 下载暂停在 {a}/{b}, 下次宣告时继续"),
        ("checking", Some(v), _) => format!("发现 v{v}, 正在取发布清单"),
        ("trial", _, _) => format!("已装上 v{}, 正在做启动检查 (连上中继即通过)", u.release),
        (_, Some(v), _) if u.bad == Some(v) => format!("v{v} 启动检查失败, 已退回 v{}", u.release),
        (_, Some(v), _) if v > u.release => format!("发现 v{v}, 等待下载"),
        (_, Some(_), _) => format!("已是最新 v{} · {heard}", u.release),
        _ => heard.clone(),
    };
    if let Some(e) = &u.error {
        text += &format!(" · 上次出错: {e}");
    }
    text
}

/// The update state of a group of module files (desktop: tray + list; CLI; agent) from meshd's
/// `module` lines, and what the user can do about it.
fn group_update(s: &Status, group: &str, files: &[&str], same_as_installed: impl Fn(&ModuleFile) -> bool) -> (String, Option<Action>) {
    let Some(u) = s.update.as_ref().filter(|_| s.running) else {
        return ("组网服务没有运行或版本较旧 (0.12 之前), 不能自动下载; 请手动更新".into(), None);
    };
    let lines: Vec<&ModuleFile> = s.module_files.iter().filter(|m| files.contains(&m.name.as_str())).collect();
    let Some(first) = lines.first() else {
        return (format!("当前发布 (v{}) 还没有这个模块的文件, 或组网服务还没取到发布清单", u.release), None);
    };
    let v = format!("v0.{}.0", first.release);
    if let Some(m) = lines.iter().find(|m| m.state == "failed") {
        return (format!("{v} 更新失败: {}", m.error.clone().unwrap_or_default()), None);
    }
    if let Some(m) = lines.iter().find(|m| m.state == "downloading") {
        let p = m.progress.map(|(a, b)| format!(": {a}/{b}")).unwrap_or_default();
        return (format!("正在下载 {v}{p}"), None);
    }
    if lines.iter().any(|m| m.state == "waiting") {
        return (format!("{v} 等待下载 (组网服务先更新自己, 再逐个取模块文件)"), None);
    }
    let pending: Vec<&&ModuleFile> = lines.iter().filter(|m| m.state == "ready" && !same_as_installed(m)).collect();
    if pending.is_empty() {
        return (format!("已是最新 {v} (发布 v{})", first.release), None);
    }
    if u.auto.iter().any(|g| g == group) {
        return (format!("{v} 已下载并校验, 正在自动安装"), None);
    }
    let action = if group == "agent" {
        Action { label: format!("更新到 {v}"), kind: ActionKind::InstallAgent { from: pending[0].path.clone() } }
    } else {
        Action { label: format!("更新到 {v}"), kind: ActionKind::Request { word: format!("install-{group}"), files: files.iter().map(|f| f.to_string()).collect() } }
    };
    (format!("{v} 已下载并校验, 可以更新"), Some(action))
}

/// Whether two files have the same bytes (the user's agent-inbox and the cached release file).
fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(x), Ok(y)) if x.len() == y.len() => std::fs::read(a).ok() == std::fs::read(b).ok(),
        _ => false,
    }
}

fn agent(s: &Status, me: Option<&Device>, probe: &dyn Probe) -> Module {
    let path = probe.find("agent-inbox");
    let installed = path.is_some();
    let version = path.as_deref().and_then(|p| probe.version("agent-inbox", p));
    let (state, ok) = match (path.is_some(), me.map(|d| &d.agents)) {
        (false, _) => ("未安装".to_string(), false),
        (true, Some(Agents::Listed { agents })) => {
            let receiving = agents.iter().filter(|a| a.receiving).count();
            (format!("运行中 · {receiving} 个可接收 / {} 个登记会话", agents.len()), true)
        }
        (true, Some(Agents::Unavailable { reason })) => (format!("本机 Agent 目录不可用: {reason}"), false),
        (true, _) => ("已安装 · 运行状态未知".to_string(), false),
    };
    let (update, action) = match &path {
        None => (String::new(), None),
        Some(_) if !s.running && s.update.is_none() => ("没有装组网模块, 暂无更新来源; 请用发布包里的 fmesh install agent --upgrade".into(), None),
        Some(p) => group_update(s, "agent", &["agent-inbox"], |m| same_file(p, Path::new(&m.path))),
    };
    Module {
        id: "agent",
        name: "Agent",
        version_text: String::new(),
        installed,
        path: path.map(|p| p.display().to_string()),
        version,
        state,
        ok,
        policy: "一键更新 (由本人确认, 会重启本机 Agent 后台服务)".into(),
        update,
        action,
        install_hint: (!installed).then(|| if cfg!(windows) { "Windows 暂不支持 Agent 模块".into() } else { "fmesh install agent (不需要 root)".into() }),
    }
}

fn auto_policy(s: &Status, group: &str) -> String {
    if s.update.as_ref().is_some_and(|u| u.auto.iter().any(|g| g == group)) {
        "自动更新 (组网服务下载并安装)".into()
    } else {
        "一键更新 (组网服务自动下载, 点击安装)".into()
    }
}

fn desktop(s: &Status, probe: &dyn Probe) -> Module {
    let tray = probe.find("meshtray");
    let list = probe.find("fmesh-desktop");
    let installed = tray.is_some() || list.is_some();
    let tv = tray.as_deref().and_then(|p| probe.version("meshtray", p));
    let lv = list.as_deref().and_then(|p| probe.version("fmesh-desktop", p));
    let version = match (&tv, &lv) {
        (Some(a), Some(b)) if a != b => Some(format!("托盘 {a} · 列表 {b}")),
        (Some(a), _) | (None, Some(a)) => Some(a.clone()),
        _ => None,
    };
    let state = match (&tray, &list) {
        (Some(_), Some(_)) => "已安装 (托盘 + 设备列表)".to_string(),
        (Some(_), None) => "已安装托盘, 缺设备列表程序".to_string(),
        (None, Some(_)) => "已安装设备列表, 没有托盘".to_string(),
        (None, None) => "未安装".to_string(),
    };
    let (update, action) = if installed { group_update(s, "desktop", &["meshtray", "fmesh-desktop"], |m| m.state == "current") } else { (String::new(), None) };
    Module {
        id: "desktop",
        name: "桌面",
        version_text: String::new(),
        installed,
        path: tray.or(list).map(|p| p.display().to_string()),
        version,
        ok: installed,
        state,
        policy: auto_policy(s, "desktop"),
        update,
        action,
        install_hint: (!installed).then(|| {
            if cfg!(target_os = "linux") {
                "Linux 没有桌面模块, 用 fmesh 命令行".to_string()
            } else if cfg!(windows) {
                "以管理员身份运行 install.ps1 -Module desktop".to_string()
            } else {
                "sudo fmesh install desktop".to_string()
            }
        }),
    }
}

fn cli(s: &Status, probe: &dyn Probe) -> Module {
    let path = probe.find("fmesh");
    let version = path.as_deref().and_then(|p| probe.version("fmesh", p));
    // Only the copy next to meshd is updated through the mesh.
    let system = path.as_deref().is_some_and(|p| p.starts_with(SYSTEM_DIR));
    let (update, action) = if system { group_update(s, "cli", &["fmesh"], |m| m.state == "current") } else { (String::new(), None) };
    Module {
        id: "cli",
        name: "命令行",
        version_text: String::new(),
        installed: path.is_some(),
        version,
        state: if path.is_some() { "已安装".into() } else { "未安装".into() },
        ok: path.is_some(),
        path: path.map(|p| p.display().to_string()),
        policy: if system { auto_policy(s, "cli") } else { "随 Agent 模块安装".into() },
        update,
        action,
        install_hint: None,
    }
}

/// What a module card offers right now.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Action {
    pub label: String,
    #[serde(flatten)]
    pub kind: ActionKind,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ActionKind {
    /// Ask meshd to install these files of the current release (desktop, CLI).
    Request { word: String, files: Vec<String> },
    /// Copy the verified cached agent-inbox over the user's own and restart its services.
    InstallAgent { from: String },
}

/// Carries out an action; the text says what happened, also on failure.
pub fn run_action(a: &Action, target: Option<&Path>) -> Result<String, String> {
    match &a.kind {
        ActionKind::Request { word, files } => request_install(word, files),
        ActionKind::InstallAgent { from } => install_agent(Path::new(from), target.ok_or("找不到本机的 agent-inbox")?),
    }
}

fn write_request(word: &str) -> Result<PathBuf, String> {
    let nonce = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let req = PathBuf::from(SERVICES_DIR).join(format!(".request-{}-{nonce}", std::process::id()));
    std::fs::write(&req, format!("{word}\n")).map_err(|e| format!("写不进请求目录 {SERVICES_DIR}: {e}"))?;
    Ok(req)
}

/// Asks meshd to install a group and waits until its files are current (or failed).
fn request_install(word: &str, files: &[String]) -> Result<String, String> {
    let req = write_request(word)?;
    let start = Instant::now();
    let result = loop {
        std::thread::sleep(Duration::from_millis(500));
        let s = crate::status::read(Path::new(crate::status::STATUS_FILE)).0;
        let mine: Vec<&ModuleFile> = s.module_files.iter().filter(|m| files.contains(&m.name)).collect();
        if let Some(m) = mine.iter().find(|m| m.state == "failed") {
            break Err(format!("安装失败: {}", m.error.clone().unwrap_or_default()));
        }
        if !mine.is_empty() && mine.iter().all(|m| m.state == "current") {
            break Ok(format!("已安装 v0.{}.0", mine[0].release));
        }
        if start.elapsed() > Duration::from_secs(20) {
            break Err("组网服务 20 秒内没有完成安装 (组网服务在运行吗?)".into());
        }
    };
    std::fs::remove_file(&req).ok();
    result
}

/// Replaces the user's agent-inbox with the verified cached file, keeping the old one as
/// `<target>.prev`, then restarts the user's own background services that run it. Processes
/// started by Claude or Codex sessions keep the old program until their session reconnects.
pub fn install_agent(from: &Path, target: &Path) -> Result<String, String> {
    let mut new = target.as_os_str().to_owned();
    new.push(".fmesh-new");
    let new = PathBuf::from(new);
    std::fs::copy(from, &new).map_err(|e| format!("复制 {}: {e}", from.display()))?;
    if !same_file(from, &new) {
        std::fs::remove_file(&new).ok();
        return Err("复制出的文件和发布文件不一致".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&new, std::fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
    }
    let mut prev = target.as_os_str().to_owned();
    prev.push(".prev");
    std::fs::copy(target, PathBuf::from(prev)).map_err(|e| format!("备份旧程序: {e}"))?;
    std::fs::rename(&new, target).map_err(|e| format!("替换 {}: {e}", target.display()))?;
    let (restarted, manual) = restart_agent_services(target);
    let mut text = String::from("已更新 agent-inbox");
    if !restarted.is_empty() {
        text += &format!(", 已重启 {} 个后台服务 ({})", restarted.len(), restarted.join(", "));
    }
    if !manual.is_empty() {
        text += &format!("; 需要管理员重启: {}", manual.join(", "));
    }
    text += ". 正在运行的 Claude / Codex 会话重连 MCP 后用上新版";
    Ok(text)
}

fn output(cmd: &str, args: &[&str]) -> Option<String> {
    let o = Command::new(cmd).args(args).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    o.status.success().then(|| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

/// Restarts this user's always-on services that run `target`: launchd agents on macOS,
/// `systemctl --user` units on Linux. System units running it are listed for an admin.
fn restart_agent_services(target: &Path) -> (Vec<String>, Vec<String>) {
    let t = target.to_string_lossy();
    let (mut restarted, mut manual) = (vec![], vec![]);
    if cfg!(target_os = "macos") {
        let uid = output("id", &["-u"]).unwrap_or_default();
        let Some(home) = std::env::var_os("HOME") else { return (restarted, manual) };
        let dir = PathBuf::from(home).join("Library/LaunchAgents");
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let f = e.path().to_string_lossy().into_owned();
            if !f.ends_with(".plist") || output("plutil", &["-extract", "ProgramArguments.0", "raw", "-o", "-", &f]).as_deref() != Some(t.as_ref()) {
                continue;
            }
            // Only jobs kept running (relay, watchers); scheduled ones pick it up next run.
            if output("plutil", &["-extract", "KeepAlive", "raw", "-o", "-", &f]).is_none_or(|k| k == "false") {
                continue;
            }
            let Some(label) = output("plutil", &["-extract", "Label", "raw", "-o", "-", &f]) else { continue };
            if output("launchctl", &["kickstart", "-k", &format!("gui/{uid}/{label}")]).is_some() {
                restarted.push(label);
            } else {
                manual.push(label);
            }
        }
    } else if cfg!(target_os = "linux") {
        for (user, list) in [(true, output("systemctl", &["--user", "list-units", "--type=service", "--state=running", "--no-legend", "--plain"])), (false, output("systemctl", &["list-units", "--type=service", "--state=running", "--no-legend", "--plain"]))] {
            for unit in list.unwrap_or_default().lines().filter_map(|l| l.split_whitespace().next()) {
                let mut args = vec!["show", "-p", "ExecStart", "--value", unit];
                if user {
                    args.insert(0, "--user");
                }
                if !output("systemctl", &args).is_some_and(|x| x.contains(t.as_ref())) {
                    continue;
                }
                if user && output("systemctl", &["--user", "restart", unit]).is_some() {
                    restarted.push(unit.to_string());
                } else {
                    manual.push(unit.to_string());
                }
            }
        }
    }
    (restarted, manual)
}

/// The result of "check for updates" (`check_now`).
#[derive(Debug, Clone, PartialEq)]
pub enum Check {
    /// A relay answered after the request: the status holds a fresh announcement.
    Fresh(Box<Status>),
    /// meshd is not running, so nothing can be asked.
    NotRunning,
    /// meshd before 0.11 does not take requests; it still asks the relays every 25 seconds.
    Unsupported(Box<Status>),
    /// No relay answered in time.
    Timeout(Box<Status>),
    /// The request could not be written.
    Failed(String),
}

/// Asks meshd to ask the relays now and waits up to `timeout` for an announcement that came
/// after the request.
pub fn check_now(timeout: Duration) -> Check {
    let read = || crate::status::read(Path::new(crate::status::STATUS_FILE)).0;
    let s = read();
    if !s.running {
        return Check::NotRunning;
    }
    if s.update.is_none() {
        return Check::Unsupported(Box::new(s));
    }
    let req = match write_request("check") {
        Ok(r) => r,
        Err(e) => return Check::Failed(e),
    };
    let start = Instant::now();
    let result = loop {
        std::thread::sleep(Duration::from_millis(400));
        let s = read();
        let elapsed = start.elapsed().as_secs() + 1;
        let fresh = s.update.as_ref().and_then(|u| u.age).zip(s.file_age).is_some_and(|(a, f)| a + f <= elapsed);
        if s.running && fresh {
            break Check::Fresh(Box::new(s));
        }
        if start.elapsed() > timeout {
            break if s.running { Check::Timeout(Box::new(s)) } else { Check::NotRunning };
        }
    };
    // meshd deletes the request when it reads it; one that is still there was never read.
    std::fs::remove_file(&req).ok();
    result
}

impl Check {
    /// One line for the status bar or the terminal.
    pub fn text(&self) -> String {
        match self {
            Check::Fresh(s) => match &s.update {
                Some(u) => format!("已检查: {}", network_update(u)),
                None => "已检查".into(),
            },
            Check::NotRunning => "检查失败: 组网服务没有运行".into(),
            Check::Unsupported(_) => "这个版本的组网服务不支持立即检查; 它每 25 秒自动问一次中继".into(),
            Check::Timeout(_) => "检查失败: 10 秒内没有中继应答 (网络断开或中继都连不上)".into(),
            Check::Failed(e) => format!("检查失败: {e}"),
        }
    }

    pub fn ok(&self) -> bool {
        matches!(self, Check::Fresh(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct Fake(HashMap<&'static str, Option<&'static str>>);

    impl Probe for Fake {
        fn find(&self, b: &str) -> Option<PathBuf> {
            self.0.contains_key(b).then(|| PathBuf::from(format!("/x/{b}")))
        }
        fn version(&self, b: &str, _: &Path) -> Option<String> {
            self.0.get(b).copied().flatten().map(str::to_string)
        }
    }

    fn status(text: &str) -> Status {
        crate::status::parse(text, true)
    }

    #[test]
    fn mark_is_found_and_checked() {
        let mut bin = vec![0u8; 1000];
        bin.extend_from_slice(crate::VERSION_MARK.as_bytes());
        bin.extend_from_slice(&[0, 1, 2]);
        assert_eq!(find_mark(&bin).as_deref(), Some(crate::VERSION));
        assert_eq!(find_mark(b"xx fmesh-version 1.2;"), None, "not x.y.z");
        assert_eq!(find_mark(b"nothing here"), None);
    }

    #[test]
    fn modules_with_versions_and_states() {
        let s = status(
            "node laptop1 10.249.0.1 id 1 roster v3 release v11 (macos-aarch64) key K=\n\
             update version 0.11.0 release 11 latest 11 via sh age 4 state idle\n\
             relay sh 203.0.113.10:41900 up rtt 1ms sees-us -\n",
        );
        let probe = Fake(HashMap::from([("meshd", None), ("meshtray", Some("0.11.0")), ("fmesh-desktop", Some("0.11.0")), ("fmesh", Some("0.11.0"))]));
        let m = local(&s, None, &probe);
        let ids: Vec<&str> = m.iter().map(|x| x.id).collect();
        assert_eq!(ids, ["network", "agent", "desktop", "cli"]);
        assert_eq!((m[0].version.as_deref(), m[0].state.as_str()), (Some("0.11.0"), "运行中 · 中继 1/1"));
        assert_eq!(m[0].version_text, "v0.11.0 · 发布 v11");
        assert_eq!((m[1].version_text.as_str(), m[2].version_text.as_str()), ("未安装", "v0.11.0"));
        assert!(m[0].update.starts_with("已是最新 v11 · 中继 sh 4 秒前宣告最新版本 v11"), "{}", m[0].update);
        assert!(!m[1].installed && m[1].install_hint.is_some());
        assert_eq!((m[2].version.as_deref(), m[2].installed), (Some("0.11.0"), true));
        assert_eq!(m[3].version.as_deref(), Some("0.11.0"));
    }

    #[test]
    fn unknown_is_not_guessed() {
        let old = status("node laptop1 10.249.0.1 id 1 roster v3 release v10 (macos-aarch64) key K=\n");
        let probe = Fake(HashMap::from([("meshd", None), ("meshtray", None), ("fmesh-desktop", Some("0.11.0"))]));
        let m = local(&old, None, &probe);
        assert_eq!((m[0].version.as_ref(), m[0].version_text.as_str()), (None, "发布 v10"));
        assert!(m[0].update.contains("不报告更新状态"));
        assert_eq!(m[2].version.as_deref(), Some("0.11.0"), "the list has a mark, the old tray not");
        let stopped = Status::default();
        let m = local(&stopped, None, &probe);
        assert_eq!((m[0].state.as_str(), m[0].ok), ("未运行", false));
    }

    #[test]
    fn tray_line() {
        let s = |t: &str| crate::status::parse(t, true);
        assert_eq!(update_short(&s("update version 0.11.0 release 11 latest 11 via sh age 1 state idle\n")), "v0.11.0 (发布 v11) · 已是最新");
        assert_eq!(update_short(&s("update version 0.11.0 release 11 latest 12 via sh age 1 state downloading progress 1/9\n")), "正在更新到 v12: 1/9");
        assert_eq!(update_short(&s("node a 10.249.0.1 id 1 roster v1 release v10 (x) key K=\n")), "组网发布 v10 · 自动更新");
        assert_eq!(update_short(&crate::status::parse("", false)), "组网未运行, 无法检查更新");
    }

    #[test]
    fn module_updates_offer_an_action_only_when_ready() {
        let st = |extra: &str| status(&format!("node a 10.249.0.1 id 1 roster v3 release v12 (x) key K=\nupdate version 0.12.0 release 12 state idle\n{extra}"));
        let files = ["meshtray", "fmesh-desktop"];
        let (t, a) = group_update(&st("module meshtray ready release 12 sha256 ab path /c/ab\nmodule fmesh-desktop current release 12\n"), "desktop", &files, |m| m.state == "current");
        assert_eq!(t, "v0.12.0 已下载并校验, 可以更新");
        let a = a.unwrap();
        assert_eq!(a.label, "更新到 v0.12.0");
        assert_eq!(a.kind, ActionKind::Request { word: "install-desktop".into(), files: vec!["meshtray".into(), "fmesh-desktop".into()] });
        let (t, a) = group_update(&st("module meshtray downloading release 12 progress 3/10\n"), "desktop", &files, |_| false);
        assert_eq!((t.as_str(), a), ("正在下载 v0.12.0: 3/10", None));
        let (t, _) = group_update(&st("module meshtray current release 12\nmodule fmesh-desktop current release 12\n"), "desktop", &files, |m| m.state == "current");
        assert_eq!(t, "已是最新 v0.12.0 (发布 v12)");
        let auto = status("node a 10.249.0.1 id 1 roster v3 release v12 (x) key K=\nupdate version 0.12.0 release 12 state idle auto desktop\nmodule meshtray ready release 12 sha256 ab path /c/ab\n");
        assert_eq!(group_update(&auto, "desktop", &files, |_| false), ("v0.12.0 已下载并校验, 正在自动安装".into(), None));
        // The agent file is compared with the user's own copy.
        let (_, a) = group_update(&st("module agent-inbox ready release 12 sha256 ab path /c/ab\n"), "agent", &["agent-inbox"], |_| false);
        assert_eq!(a.unwrap().kind, ActionKind::InstallAgent { from: "/c/ab".into() });
        let (t, a) = group_update(&st("module agent-inbox ready release 12 sha256 ab path /c/ab\n"), "agent", &["agent-inbox"], |_| true);
        assert_eq!((t.as_str(), a), ("已是最新 v0.12.0 (发布 v12)", None));
        assert!(group_update(&st(""), "cli", &["fmesh"], |_| false).0.contains("还没有这个模块的文件"));
    }

    #[test]
    fn agent_install_keeps_the_old_file() {
        let d = std::env::temp_dir().join(format!("fmesh-agent-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let (from, target) = (d.join("cached"), d.join("agent-inbox"));
        std::fs::write(&from, b"new").unwrap();
        std::fs::write(&target, b"old").unwrap();
        let text = install_agent(&from, &target).unwrap();
        assert!(text.starts_with("已更新 agent-inbox"), "{text}");
        assert_eq!(std::fs::read(&target).unwrap(), b"new");
        assert_eq!(std::fs::read(d.join("agent-inbox.prev")).unwrap(), b"old");
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn update_words() {
        let u = |line: &str| crate::status::parse(line, true).update.unwrap();
        assert_eq!(network_update(&u("update version 0.11.0 release 11 latest 12 via hk age 2 state downloading progress 3/9")), "正在下载 v12: 3/9");
        assert_eq!(network_update(&u("update version 0.11.0 release 11 latest 12 via hk age 2 state idle bad 12")), "v12 启动检查失败, 已退回 v11");
        assert_eq!(network_update(&u("update version 0.11.0 release 11 state idle")), "还没有中继宣告版本 (中继都连不上?)");
        assert!(network_update(&u("update version 0.11.0 release 11 latest 12 via hk age 2 state idle error manifest rejected: x")).ends_with("上次出错: manifest rejected: x"));
    }
}
