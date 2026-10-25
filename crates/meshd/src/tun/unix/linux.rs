//! Linux TUN via /dev/net/tun, no packet info header (IFF_NO_PI). The fd is non-blocking:
//! the send path drains it in batches, and `Tun::read` waits with poll.

use super::Tun;
use std::io;
use std::mem::zeroed;

const TUNSETIFF: libc::c_ulong = 0x400454ca;
pub const NAME: &str = "flytomesh";

/// Opens the TUN device `flytomesh`.
pub fn open() -> io::Result<Tun> {
    unsafe {
        let fd = libc::open(c"/dev/net/tun".as_ptr(), libc::O_RDWR | libc::O_CLOEXEC | libc::O_NONBLOCK);
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut tun = Tun { fd, name: String::new() };

        let mut req: libc::ifreq = zeroed();
        // A fixed name, so firewall rules can refer to it (`ufw allow in on flytomesh`).
        for (d, c) in req.ifr_name.iter_mut().zip(NAME.bytes()) {
            *d = c as libc::c_char;
        }
        req.ifr_ifru.ifru_flags = (libc::IFF_TUN | libc::IFF_NO_PI) as libc::c_short;
        if libc::ioctl(fd, TUNSETIFF as _, &mut req) < 0 {
            return Err(io::Error::last_os_error());
        }

        let name: Vec<u8> = req
            .ifr_name
            .iter()
            .take_while(|&&c| c != 0)
            .map(|&c| c as u8)
            .collect();
        tun.name = String::from_utf8_lossy(&name).into_owned();
        Ok(tun)
    }
}
