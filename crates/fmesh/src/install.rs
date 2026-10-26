//! `fmesh install <agent|network|desktop>`: put one module on this machine.
//!
//! Each module stands alone. `agent` needs no root and no network module: a local-only relay
//! for this user's sessions. `network` installs meshd as a root service and generates the node
//! key here; joining needs the admin to add the printed request to the roster. `desktop` adds
//! the tray and the native list on a machine with a desktop.
//!
//! Every install is a plan that `--dry-run` prints. Existing configuration and service files
//! are kept, never overwritten; binaries are replaced by writing a new file and renaming it
//! (macOS refuses to start a signed binary overwritten in place).

use std::path::{Path, PathBuf};
use std::process::Command;

const MESHD_PLIST: &str = include_str!("../../../deploy/mesh/com.flyto.meshd.plist");
const MESHD_UNIT: &str = include_str!("../../../deploy/mesh/meshd.service");
const TRAY_PLIST: &str = include_str!("../../../deploy/mesh/com.flyto.meshtray.plist");

pub const USAGE: &str = "fmesh install agent [--home DIR] [--host NAME] [--port N] [--no-service] [--upgrade] [--dry-run]
fmesh install network --name NAME [--admin-pub FILE --roster FILE] [--dry-run]   (root)
fmesh install desktop [--upgrade] [--dry-run]                                     (root, macOS)
An existing install is left alone: --upgrade only replaces binaries (agent, desktop); network
nodes update themselves through signed releases.";

#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    Mkdir { path: PathBuf, mode: u32 },
    /// Atomic replace: copy to a temporary name in the same directory, then rename.
    Install { from: PathBuf, to: PathBuf },
    /// Written only when absent.
    Write { path: PathBuf, content: String, mode: u32 },
    /// A node key generated here, only when absent.
    Genkey { meshd: PathBuf, key: PathBuf },
    /// Verify a signed roster before installing it.
    CheckRoster { meshd: PathBuf, admin_pub: PathBuf, roster: PathBuf },
    Run { argv: Vec<String> },
    /// Print the join request for the admin.
    JoinRequest { meshd: PathBuf, key: PathBuf, name: String },
    Note(String),
}

pub struct Opts {
    pub home: PathBuf,
    pub host: String,
    pub port: u16,
    pub service: bool,
    pub dry_run: bool,
    pub upgrade: bool,
    /// System root for /opt, /etc and /Library (tests use a scratch directory).
    pub root: PathBuf,
    pub name: Option<String>,
    pub admin_pub: Option<PathBuf>,
    pub roster: Option<PathBuf>,
    /// Directory holding the module binaries (this package).
    pub from: PathBuf,
}

pub fn run(args: &[String]) -> Result<(), String> {
    let (module, rest) = args.split_first().ok_or_else(|| format!("usage:\n{USAGE}"))?;
    let o = parse(rest)?;
    let steps = match module.as_str() {
        "agent" => agent(&o)?,
        "network" => network(&o)?,
        "desktop" => desktop(&o)?,
        _ => return Err(format!("usage:\n{USAGE}")),
    };
    if o.dry_run {
        println!("plan ({} steps, nothing changed):", steps.len());
        for s in &steps {
            println!("  {}", describe(s));
        }
        return Ok(());
    }
    for s in &steps {
        execute(s)?;
    }
    Ok(())
}

fn parse(args: &[String]) -> Result<Opts, String> {
    let exe_dir = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)).unwrap_or_default();
    let mut o = Opts {
        home: std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default(),
        host: hostname(),
        port: 8831,
        service: true,
        dry_run: false,
        upgrade: false,
        root: PathBuf::from("/"),
        name: None,
        admin_pub: None,
        roster: None,
        from: exe_dir,
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut value = || it.next().cloned().ok_or_else(|| format!("{a} needs a value"));
        match a.as_str() {
            "--home" => o.home = PathBuf::from(value()?),
            "--host" => o.host = value()?,
            "--port" => o.port = value()?.parse().map_err(|_| "--port must be a number".to_string())?,
            "--no-service" => o.service = false,
            "--dry-run" => o.dry_run = true,
            "--upgrade" => o.upgrade = true,
            "--name" => o.name = Some(value()?),
            "--admin-pub" => o.admin_pub = Some(PathBuf::from(value()?)),
            "--roster" => o.roster = Some(PathBuf::from(value()?)),
            "--from" => o.from = PathBuf::from(value()?),
            _ => return Err(format!("unknown option {a}\n{USAGE}")),
        }
    }
    if o.home.as_os_str().is_empty() {
        return Err("HOME is not set; pass --home".into());
    }
    Ok(o)
}

