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
pub mod status;
pub mod view;
