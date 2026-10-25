//! UDP sockets that share the node's port (SO_REUSEPORT).
//!
//! The kernel hands a datagram to the socket connected to its source, and everything else to
//! one of the unconnected ones (verified on macOS 15, Linux 5.10 and 6.1, see
//! `tools/reuseport_demux_test.py`). So each peer with a direct path gets its own connected
//! socket, which the send path needs: on macOS an unconnected `sendto` does a route lookup and
//! a temporary bind per call, which cost 77% of the send thread in the spike.
//!
//! Windows has no SO_REUSEPORT with these semantics: there the node uses one unconnected,
//! non-blocking socket for everything (see `imp` at the bottom).

pub use imp::*;

#[cfg(unix)]
mod imp {
use std::io;
use std::mem::{size_of, zeroed};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};

/// Socket buffers large enough that a burst in one direction does not drop the other
/// direction's packets (macOS default UDP receive space is 768 KB).
const SOCK_BUF: libc::c_int = 7 << 20;

fn setopt(fd: RawFd, level: libc::c_int, opt: libc::c_int, v: libc::c_int) {
    unsafe {
        libc::setsockopt(fd, level, opt, (&v as *const libc::c_int).cast(), size_of::<libc::c_int>() as libc::socklen_t);
    }
}

fn sockaddr(a: SocketAddrV4) -> libc::sockaddr_in {
    let mut s: libc::sockaddr_in = unsafe { zeroed() };
    #[cfg(target_os = "macos")]
    {
        s.sin_len = size_of::<libc::sockaddr_in>() as u8;
    }
    s.sin_family = libc::AF_INET as libc::sa_family_t;
    s.sin_port = a.port().to_be();
    s.sin_addr.s_addr = u32::from(*a.ip()).to_be();
    s
}

/// A UDP socket bound to 0.0.0.0:`port` that shares the port with the node's other sockets;
/// connected to `peer` when given, and pinned to interface `pin` when given.
pub fn open(port: u16, peer: Option<SocketAddrV4>, pin: Option<&str>) -> io::Result<UdpSocket> {
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let sock = unsafe { UdpSocket::from_raw_fd(fd) };
    setopt(fd, libc::SOL_SOCKET, libc::SO_REUSEADDR, 1);
    setopt(fd, libc::SOL_SOCKET, libc::SO_REUSEPORT, 1);
    setopt(fd, libc::SOL_SOCKET, libc::SO_RCVBUF, SOCK_BUF);
    setopt(fd, libc::SOL_SOCKET, libc::SO_SNDBUF, SOCK_BUF);
    if pin.is_some() {
        crate::netif::bind_interface(&sock, pin)?;
    }
    let local = sockaddr(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port));
    let len = size_of::<libc::sockaddr_in>() as libc::socklen_t;
    if unsafe { libc::bind(fd, (&local as *const libc::sockaddr_in).cast(), len) } < 0 {
        return Err(io::Error::last_os_error());
    }
    if let Some(p) = peer {
        let remote = sockaddr(p);
        if unsafe { libc::connect(fd, (&remote as *const libc::sockaddr_in).cast(), len) } < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(sock)
}

/// One datagram if one is queued, without blocking (and without making the socket
/// non-blocking, which would also affect the send thread).
pub fn recv_now(sock: &UdpSocket, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
    let mut from: libc::sockaddr_in = unsafe { zeroed() };
    let mut len = size_of::<libc::sockaddr_in>() as libc::socklen_t;
    let n = unsafe {
        libc::recvfrom(
            sock.as_raw_fd(),
            buf.as_mut_ptr().cast(),
            buf.len(),
            libc::MSG_DONTWAIT,
            (&mut from as *mut libc::sockaddr_in).cast(),
            &mut len,
        )
    };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    let ip = Ipv4Addr::from(u32::from_be(from.sin_addr.s_addr));
    Ok((n as usize, SocketAddr::V4(SocketAddrV4::new(ip, u16::from_be(from.sin_port)))))
}

/// Waits until one of `socks` is readable or `timeout_ms` passes; returns the readable ones.
pub fn wait(socks: &[&UdpSocket], timeout_ms: i32, ready: &mut Vec<usize>) -> io::Result<()> {
    let mut p: Vec<libc::pollfd> =
        socks.iter().map(|s| libc::pollfd { fd: s.as_raw_fd(), events: libc::POLLIN, revents: 0 }).collect();
    let n = unsafe { libc::poll(p.as_mut_ptr(), p.len() as libc::nfds_t, timeout_ms) };
    ready.clear();
    if n < 0 {
        let e = io::Error::last_os_error();
        return if e.kind() == io::ErrorKind::Interrupted { Ok(()) } else { Err(e) };
    }
    ready.extend(p.iter().enumerate().filter(|(_, x)| x.revents != 0).map(|(i, _)| i));
    Ok(())
}
}

#[cfg(windows)]
mod imp {
    use std::io;
    use std::net::{SocketAddr, SocketAddrV4, UdpSocket};
    use std::os::windows::io::AsRawSocket;
    use windows_sys::Win32::Networking::WinSock as ws;

    /// One unconnected, non-blocking socket on 0.0.0.0:`port` (`peer` and `pin` unsupported).
    pub fn open(port: u16, peer: Option<SocketAddrV4>, _pin: Option<&str>) -> io::Result<UdpSocket> {
        if peer.is_some() {
            return Err(io::Error::new(io::ErrorKind::Unsupported, "per-peer sockets need SO_REUSEPORT"));
        }
        let sock = UdpSocket::bind(("0.0.0.0", port))?;
        let size: i32 = 7 << 20;
        for opt in [ws::SO_RCVBUF, ws::SO_SNDBUF] {
            unsafe {
                ws::setsockopt(sock.as_raw_socket() as ws::SOCKET, ws::SOL_SOCKET, opt, (&size as *const i32).cast(), 4);
            }
        }
        sock.set_nonblocking(true)?;
        Ok(sock)
    }

    /// One datagram if one is queued (the socket is non-blocking).
    pub fn recv_now(sock: &UdpSocket, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        sock.recv_from(buf)
    }

    /// Waits until one of `socks` is readable or `timeout_ms` passes; returns the readable ones.
    pub fn wait(socks: &[&UdpSocket], timeout_ms: i32, ready: &mut Vec<usize>) -> io::Result<()> {
        let mut p: Vec<ws::WSAPOLLFD> = socks
            .iter()
            .map(|s| ws::WSAPOLLFD { fd: s.as_raw_socket() as ws::SOCKET, events: ws::POLLRDNORM, revents: 0 })
            .collect();
        let n = unsafe { ws::WSAPoll(p.as_mut_ptr(), p.len() as u32, timeout_ms) };
        ready.clear();
        if n < 0 {
            return Err(io::Error::from_raw_os_error(unsafe { ws::WSAGetLastError() }));
        }
        ready.extend(p.iter().enumerate().filter(|(_, x)| x.revents != 0).map(|(i, _)| i));
        Ok(())
    }
}