/// A relay host component: ASCII letters, digits, `_` and `-`, at most 63 bytes.
pub fn component(raw: &str) -> String {
    let s: String = raw.split('.').next().unwrap_or("").chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '-' }).collect();
    let s = s.trim_matches('-').chars().take(63).collect::<String>();
    if s.is_empty() { "host".into() } else { s }
}

fn hostname() -> String {
    let out = Command::new("hostname").arg("-s").output().ok().filter(|o| o.status.success());
    component(&out.map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned()).unwrap_or_default())
}

fn bin(dir: &Path, name: &str) -> PathBuf {
    dir.join(if cfg!(windows) { format!("{name}.exe") } else { name.to_owned() })
}

fn need(from: &Path, name: &str) -> Result<PathBuf, String> {
    let p = bin(from, name);
    if p.exists() { Ok(p) } else { Err(format!("{} is missing from this package ({})", name, from.display())) }
}

/// Agent Mesh for this user: binaries in ~/.local/bin, a local-only relay run by
/// `agent-inbox daemon`, and its user service.
pub fn agent(o: &Opts) -> Result<Vec<Step>, String> {
    if cfg!(windows) {
        return Err("Agent Mesh is not supported on Windows yet: its local relay control uses Unix sockets. \
                    Windows desktops can list agents through directory_command in devices.json."
            .into());
    }
    let host = component(&o.host);
    let bin_dir = o.home.join(".local/bin");
    let conf = o.home.join(".config/agent-inbox");
    let agent_inbox = bin_dir.join("agent-inbox");
    let found = existing_agent(o);
    if o.upgrade {
        if found.is_empty() && !agent_inbox.exists() {
            return Err("nothing to upgrade: no Agent Mesh install found".into());
        }
        return Ok(vec![
            Step::Install { from: need(&o.from, "agent-inbox")?, to: agent_inbox },
            Step::Install { from: need(&o.from, "fmesh")?, to: bin_dir.join("fmesh") },
            Step::Note("binaries replaced; restart the relay service, then reconnect Claude sessions (see agent-inbox docs/DESKTOP-SERVICE.md)".into()),
        ]);
    }
    if !found.is_empty() {
        return Err(format!(
            "an Agent Mesh install is already here ({}). Running a second relay next to it is not supported; \
             use --upgrade to replace only the binaries, or migrate by hand (agent-inbox docs/DESKTOP-SERVICE.md)",
            found.join(", ")
        ));
    }
    let mut s = vec![
        Step::Mkdir { path: bin_dir.clone(), mode: 0o755 },
        Step::Mkdir { path: conf.clone(), mode: 0o700 },
        Step::Mkdir { path: conf.join("state"), mode: 0o700 },
        Step::Install { from: need(&o.from, "agent-inbox")?, to: agent_inbox.clone() },
        Step::Install { from: need(&o.from, "fmesh")?, to: bin_dir.join("fmesh") },
        Step::Write {
            path: conf.join("relay.toml"),
            content: format!(
                "# Agent Mesh relay for this user's sessions on this machine only.\n\
                 # Cross-host routing needs the network module and dynamic membership; see the docs.\n\
                 host = '{host}'\nlocal_only = true\njournal = 'state/relay.md'\nstate_dir = 'state'\n"
            ),
            mode: 0o600,
        },
        Step::Write {
            path: conf.join("daemon.toml"),
            content: format!(
                "# One startup entry: the relay (and optional watchers, see the docs).\n\
                 state_dir = 'state/desktop-daemon'\n\n[relay]\nlisten = '127.0.0.1:{}'\nconfig = 'relay.toml'\nno_tunnel = true\n",
                o.port
            ),
            mode: 0o600,
        },
    ];
    let daemon = [agent_inbox.display().to_string(), "daemon".into(), "--config".into(), conf.join("daemon.toml").display().to_string()];
    if o.service {
        if cfg!(target_os = "macos") {
            let plist = o.home.join("Library/LaunchAgents/io.agent-inbox.plist");
            let args: String = daemon.iter().map(|a| format!("<string>{}</string>", xml(a))).collect();
            let log = xml(&conf.join("state/daemon.log").display().to_string());
            s.push(Step::Mkdir { path: o.home.join("Library/LaunchAgents"), mode: 0o755 });
            s.push(Step::Write {
                path: plist.clone(),
                content: format!(
                    "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
                     <plist version=\"1.0\">\n<dict>\n  <key>Label</key><string>io.agent-inbox</string>\n  <key>ProgramArguments</key><array>{args}</array>\n\
                     \x20 <key>RunAtLoad</key><true/>\n  <key>KeepAlive</key><true/>\n  <key>ThrottleInterval</key><integer>15</integer>\n  <key>Umask</key><integer>63</integer>\n\
                     \x20 <key>StandardOutPath</key><string>{log}</string>\n  <key>StandardErrorPath</key><string>{log}</string>\n</dict>\n</plist>\n"
                ),
                mode: 0o644,
            });
            s.push(Step::Run { argv: vec!["launchctl".into(), "bootstrap".into(), format!("gui/{}", uid()), plist.display().to_string()] });
        } else {
            let unit = o.home.join(".config/systemd/user/agent-inbox-desktop.service");
            s.push(Step::Mkdir { path: o.home.join(".config/systemd/user"), mode: 0o755 });
            s.push(Step::Write {
                path: unit,
                content: format!(
                    "[Unit]\nDescription=fMesh Agent Mesh relay for this user's sessions\n\n[Service]\nType=exec\nExecStart={}\nRestart=always\nRestartSec=15\nUMask=0077\n\n[Install]\nWantedBy=default.target\n",
                    daemon.join(" ")
                ),
                mode: 0o644,
            });
            s.push(Step::Run { argv: vec!["systemctl".into(), "--user".into(), "daemon-reload".into()] });
            s.push(Step::Run { argv: vec!["systemctl".into(), "--user".into(), "enable".into(), "--now".into(), "agent-inbox-desktop.service".into()] });
        }
    } else {
        s.push(Step::Note(format!("no service installed; run it in the foreground with: {}", daemon.join(" "))));
    }
    s.push(Step::Note(format!(
        "Claude Code, once per user: claude mcp add --scope user --transport stdio agent-inbox-join -- {} claude --relay 127.0.0.1:{}",
        agent_inbox.display(),
        o.port
    )));
    s.push(Step::Note("Codex and Grok watchers: see crates/agent-inbox/docs/CODEX-AUTO-JOIN.md and GROK-AUTO-JOIN.md".into()));
    s.push(Step::Note(format!("check: {} agents", bin_dir.join("fmesh").display())));
    Ok(s)
}

