//! fmesh-desktop: the full device and agent list in one Slint window (Cupertino look on
//! macOS, Fluent on Windows). Started on demand by the tray or `fmesh desktop`; closing the
//! window ends the process, so nothing stays resident. Data comes from fmesh-core, the same
//! model as `fmesh devices`, loaded off the UI thread; every action answers in the window.

#![cfg_attr(windows, windows_subsystem = "windows")]

mod model;
mod ui;

use fmesh_core::modules::{self, Module};
use fmesh_core::view::View;
use fmesh_core::{directory, labels, status, view};

/// Reads every source; slow parts (the directory query, reading binaries for their version)
/// run here, never on the UI thread.
pub fn load() -> (View, Vec<Module>) {
    let (s, _) = status::read(std::path::Path::new(status::STATUS_FILE));
    let l = labels::load();
    let d = directory::query(l.directory_command.as_deref());
    let v = view::build(&s, &l, &d);
    let m = modules::local(&s, v.devices.first().filter(|d| d.me), &modules::Disk);
    (v, m)
}

/// How the window was asked to open: `--modules` starts on the modules page, `--check` also
/// checks for updates right away (the tray's "检查更新").
pub struct Start {
    pub modules: bool,
    pub check: bool,
}

fn main() {
    fmesh_core::keep_version_mark();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--version") {
        println!("fmesh-desktop {}", fmesh_core::VERSION);
        return;
    }
    let check = args.iter().any(|a| a == "--check");
    let start = Start { modules: check || args.iter().any(|a| a == "--modules"), check };
    if let Err(e) = ui::run(start) {
        eprintln!("fmesh-desktop: {e}");
        std::process::exit(1);
    }
}
