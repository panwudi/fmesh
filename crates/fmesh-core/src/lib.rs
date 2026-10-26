//! The read-only model behind `fmesh status`, `fmesh devices` and the desktop lists.
//!
//! Three sources, each allowed to be missing: meshd's status file (devices, paths, service
//! announcements), the user's explicit device labels (`devices.json`) and the agent
//! directory (`agent-inbox who --local --json`). `view::build` combines them into one list
//! grouped by computer. Every UI renders that list; none of them reads the sources itself.
//!
//! Nothing here is authorization. Owner labels and shared-host marks are display data, a
//! service announcement is a discovery candidate, and "receiving" says nothing about whether
//! a person is present or a model has acted.

pub mod directory;
pub mod labels;
pub mod modules;
pub mod status;
pub mod view;

/// The fMesh product version, shared by every binary in the workspace.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Found in a binary's bytes by `modules::find_mark`, so the version of a GUI program can be
/// read without running it. Every binary that links fmesh-core calls `keep_version_mark`.
pub const VERSION_MARK: &str = concat!("fmesh-version ", env!("CARGO_PKG_VERSION"), ";");

/// Keeps `VERSION_MARK` in the binary (the linker would drop an unused constant).
pub fn keep_version_mark() {
    std::hint::black_box(VERSION_MARK);
}