/// Signs of an Agent Mesh install for this user: service entries of either generation, the
/// daemon config, or something already listening on the relay port.
fn existing_agent(o: &Opts) -> Vec<String> {
    let mut found = vec![];
    for p in [
        "Library/LaunchAgents/io.agent-inbox.plist",
        "Library/LaunchAgents/io.agent-inbox.relay.plist",
        ".config/systemd/user/agent-inbox-desktop.service",
        ".config/systemd/user/agent-inbox-relay.user.service",
        ".config/systemd/user/agent-inbox-relay.service",
        ".config/agent-inbox/daemon.toml",
        ".config/agent-inbox/relay.toml",
    ] {
        if o.home.join(p).exists() {
            found.push(format!("~/{p}"));
        }
    }
    if std::net::TcpListener::bind(("127.0.0.1", o.port)).is_err() {
        found.push(format!("a process listening on 127.0.0.1:{}", o.port));
    }
    found
}

/// Network Mesh node: meshd as a root service, its key generated here.
pub fn network(o: &Opts) -> Result<Vec<Step>, String> {
    if cfg!(windows) {
        return Err("on Windows use deploy/windows/install.ps1 -Module network".into());
    }
    let name = component(o.name.as_deref().ok_or("--name NAME is required (the node name the admin will put in the roster)")?);
    let (opt, etc) = (o.root.join("opt/flytomesh"), o.root.join("etc/flytomesh"));
    // /etc/flytomesh is root-only: a directory we cannot read is treated as a node too.
    let unreadable = etc.exists() && std::fs::read_dir(&etc).is_err();
    if unreadable || etc.join("roster").exists() {
        return Err(format!(
            "this machine is already a mesh node ({} exists). Nodes update themselves through signed releases; \
             replacing meshd by hand would bypass that",
            etc.join("roster").display()
        ));
    }
    let meshd = opt.join("meshd");
    let key = etc.join("key");
    let mut s = vec![
        Step::Mkdir { path: opt.clone(), mode: 0o755 },
        Step::Mkdir { path: etc.clone(), mode: 0o700 },
        Step::Install { from: need(&o.from, "meshd")?, to: meshd.clone() },
        Step::Install { from: need(&o.from, "fmesh")?, to: opt.join("fmesh") },
        Step::Genkey { meshd: meshd.clone(), key: key.clone() },
    ];
    match (&o.admin_pub, &o.roster) {
        (Some(admin), Some(roster)) => {
            s.push(Step::CheckRoster { meshd: meshd.clone(), admin_pub: admin.clone(), roster: roster.clone() });
            s.push(Step::Install { from: admin.clone(), to: etc.join("admin.pub") });
            s.push(Step::Install { from: roster.clone(), to: etc.join("roster") });
            if cfg!(target_os = "macos") {
                let plist = o.root.join("Library/LaunchDaemons/com.flyto.meshd.plist");
                s.push(Step::Write { path: plist.clone(), content: MESHD_PLIST.into(), mode: 0o644 });
                s.push(Step::Run { argv: vec!["launchctl".into(), "bootstrap".into(), "system".into(), plist.display().to_string()] });
            } else {
                s.push(Step::Write { path: o.root.join("etc/systemd/system/meshd.service"), content: MESHD_UNIT.into(), mode: 0o644 });
                s.push(Step::Run { argv: vec!["systemctl".into(), "daemon-reload".into()] });
                s.push(Step::Run { argv: vec!["systemctl".into(), "enable".into(), "--now".into(), "meshd.service".into()] });
            }
            s.push(Step::Note("check: /opt/flytomesh/fmesh status (relays up within ~25 s; clocks must be in sync)".into()));
        }
        (None, None) => {
            s.push(Step::JoinRequest { meshd, key, name });
        }
        _ => return Err("--admin-pub and --roster go together".into()),
    }
    Ok(s)
}

