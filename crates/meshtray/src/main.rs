//! meshtray: the fMesh tray icon, for machines with a desktop (macOS menu bar, Windows
//! notification area). A separate, unprivileged process: it only reads the status file meshd
//! writes every 2 seconds, so the icon crashing or being quit never affects the network.
//! Headless machines do not run it; `fmesh status` and `fmesh devices` show the same.
//!
//! The menu stays short however big the mesh gets: this machine and its connection, how many
//! devices are online, how many announce the agent service, then a few actions. The full
//! device and agent list is the native `fmesh-desktop` window, started on demand and gone when
//! closed. Every click shows its result in the menu for a few seconds.

#![cfg_attr(windows, windows_subsystem = "windows")]

use fmesh_core::directory::Directory;
use fmesh_core::status::{self, Status};
use fmesh_core::{labels, modules, view};
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::{Duration, Instant};
use tao::event::{Event, StartCause};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

#[cfg(target_os = "macos")]
const LOG_FILE: &str = "/var/log/meshd.log";
#[cfg(windows)]
const LOG_FILE: &str = r"C:\ProgramData\flytoMesh\meshd.log";

const REFRESH: Duration = Duration::from_secs(2);
/// How long a click's confirmation stays in the menu.
const FEEDBACK: Duration = Duration::from_secs(3);

fn read_status() -> (Status, String) {
    status::read(std::path::Path::new(status::STATUS_FILE))
}

/// The native list, installed next to the tray (or the `fmesh` command).
fn desktop_exe() -> PathBuf {
    let name = if cfg!(windows) { "fmesh-desktop.exe" } else { "fmesh-desktop" };
    std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join(name))).unwrap_or_else(|| PathBuf::from(name))
}

/// An 18x18 glyph (three linked nodes), drawn in code: no image files to ship. On macOS it
/// is a template image, so the system colours it for light / dark menu bars.
fn icon(ok: bool) -> Icon {
    const N: usize = 18;
    let alpha = if ok { 255.0 } else { 90.0 };
    let nodes = [(9.0f32, 3.5f32), (3.5, 14.0), (14.5, 14.0)];
    let mut px = vec![0u8; N * N * 4];
    let seg_dist = |x: f32, y: f32, (ax, ay): (f32, f32), (bx, by): (f32, f32)| {
        let (dx, dy) = (bx - ax, by - ay);
        let t = (((x - ax) * dx + (y - ay) * dy) / (dx * dx + dy * dy)).clamp(0.0, 1.0);
        ((x - ax - t * dx).powi(2) + (y - ay - t * dy).powi(2)).sqrt()
    };
    for yy in 0..N {
        for xx in 0..N {
            let (x, y) = (xx as f32 + 0.5, yy as f32 + 0.5);
            let mut cover: f32 = 0.0;
            for &(cx, cy) in &nodes {
                let d = ((x - cx).powi(2) + (y - cy).powi(2)).sqrt();
                cover = cover.max((3.0 - d).clamp(0.0, 1.0));
            }
            for (a, b) in [(0, 1), (1, 2), (2, 0)] {
                cover = cover.max((1.4 - seg_dist(x, y, nodes[a], nodes[b])).clamp(0.0, 1.0));
            }
            let i = (yy * N + xx) * 4;
            px[i + 3] = (cover * alpha) as u8;
        }
    }
    Icon::from_rgba(px, N as u32, N as u32).expect("icon")
}


fn copy(text: &str) -> bool {
    use std::io::Write;
    #[cfg(target_os = "macos")]
    let cmd = "pbcopy";
    #[cfg(windows)]
    let cmd = "clip";
    let Ok(mut child) = std::process::Command::new(cmd).stdin(std::process::Stdio::piped()).spawn() else { return false };
    if let Some(mut stdin) = child.stdin.take() {
        if stdin.write_all(text.as_bytes()).is_err() {
            return false;
        }
    }
    child.wait().is_ok_and(|s| s.success())
}

