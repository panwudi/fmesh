//! Virtual network interface (TUN), opened directly through the OS (no tun crate).
//!
//! The device carries raw IP packets. `read` / `write` take and return the bare IP packet;
//! platform framing (the 4-byte address family header on macOS utun) is handled inside.
//! Unix: `unix.rs` (utun / /dev/net/tun). Windows: `windows.rs` (wintun).

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::{open, Tun};

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::{open, Tun};