/// Desktop: the tray and the native list, for every user of this Mac.
pub fn desktop(o: &Opts) -> Result<Vec<Step>, String> {
    if cfg!(windows) {
        return Err("on Windows use deploy/windows/install.ps1 -Module desktop".into());
    }
    if !cfg!(target_os = "macos") {
        return Err("no desktop module on this platform; use `fmesh devices`".into());
    }
    let opt = o.root.join("opt/flytomesh");
    let plist = o.root.join("Library/LaunchAgents/com.flyto.meshtray.plist");
    let binaries = vec![
        Step::Mkdir { path: opt.clone(), mode: 0o755 },
        Step::Install { from: need(&o.from, "meshtray")?, to: opt.join("meshtray") },
        Step::Install { from: need(&o.from, "fmesh-desktop")?, to: opt.join("fmesh-desktop") },
    ];
    if plist.exists() {
        if !o.upgrade {
            return Err(format!("the desktop module is already installed ({}); use --upgrade to replace its binaries", plist.display()));
        }
        let mut s = binaries;
        s.push(Step::Note("binaries replaced; quit the tray from its menu and log in again (or launchctl kickstart it)".into()));
        return Ok(s);
    }
    Ok(vec![
        Step::Mkdir { path: opt.clone(), mode: 0o755 },
        Step::Install { from: need(&o.from, "meshtray")?, to: opt.join("meshtray") },
        Step::Install { from: need(&o.from, "fmesh-desktop")?, to: opt.join("fmesh-desktop") },
        Step::Write { path: plist, content: TRAY_PLIST.into(), mode: 0o644 },
        Step::Note("the tray starts at the next login, or now with: launchctl bootstrap gui/$(id -u) /Library/LaunchAgents/com.flyto.meshtray.plist".into()),
    ])
}