fn open_log() -> bool {
    #[cfg(target_os = "macos")]
    let r = std::process::Command::new("open").args(["-a", "Console", LOG_FILE]).status();
    #[cfg(windows)]
    let r = std::process::Command::new("notepad").arg(LOG_FILE).spawn().map(|_| ());
    r.is_ok()
}

/// The menu's fixed lines, as shown for a status (the same summary as `fmesh status`; the
/// tray never queries the agent directory itself).
fn texts(s: &Status) -> [String; 3] {
    let none = Directory::Unavailable { reason: String::new() };
    view::build(s, &labels::Labels::default(), &none).summary.lines()
}

struct Ui {
    tray: TrayIcon,
    lines: [MenuItem; 3],
    version: MenuItem,
    devices: MenuItem,
    check: MenuItem,
    copy_ip: MenuItem,
    copy_status: MenuItem,
    log: MenuItem,
    quit: MenuItem,
    shown: Status,
    raw: String,
    shown_ok: Option<bool>,
    /// A click's confirmation: (menu item id, text to show, until).
    feedback: Option<(String, String, Instant)>,
    /// The open list window, if this tray started it.
    window: Option<Child>,
}

impl Ui {
    fn new() -> Ui {
        let (s, raw) = read_status();
        let menu = Menu::new();
        let lines = [MenuItem::new("", false, None), MenuItem::new("", false, None), MenuItem::new("", false, None)];
        let version = MenuItem::new("", false, None);
        let devices = MenuItem::new("", true, None);
        let check = MenuItem::new("", true, None);
        let copy_ip = MenuItem::new("", true, None);
        let copy_status = MenuItem::new("", true, None);
        let log = MenuItem::new("", true, None);
        let quit = MenuItem::new("退出图标 (网络不受影响)", true, None);
        for l in &lines {
            menu.append(l).ok();
        }
        menu.append(&version).ok();
        menu.append(&PredefinedMenuItem::separator()).ok();
        for item in [&devices, &check, &copy_ip, &copy_status, &log] {
            menu.append(item).ok();
        }
        menu.append(&PredefinedMenuItem::separator()).ok();
        menu.append(&quit).ok();
        let builder = TrayIconBuilder::new().with_menu(Box::new(menu)).with_tooltip("fMesh");
        // macOS draws it as a template (system colours for light / dark menu bars).
        #[cfg(target_os = "macos")]
        let builder = builder.with_icon_templated(icon(s.running));
        #[cfg(not(target_os = "macos"))]
        let builder = builder.with_icon(icon(s.running));
        let tray = builder.build().expect("tray icon");
        let mut ui = Ui {
            tray,
            lines,
            version,
            devices,
            check,
            copy_ip,
            copy_status,
            log,
            quit,
            shown: Status::default(),
            raw: String::new(),
            shown_ok: None,
            feedback: None,
            window: None,
        };
        ui.refresh(s, raw);
        ui
    }

