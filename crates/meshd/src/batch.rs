//! Batched UDP send on a connected socket: one syscall for many datagrams.
//!
//! - macOS: `sendmsg_x`, a private but long-standing libsystem_kernel export (declared in xnu's
//!   sys/socket.h, KERNEL_PRIVATE section). The socket must be connected (msg_name is NULL).
//!   Its sibling `recvmsg_x` works on UDP sockets (64 datagrams per call, tested 2026-10-16) but
//!   returns only one packet per call on a utun socket, so it does not help reading the TUN.
//! - Linux: `sendmmsg`.
//! - Windows: one `send` per datagram (no per-peer connected sockets there, so this is only
//!   reached for completeness).

use std::io;
use std::net::UdpSocket;

/// Most datagrams handed to one call.
pub const MAX: usize = 64;

#[cfg(target_os = "macos")]
#[repr(C)]
struct MsghdrX {
    msg_name: *mut libc::c_void,
    msg_namelen: libc::socklen_t,
    msg_iov: *mut libc::iovec,
    msg_iovlen: libc::c_int,
    msg_control: *mut libc::c_void,
    msg_controllen: libc::socklen_t,
    msg_flags: libc::c_int,
    msg_datalen: libc::size_t,
}

#[cfg(target_os = "macos")]
extern "C" {
    fn sendmsg_x(s: libc::c_int, msgp: *const MsghdrX, cnt: libc::c_uint, flags: libc::c_int) -> isize;
}

/// Sends up to `MAX` of `bufs` as separate datagrams on a connected socket. Returns how many
/// were sent; the rest are dropped (full send buffer), which WireGuard and the inner TCP
/// tolerate.
#[cfg(unix)]
pub fn send_batch(sock: &UdpSocket, bufs: &[&[u8]]) -> io::Result<usize> {
    use std::os::fd::AsRawFd;
    let fd = sock.as_raw_fd();
    let bufs = &bufs[..bufs.len().min(MAX)];
    let mut iov: [libc::iovec; MAX] = unsafe { std::mem::zeroed() };
    for (v, b) in iov.iter_mut().zip(bufs) {
        *v = libc::iovec { iov_base: b.as_ptr() as *mut _, iov_len: b.len() };
    }
    #[cfg(target_os = "macos")]
    let n = unsafe {
        let mut hdrs: [MsghdrX; MAX] = std::mem::zeroed();
        for (h, v) in hdrs.iter_mut().zip(iov.iter_mut()).take(bufs.len()) {
            h.msg_iov = v;
            h.msg_iovlen = 1;
        }
        sendmsg_x(fd, hdrs.as_ptr(), bufs.len() as libc::c_uint, 0)
    };
    #[cfg(target_os = "linux")]
    let n = unsafe {
        let mut hdrs: [libc::mmsghdr; MAX] = std::mem::zeroed();
        for (h, v) in hdrs.iter_mut().zip(iov.iter_mut()).take(bufs.len()) {
            h.msg_hdr.msg_iov = v;
            h.msg_hdr.msg_iovlen = 1;
        }
        libc::sendmmsg(fd, hdrs.as_mut_ptr(), bufs.len() as libc::c_uint, 0) as isize
    };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(n as usize)
}

#[cfg(windows)]
pub fn send_batch(sock: &UdpSocket, bufs: &[&[u8]]) -> io::Result<usize> {
    let mut n = 0;
    for b in bufs {
        if sock.send(b).is_err() {
            break;
        }
        n += 1;
    }
    Ok(n)
}