fn xml(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn uid() -> u32 {
    Command::new("id").arg("-u").output().ok().and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse().ok()).unwrap_or(0)
}

fn describe(s: &Step) -> String {
    match s {
        Step::Mkdir { path, mode } => format!("mkdir {} ({mode:o})", path.display()),
        Step::Install { from, to } => format!("install {} -> {}", from.display(), to.display()),
        Step::Write { path, mode, .. } => format!("write {} ({mode:o}) unless it exists", path.display()),
        Step::Genkey { key, .. } => format!("generate the node key {} unless it exists", key.display()),
        Step::CheckRoster { roster, .. } => format!("verify {} against the admin key", roster.display()),
        Step::Run { argv } => format!("run {}", argv.join(" ")),
        Step::JoinRequest { name, .. } => format!("print the join request for node {name}"),
        Step::Note(n) => format!("note: {n}"),
    }
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).map_err(|e| format!("chmod {}: {e}", path.display()))
}
#[cfg(not(unix))]
fn set_mode(_: &Path, _: u32) -> Result<(), String> {
    Ok(())
}

/// Gives an installed file the owner of its directory. Copying as root on macOS keeps the
/// source's owner (clonefile), which would leave a user-writable binary in a root-owned
/// directory such as /opt/flytomesh.
#[cfg(unix)]
fn own_like_parent(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;
    let meta = |p: &Path| std::fs::metadata(p).map_err(|e| format!("stat {}: {e}", p.display()));
    let dir = meta(path.parent().unwrap_or(Path::new(".")))?;
    let file = meta(path)?;
    if (file.uid(), file.gid()) != (dir.uid(), dir.gid()) {
        std::os::unix::fs::chown(path, Some(dir.uid()), Some(dir.gid()))
            .map_err(|e| format!("chown {}: {e}", path.display()))?;
    }
    Ok(())
}
#[cfg(not(unix))]
fn own_like_parent(_: &Path) -> Result<(), String> {
    Ok(())
}

