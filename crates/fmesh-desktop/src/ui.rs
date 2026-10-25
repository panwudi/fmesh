//! Glue between the Slint window (ui/app.slint) and the model: builds the card list, keeps
//! search, filter and expansion, copies to the clipboard and reports every result.

use crate::model::{self, Row, FILTERS};
use fmesh_core::view::{Agents, Device, Link, View};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};
use std::cell::RefCell;
use std::collections::BTreeSet;
use std::time::Duration;

slint::include_modules!();

#[derive(Default)]
struct State {
    view: Option<View>,
    query: String,
    filter: usize,
    /// Devices the user opened, by name, kept across searches and refreshes.
    opened: BTreeSet<String>,
    /// Bumped per message so an old timer does not clear a newer one.
    hint_seq: u64,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
}

fn item(i: usize, d: &Device, expanded: bool) -> DeviceItem {
    let agents: Vec<AgentItem> = d
        .agent_list()
        .iter()
        .map(|a| AgentItem {
            short: a.name.split_once('.').map_or(a.name.as_str(), |(_, s)| s).into(),
            role: if a.role.is_empty() { "职责未填写".into() } else { a.role.as_str().into() },
            receiving: a.receiving,
            name: a.name.as_str().into(),
        })
        .collect();
    let mut detail = Vec::new();
    if !d.ip.is_empty() {
        detail.push(d.ip.clone());
    }
    if !d.me {
        detail.push(d.path.clone());
        detail.push(format!("握手 {}", d.handshake_text()));
    } else if d.link == Link::NetworkStopped {
        detail.push("组网未运行".into());
    }
    DeviceItem {
        index: i as i32,
        title: d.mesh_name.as_str().into(),
        me: d.me,
        shared: d.shared,
        online: d.online(),
        stopped: d.link == Link::NetworkStopped,
        detail: detail.join("  ·  ").into(),
        owner: d.owner_text().into(),
        service: match &d.agents {
            Agents::Listed { .. } => format!("Agent · {}", d.agents_text()),
            _ => d.agents_text(),
        }
        .into(),
        has_agents: !agents.is_empty(),
        expanded: expanded && !agents.is_empty(),
        agents: ModelRc::new(VecModel::from(agents)),
    }
}

fn render(app: &AppWindow) {
    STATE.with(|s| {
        let s = s.borrow();
        let Some(v) = s.view.as_ref() else { return };
        let shown = model::visible(v, &s.query, FILTERS[s.filter].1);
        let hits: BTreeSet<usize> = model::expand(v, &s.query).into_iter().collect();
        let items: Vec<DeviceItem> =
            shown.iter().map(|&i| item(i, &v.devices[i], hits.contains(&i) || s.opened.contains(&v.devices[i].name))).collect();
        app.set_devices(ModelRc::new(VecModel::from(items)));
        app.set_count(format!("显示 {} / {} 台", shown.len(), v.devices.len()).into());
        app.set_summary(model::summary(v).into());
        app.set_stats(ModelRc::new(VecModel::from(stats(v))));
        app.set_network_ok(v.summary.network && v.summary.relays_up > 0);
    });
}

fn stats(v: &View) -> Vec<Stat> {
    let s = &v.summary;
    let stat = |label: &str, value: String, ok: bool| Stat { label: label.into(), value: value.into(), ok };
    if !s.network {
        return vec![stat("组网", "未运行".into(), false), stat("本机", v.devices[0].agents_text(), true)];
    }
    let mut out = vec![
        stat("本机", format!("{} {}", s.me, s.ip), true),
        stat("中继", format!("{}/{}", s.relays_up, s.relays), s.relays_up > 0),
        stat("在线", format!("{}/{}", s.others_online, s.others), true),
        stat("Agent 服务", format!("{} 台", s.agent_devices), s.agent_devices > 0),
        stat("版本", format!("{} · 名单 {}", s.release, s.roster), true),
    ];
    if v.labels_error {
        out.push(stat("归属配置", "读取失败".into(), false));
    }
    out
}