    fn refresh(&mut self, s: Status, raw: String) {
        if self.feedback.as_ref().is_some_and(|(_, _, until)| Instant::now() >= *until) {
            self.feedback = None;
            self.shown = Status::default(); // force the texts back
        }
        let ok = s.running && s.relays_up() > 0;
        if self.shown_ok != Some(ok) {
            #[cfg(target_os = "macos")]
            self.tray.set_icon_templated(Some(icon(ok))).ok();
            #[cfg(not(target_os = "macos"))]
            self.tray.set_icon(Some(icon(ok))).ok();
            self.tray.set_tooltip(Some(if ok { "fMesh 已连接" } else { "fMesh 未连接" })).ok();
            self.shown_ok = Some(ok);
        }
        self.raw = raw;
        if s == self.shown && self.feedback.is_none() {
            return;
        }
        for (item, text) in self.lines.iter().zip(texts(&s)) {
            item.set_text(text);
        }
        if let Some(owner) = labels::load().owners.get(&s.me) {
            self.lines[0].set_text(format!("{} · {}", texts(&s)[0], owner));
        }
        self.version.set_text(modules::update_short(&s));
        self.devices.set_text(format!("查看设备与 Agent ({} 台)...", s.peers.len() + 1));
        self.check.set_text("本机模块与检查更新...");
        self.copy_ip.set_text(if s.ip.is_empty() { "复制本机地址".to_string() } else { format!("复制本机地址 {}", s.ip) });
        self.copy_status.set_text("复制诊断信息");
        self.log.set_text("查看日志");
        if let Some((id, text, _)) = &self.feedback {
            for item in [&self.devices, &self.check, &self.copy_ip, &self.copy_status, &self.log] {
                if item.id().0 == *id {
                    item.set_text(text);
                }
            }
        }
        self.shown = s;
    }

    /// Handles a click; the result is shown in the clicked item for `FEEDBACK`.
    fn click(&mut self, id: &str) -> bool {
        let (s, raw) = read_status();
        let result = if id == self.quit.id().0 {
            return false;
        } else if id == self.devices.id().0 {
            self.open_window(&[])
        } else if id == self.check.id().0 {
            self.open_window(&["--check"])
        } else if id == self.copy_ip.id().0 {
            if s.ip.is_empty() {
                "meshd 未运行, 没有地址".into()
            } else if copy(&s.ip) {
                format!("✓ 已复制 {}", s.ip)
            } else {
                "复制失败".into()
            }
        } else if id == self.copy_status.id().0 {
            if raw.is_empty() {
                "没有状态可复制 (meshd 未运行)".into()
            } else if copy(&raw) {
                "✓ 已复制诊断信息".into()
            } else {
                "复制失败".into()
            }
        } else if id == self.log.id().0 {
            if open_log() { "✓ 已打开日志".into() } else { format!("打不开日志 {LOG_FILE}") }
        } else {
            return true;
        };
        self.feedback = Some((id.to_string(), result, Instant::now() + FEEDBACK));
        self.shown = Status::default();
        self.refresh(s, raw);
        true
    }
}

impl Ui {
    /// Starts the native list unless the one this tray started is still open. `--check`
    /// opens it on the modules page and checks for updates there.
    fn open_window(&mut self, args: &[&str]) -> String {
        if let Some(child) = self.window.as_mut() {
            if matches!(child.try_wait(), Ok(None)) {
                return "窗口已经打开, 请在窗口里操作".into();
            }
        }
        let exe = desktop_exe();
        match Command::new(&exe).args(args).spawn() {
            Ok(child) => {
                self.window = Some(child);
                if args.is_empty() { "✓ 已打开设备与 Agent 窗口".into() } else { "✓ 已打开本机模块, 正在检查更新".into() }
            }
            Err(_) => format!("找不到设备列表程序 {}", exe.display()),
        }
    }
}

/// Identity of the file at the executable's path, to notice an upgrade. Installers copy the
/// new binary (keeping its modification time) and rename it into place, so the inode (Unix)
/// or creation time (Windows) is what changes.
fn exe_stamp() -> Option<(u64, u64, Option<std::time::SystemTime>)> {
    let m = std::fs::metadata(std::env::current_exe().ok()?).ok()?;
    #[cfg(unix)]
    let ino = std::os::unix::fs::MetadataExt::ino(&m);
    #[cfg(not(unix))]
    let ino = 0;
    Some((m.len(), ino, m.created().ok()))
}

