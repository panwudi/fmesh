//! fmesh-desktop: the full device and agent list in a native window (AppKit on macOS, Win32
//! on Windows). Started on demand by the tray or `fmesh desktop`; closing the window ends the
//! process, so nothing stays resident. Data comes from fmesh-core, the same model as
//! `fmesh devices`, loaded off the UI thread; every action shows its result in the window.

#![cfg_attr(windows, windows_subsystem = "windows")]

mod model;

#[cfg(target_os = "macos")]
mod mac;
#[cfg(windows)]
mod win;

use fmesh_core::view::View;
use fmesh_core::{directory, labels, status, view};

/// Reads every source; slow parts (the directory query) run here, never on the UI thread.
pub fn load() -> View {
    let (s, _) = status::read(std::path::Path::new(status::STATUS_FILE));
    let l = labels::load();
    let d = directory::query(l.directory_command.as_deref());
    view::build(&s, &l, &d)
}

fn main() {
    #[cfg(target_os = "macos")]
    mac::run();
    #[cfg(windows)]
    win::run();
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        eprintln!("fmesh-desktop: no native list on this platform; use `fmesh devices`");
        std::process::exit(2);
    }
}