fn execute(s: &Step) -> Result<(), String> {
    match s {
        Step::Mkdir { path, mode } => {
            if !path.exists() {
                std::fs::create_dir_all(path).map_err(|e| format!("mkdir {}: {e}", path.display()))?;
                set_mode(path, *mode)?;
            }
        }
        Step::Install { from, to } => {
            let tmp = to.with_extension("fmesh-new");
            std::fs::copy(from, &tmp).map_err(|e| format!("copy {} -> {}: {e}", from.display(), tmp.display()))?;
            if let Err(e) = own_like_parent(&tmp) {
                std::fs::remove_file(&tmp).ok();
                return Err(e);
            }
            std::fs::rename(&tmp, to).map_err(|e| format!("rename {}: {e}", to.display()))?;
            println!("installed {}", to.display());
        }
        Step::Write { path, content, mode } => {
            if path.exists() {
                println!("kept existing {}", path.display());
            } else {
                std::fs::write(path, content).map_err(|e| format!("write {}: {e}", path.display()))?;
                set_mode(path, *mode)?;
                println!("wrote {}", path.display());
            }
        }
        Step::Genkey { meshd, key } => {
            if key.exists() {
                println!("kept existing node key {}", key.display());
            } else {
                let out = Command::new(meshd).arg("genkey").output().map_err(|e| format!("meshd genkey: {e}"))?;
                if !out.status.success() {
                    return Err("meshd genkey failed".into());
                }
                std::fs::write(key, &out.stdout).map_err(|e| format!("write {}: {e}", key.display()))?;
                set_mode(key, 0o600)?;
                println!("generated the node key {} (never leaves this machine)", key.display());
            }
        }
        Step::CheckRoster { meshd, admin_pub, roster } => {
            let admin = std::fs::read_to_string(admin_pub).map_err(|e| format!("read {}: {e}", admin_pub.display()))?;
            let input = std::fs::File::open(roster).map_err(|e| format!("open {}: {e}", roster.display()))?;
            let out = Command::new(meshd).args(["roster-check", "--admin-pub", admin.trim()]).stdin(input).output().map_err(|e| e.to_string())?;
            if !out.status.success() {
                return Err(format!("the roster does not verify: {}", String::from_utf8_lossy(&out.stderr).trim()));
            }
            print!("{}", String::from_utf8_lossy(&out.stdout));
        }
        Step::Run { argv } => {
            let st = Command::new(&argv[0]).args(&argv[1..]).status().map_err(|e| format!("{}: {e}", argv.join(" ")))?;
            if !st.success() {
                return Err(format!("{} failed ({st})", argv.join(" ")));
            }
        }
        Step::JoinRequest { meshd, key, name } => {
            let input = std::fs::File::open(key).map_err(|e| format!("open {}: {e}", key.display()))?;
            let out = Command::new(meshd).arg("pubkey").stdin(input).output().map_err(|e| e.to_string())?;
            let pubkey = String::from_utf8_lossy(&out.stdout).trim().to_owned();
            println!("Join request (send this to the mesh admin; it holds no secret):");
            println!("  name {name}");
            println!("  key  {pubkey}");
            println!("The admin adds `node <id> {name} <mesh IP> {pubkey}` to the roster and signs it.");
            println!("Then run: fmesh install network --name {name} --admin-pub <admin.pub> --roster <roster>");
        }
        Step::Note(n) => println!("{n}"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(dir: &Path) -> Opts {
        Opts {
            home: dir.join("home"),
            host: "Box.local".into(),
            port: std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port(),
            service: false,
            dry_run: true,
            upgrade: false,
            root: dir.join("root"),
            name: None,
            admin_pub: None,
            roster: None,
            from: dir.to_path_buf(),
        }
    }

    fn package() -> PathBuf {
        let d = std::env::temp_dir().join(format!("fmesh-install-test-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        for b in ["agent-inbox", "fmesh", "meshd", "meshtray", "fmesh-desktop"] {
            std::fs::write(d.join(b), b"bin").unwrap();
        }
        d
    }

    #[test]
    fn host_names_become_relay_components() {
        assert_eq!(component("Box.local"), "Box");
        assert_eq!(component("my host!"), "my-host");
        assert_eq!(component(""), "host");
        assert_eq!(component(&"x".repeat(80)).len(), 63);
    }

    #[cfg(unix)]
    #[test]
    fn agent_only_is_user_level_local_only_and_needs_no_network() {
        let d = package();
        let steps = agent(&opts(&d)).unwrap();
        let text: Vec<String> = steps.iter().map(describe).collect();
        assert!(steps.iter().all(|s| !matches!(s, Step::Install { to, .. } | Step::Write { path: to, .. } if !to.starts_with(d.join("home")))), "{text:?}");
        assert!(!text.iter().any(|t| t.contains("meshd") || t.contains("/opt/") || t.contains("/etc/")), "{text:?}");
        let relay = steps.iter().find_map(|s| match s { Step::Write { path, content, .. } if path.ends_with("relay.toml") => Some(content.clone()), _ => None }).unwrap();
        assert!(relay.contains("host = 'Box'") && relay.contains("local_only = true"));
        let o = opts(&d);
        let daemon = agent(&o).unwrap().into_iter().find_map(|s| match s { Step::Write { path, content, .. } if path.ends_with("daemon.toml") => Some(content), _ => None }).unwrap();
        assert!(daemon.contains(&format!("127.0.0.1:{}", o.port)) && daemon.contains("no_tunnel = true"));
    }

    #[cfg(unix)]
    #[test]
    fn network_join_generates_the_key_here_and_needs_both_roster_files() {
        let d = package();
        let mut o = opts(&d);
        assert!(network(&o).is_err(), "a name is required");
        o.name = Some("laptop9".into());
        let steps = network(&o).unwrap();
        assert!(steps.iter().any(|s| matches!(s, Step::Genkey { .. })));
        assert!(steps.iter().any(|s| matches!(s, Step::JoinRequest { name, .. } if name == "laptop9")));
        assert!(!steps.iter().any(|s| matches!(s, Step::Run { .. })), "no service before the roster arrives");
        o.roster = Some(d.join("roster"));
        assert!(network(&o).is_err());
        o.admin_pub = Some(d.join("admin.pub"));
        let steps = network(&o).unwrap();
        let check = steps.iter().position(|s| matches!(s, Step::CheckRoster { .. })).unwrap();
        let start = steps.iter().position(|s| matches!(s, Step::Run { .. })).unwrap();
        assert!(check < start, "verify before starting");
    }

    #[cfg(unix)]
    #[test]
    fn an_existing_install_is_refused_unless_upgrading_binaries() {
        let d = package().join("existing");
        std::fs::create_dir_all(&d).unwrap();
        for b in ["agent-inbox", "fmesh", "meshd", "meshtray", "fmesh-desktop"] {
            std::fs::write(d.join(b), b"bin").unwrap();
        }
        let mut o = opts(&d);
        // The older relay entry counts as an install.
        let agents = o.home.join("Library/LaunchAgents");
        std::fs::create_dir_all(&agents).unwrap();
        std::fs::write(agents.join("io.agent-inbox.relay.plist"), "x").unwrap();
        let err = agent(&o).unwrap_err();
        assert!(err.contains("io.agent-inbox.relay.plist"), "{err}");
        o.upgrade = true;
        let steps = agent(&o).unwrap();
        assert!(steps.iter().all(|s| matches!(s, Step::Install { .. } | Step::Note(_))), "upgrade writes no config, starts nothing");
        // Something on the relay port counts too.
        let busy = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let mut o = opts(&d.join("other"));
        o.from = d.clone();
        o.port = busy.local_addr().unwrap().port();
        assert!(agent(&o).unwrap_err().contains("listening"));
        // A node with a roster is left to signed releases.
        let mut o = opts(&d);
        o.name = Some("n".into());
        std::fs::create_dir_all(o.root.join("etc/flytomesh")).unwrap();
        std::fs::write(o.root.join("etc/flytomesh/roster"), "x").unwrap();
        assert!(network(&o).unwrap_err().contains("signed releases"));
    }

    #[cfg(unix)]
    #[test]
    fn writes_keep_existing_files_and_installs_replace_atomically() {
        let d = package();
        let target = d.join("existing.toml");
        std::fs::write(&target, "mine").unwrap();
        execute(&Step::Write { path: target.clone(), content: "theirs".into(), mode: 0o600 }).unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "mine");
        let to = d.join("installed");
        std::fs::write(&to, "old").unwrap();
        execute(&Step::Install { from: d.join("meshd"), to: to.clone() }).unwrap();
        assert_eq!(std::fs::read(&to).unwrap(), b"bin");
        assert!(!to.with_extension("fmesh-new").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let (f, dir) = (std::fs::metadata(&to).unwrap(), std::fs::metadata(&d).unwrap());
            assert_eq!((f.uid(), f.gid()), (dir.uid(), dir.gid()));
        }
    }
}