/// Shows `text` in the status line; it falls back to the count after a while.
fn say(app: &AppWindow, text: impl Into<SharedString>) {
    app.set_hint(text.into());
    let seq = STATE.with(|s| {
        let mut s = s.borrow_mut();
        s.hint_seq += 1;
        s.hint_seq
    });
    let weak = app.as_weak();
    slint::Timer::single_shot(Duration::from_secs(5), move || {
        if STATE.with(|s| s.borrow().hint_seq) == seq {
            if let Some(app) = weak.upgrade() {
                app.set_hint("".into());
            }
        }
    });
}

fn copy_row(app: &AppWindow, row: Row) {
    let text = STATE.with(|s| s.borrow().view.as_ref().map(|v| (model::copy_text(v, row), model::copied(v, row))));
    let Some((text, done)) = text else { return };
    if copy(&text) {
        say(app, done);
    } else {
        say(app, "复制失败, 请重试");
    }
}

/// The system clipboard through the platform's own tool (no extra dependency).
fn copy(text: &str) -> bool {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let (cmd, args): (&str, &[&str]) = if cfg!(windows) { ("clip", &[]) } else { ("pbcopy", &[]) };
    let Ok(mut child) = Command::new(cmd).args(args).stdin(Stdio::piped()).spawn() else { return false };
    let wrote = child.stdin.take().is_some_and(|mut i| i.write_all(text.as_bytes()).is_ok());
    wrote && child.wait().is_ok_and(|s| s.success())
}

fn reload(app: &AppWindow) {
    if app.get_loading() {
        return;
    }
    app.set_loading(true);
    say(app, "正在查询设备与 Agent 目录...");
    let weak = app.as_weak();
    std::thread::spawn(move || {
        let view = crate::load();
        let _ = slint::invoke_from_event_loop(move || {
            let Some(app) = weak.upgrade() else { return };
            let total = view.devices.len();
            STATE.with(|s| s.borrow_mut().view = Some(view));
            app.set_loading(false);
            render(&app);
            say(&app, format!("已更新 · {total} 台设备"));
        });
    });
}

pub fn run() -> Result<(), slint::PlatformError> {
    let app = AppWindow::new()?;
    app.set_filters(ModelRc::new(VecModel::from(FILTERS.iter().map(|(t, _)| SharedString::from(*t)).collect::<Vec<_>>())));
    app.set_note(model::NOTE.into());
    // The ⌘ glyph is missing from Windows fonts.
    let key = if cfg!(target_os = "macos") { "⌘F" } else { "Ctrl+F" };
    app.set_search_hint(format!("搜索设备, 地址, 归属, Agent 或职责  ({key})").into());

    let weak = app.as_weak();
    app.on_search(move |q| {
        STATE.with(|s| s.borrow_mut().query = q.to_string());
        if let Some(app) = weak.upgrade() {
            render(&app);
        }
    });
    let weak = app.as_weak();
    app.on_set_filter(move |i| {
        STATE.with(|s| s.borrow_mut().filter = (i.max(0) as usize).min(FILTERS.len() - 1));
        if let Some(app) = weak.upgrade() {
            render(&app);
            say(&app, format!("筛选: {}", FILTERS[i.max(0) as usize % FILTERS.len()].0));
        }
    });
    let weak = app.as_weak();
    app.on_refresh(move || {
        if let Some(app) = weak.upgrade() {
            reload(&app);
        }
    });
    let weak = app.as_weak();
    app.on_toggle(move |i| {
        STATE.with(|s| {
            let mut s = s.borrow_mut();
            let name = s.view.as_ref().and_then(|v| v.devices.get(i as usize)).map(|d| d.name.clone());
            if let Some(name) = name {
                if !s.opened.remove(&name) {
                    s.opened.insert(name);
                }
            }
        });
        if let Some(app) = weak.upgrade() {
            render(&app);
        }
    });
    let weak = app.as_weak();
    app.on_copy_device(move |i| {
        if let Some(app) = weak.upgrade() {
            copy_row(&app, Row { device: i as usize, agent: None });
        }
    });
    let weak = app.as_weak();
    app.on_copy_agent(move |i, j| {
        if let Some(app) = weak.upgrade() {
            copy_row(&app, Row { device: i as usize, agent: Some(j as usize) });
        }
    });

    reload(&app);
    app.run()
}