/// The executable was replaced (an upgrade): start the new one. Under launchd a non-zero exit
/// makes it start the job again (KeepAlive on unsuccessful exit); elsewhere the new process is
/// started directly and outlives this one.
fn restart_into_new_binary() -> ! {
    eprintln!("meshtray: executable replaced, restarting into the new version");
    #[cfg(target_os = "macos")]
    if std::env::var_os("XPC_SERVICE_NAME").is_some_and(|n| n == "com.flyto.meshtray") {
        std::process::exit(75);
    }
    if let Ok(exe) = std::env::current_exe() {
        Command::new(exe).spawn().ok();
    }
    std::process::exit(0);
}

fn main() {
    fmesh_core::keep_version_mark();
    if std::env::args().nth(1).as_deref() == Some("--version") {
        println!("meshtray {}", fmesh_core::VERSION);
        return;
    }
    let started_as = exe_stamp();
    // `meshtray --devices`: just open the list window (the same as the menu item).
    if std::env::args().nth(1).as_deref() == Some("--devices") {
        let exe = desktop_exe();
        if let Err(e) = Command::new(&exe).spawn() {
            eprintln!("meshtray: cannot start {}: {e}", exe.display());
            std::process::exit(1);
        }
        return;
    }
    let mut builder = EventLoopBuilder::new();
    let event_loop = {
        #[cfg(target_os = "macos")]
        {
            use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};
            let mut el = builder.build();
            el.set_activation_policy(ActivationPolicy::Accessory); // no Dock icon
            el
        }
        #[cfg(not(target_os = "macos"))]
        {
            builder.build()
        }
    };
    let _ = &mut builder;
    let mut ui: Option<Ui> = None;
    let menu_events = MenuEvent::receiver();
    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(200));
        if let Event::NewEvents(StartCause::Init) = event {
            ui = Some(Ui::new());
        }
        let Some(u) = ui.as_mut() else { return };
        while let Ok(e) = menu_events.try_recv() {
            if !u.click(&e.id.0) {
                *control_flow = ControlFlow::Exit;
                return;
            }
        }
        if let Event::NewEvents(StartCause::ResumeTimeReached { .. }) = event {
            thread_local!(static LAST: std::cell::Cell<Option<Instant>> = const { std::cell::Cell::new(None) });
            let due = LAST.with(|l| l.get().is_none_or(|t| t.elapsed() >= REFRESH));
            if due || u.feedback.is_some() {
                LAST.with(|l| l.set(Some(Instant::now())));
                if started_as.is_some() && exe_stamp().is_some_and(|now| Some(now) != started_as) {
                    restart_into_new_binary();
                }
                let (s, raw) = read_status();
                u.refresh(s, raw);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::texts;
    use fmesh_core::status::parse;

    #[test]
    fn menu_is_a_short_summary() {
        let s = parse(
            "node laptop1 10.249.0.1 id 1 roster v14 release v8 (macos-aarch64) key K=\n\
             relay sh 203.0.113.10:41900 up rtt 42ms sees-us -\n\
             relay hk 203.0.113.20:41900 down rtt - sees-us -\n\
             peer laptop2 10.249.0.2 direct 10.0.0.1:41820 handshake 5s tx 0 rx 0 id 2 key J=\n\
             peer desk 10.249.0.10 relay hk handshake never tx 0 rx 0 id 10 key D=\n\
             service laptop1 10.249.0.1 id 1 name agent-inbox port 8832 ident - expires 60s uid 501\n\
             service laptop2 10.249.0.2 id 2 name agent-inbox port 8832 ident - expires 100s\n",
            true,
        );
        assert_eq!(
            texts(&s),
            ["laptop1 10.249.0.1 · 已连接 (中继 1/2)".to_string(), "其他设备在线: 1/2 台".into(), "Agent: 2 台可用".into()]
        );
        let none = parse("node a 10.249.0.3 id 3 roster v1 release v8 (x) key K=\nrelay sh 203.0.113.10:41900 up rtt 1ms sees-us -\n", true);
        assert_eq!(texts(&none)[2], "Agent: 未发现可用服务");
        assert_eq!(texts(&parse("", false))[0], "组网未运行");
    }
}
