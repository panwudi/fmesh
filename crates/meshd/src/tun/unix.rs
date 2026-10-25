//! TUN on macOS (utun) and Linux (/dev/net/tun).

use std::io;
use std::os::fd::RawFd;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::open;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::open;

pub struct Tun {
    fd: RawFd,
    name: String,
}

impl Tun {
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Reads one IP packet into `buf` (blocking). Returns its length.
    pub fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        #[cfg(target_os = "macos")]
        {
            // utun prefixes every packet with a 4-byte address family; read it into a
            // scratch header so the packet itself lands at buf[0] with no copy.
            let mut hdr = [0u8; 4];
            let iov = [
                libc::iovec { iov_base: hdr.as_mut_ptr().cast(), iov_len: 4 },
                libc::iovec { iov_base: buf.as_mut_ptr().cast(), iov_len: buf.len() },
            ];
            let n = unsafe { libc::readv(self.fd, iov.as_ptr(), 2) };
            if n < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok((n as usize).saturating_sub(4))
        }
        #[cfg(target_os = "linux")]
        loop {
            match self.read_nb(buf) {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    let mut p = libc::pollfd { fd: self.fd, events: libc::POLLIN, revents: 0 };
                    if unsafe { libc::poll(&mut p, 1, -1) } < 0 {
                        let e = io::Error::last_os_error();
                        if e.kind() != io::ErrorKind::Interrupted {
                            return Err(e);
                        }
                    }
                }
                r => return r,
            }
        }
    }

    /// Like `read` but never blocks: `WouldBlock` when no packet is queued.
    #[cfg(target_os = "linux")]
    pub fn read_nb(&self, buf: &mut [u8]) -> io::Result<usize> {
        let n = unsafe { libc::read(self.fd, buf.as_mut_ptr().cast(), buf.len()) };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(n as usize)
    }

    /// Like `read` but never blocks: `WouldBlock` when no packet is queued.
    /// utun is a socket, so MSG_DONTWAIT works per call without making the fd non-blocking.
    #[cfg(target_os = "macos")]
    pub fn read_nb(&self, buf: &mut [u8]) -> io::Result<usize> {
        let mut hdr = [0u8; 4];
        let mut iov = [
            libc::iovec { iov_base: hdr.as_mut_ptr().cast(), iov_len: 4 },
            libc::iovec { iov_base: buf.as_mut_ptr().cast(), iov_len: buf.len() },
        ];
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_iov = iov.as_mut_ptr();
        msg.msg_iovlen = 2;
        let n = unsafe { libc::recvmsg(self.fd, &mut msg, libc::MSG_DONTWAIT) };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok((n as usize).saturating_sub(4))
    }

    /// Writes one IP packet. `v6` selects the address family header on macOS.
    pub fn write(&self, pkt: &[u8], v6: bool) -> io::Result<()> {
        #[cfg(target_os = "macos")]
        let n = {
            let af: u32 = if v6 { libc::AF_INET6 as u32 } else { libc::AF_INET as u32 };
            let hdr = af.to_be_bytes();
            let iov = [
                libc::iovec { iov_base: hdr.as_ptr() as *mut _, iov_len: 4 },
                libc::iovec { iov_base: pkt.as_ptr() as *mut _, iov_len: pkt.len() },
            ];
            unsafe { libc::writev(self.fd, iov.as_ptr(), 2) }
        };
        #[cfg(target_os = "linux")]
        let n = {
            let _ = v6;
            unsafe { libc::write(self.fd, pkt.as_ptr().cast(), pkt.len()) }
        };
        if n < 0 {
            let e = io::Error::last_os_error();
            // A full TUN queue drops the packet; the inner TCP recovers.
            if matches!(e.raw_os_error(), Some(libc::ENOBUFS) | Some(libc::EAGAIN)) {
                return Ok(());
            }
            return Err(e);
        }
        Ok(())
    }
}

impl Drop for Tun {
    fn drop(&mut self) {
        unsafe { libc::close(self.fd) };
    }
}
