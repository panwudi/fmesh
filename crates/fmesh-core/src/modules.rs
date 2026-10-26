//! The modules on this machine: which are installed, their versions, whether they run and how
//! they are updated. Same model for `fmesh modules` and the desktop window.
//!
//! Versions come from what each program reports without running GUI programs: meshd from its
//! status file, agent-inbox and fmesh from `--version`, the tray and the list window from the
//! version mark compiled into them (`VERSION_MARK`). Builds before 0.11 carry no mark and
//! report no update state; they show as "unknown", never as a guessed version.

use crate::status::{Status, Update};
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
    pub policy: &'static str,
    /// Update state in words.
    pub update: String,
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
    let mut list = vec![network(s, probe), agent(me, probe), desktop(probe), cli(probe)];
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
        policy: "自动更新 (管理员统一发版)",
        update,
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

fn agent(me: Option<&Device>, probe: &dyn Probe) -> Module {
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
    Module {
        id: "agent",
        name: "Agent",
        version_text: String::new(),
        installed,
        path: path.map(|p| p.display().to_string()),
        version,
        state,
        ok,
        policy: "手动更新",
        update: "在发布包里运行 fmesh install agent --upgrade; 一键更新在下一版".into(),
        install_hint: (!installed).then(|| if cfg!(windows) { "Windows 暂不支持 Agent 模块".into() } else { "fmesh install agent (不需要 root)".into() }),
    }
}

fn desktop(probe: &dyn Probe) -> Module {
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
    Module {
        id: "desktop",
        name: "桌面",
        version_text: String::new(),
        installed,
        path: tray.or(list).map(|p| p.display().to_string()),
        version,
        ok: installed,
        state,
        policy: "手动更新",
        update: if installed { "在发布包里运行 sudo fmesh install desktop --upgrade; 一键更新在下一版".into() } else { String::new() },
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

fn cli(probe: &dyn Probe) -> Module {
    let path = probe.find("fmesh");
    let version = path.as_deref().and_then(|p| probe.version("fmesh", p));
    Module {
        id: "cli",
        name: "命令行",
        version_text: String::new(),
        installed: path.is_some(),
        version,
        state: if path.is_some() { "已安装".into() } else { "未安装".into() },
        ok: path.is_some(),
        path: path.map(|p| p.display().to_string()),
        policy: "随组网或桌面模块安装",
        update: String::new(),
        install_hint: None,
    }
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
    let nonce = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let req = PathBuf::from(SERVICES_DIR).join(format!(".request-{}-{nonce}", std::process::id()));
    if let Err(e) = std::fs::write(&req, "check\n") {
        return Check::Failed(format!("写不进请求目录 {SERVICES_DIR}: {e}"));
    }
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
    fn update_words() {
        let u = |line: &str| crate::status::parse(line, true).update.unwrap();
        assert_eq!(network_update(&u("update version 0.11.0 release 11 latest 12 via hk age 2 state downloading progress 3/9")), "正在下载 v12: 3/9");
        assert_eq!(network_update(&u("update version 0.11.0 release 11 latest 12 via hk age 2 state idle bad 12")), "v12 启动检查失败, 已退回 v11");
        assert_eq!(network_update(&u("update version 0.11.0 release 11 state idle")), "还没有中继宣告版本 (中继都连不上?)");
        assert!(network_update(&u("update version 0.11.0 release 11 latest 12 via hk age 2 state idle error manifest rejected: x")).ends_with("上次出错: manifest rejected: x"));
    }
}
