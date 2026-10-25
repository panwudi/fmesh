//! macOS utun via the kernel control socket "com.apple.net.utun_control".
//! Needs root. No NetworkExtension, no signing.

use super::Tun;
use std::ffi::CStr;
use std::io;
use std::mem::{size_of, zeroed};

const UTUN_CONTROL_NAME: &[u8] = b"com.apple.net.utun_control";
const UTUN_OPT_IFNAME: libc::c_int = 2;

/// Opens a new utun device; the kernel picks the unit (utunN).
pub fn open() -> io::Result<Tun> {
    unsafe {
        let fd = libc::socket(libc::PF_SYSTEM, libc::SOCK_DGRAM, libc::SYSPROTO_CONTROL);
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut tun = Tun { fd, name: String::new() };

        let mut info: libc::ctl_info = zeroed();
        for (dst, src) in info.ctl_name.iter_mut().zip(UTUN_CONTROL_NAME) {
            *dst = *src as libc::c_char;
        }
        if libc::ioctl(fd, libc::CTLIOCGINFO, &mut info) < 0 {
            return Err(io::Error::last_os_error());
        }

        let addr = libc::sockaddr_ctl {
            sc_len: size_of::<libc::sockaddr_ctl>() as u8,
            sc_family: libc::AF_SYSTEM as u8,
            ss_sysaddr: libc::AF_SYS_CONTROL as u16,
            sc_id: info.ctl_id,
            sc_unit: 0, // 0 = let the kernel choose the next free utunN
            sc_reserved: [0; 5],
        };
        if libc::connect(
            fd,
            (&addr as *const libc::sockaddr_ctl).cast(),
            size_of::<libc::sockaddr_ctl>() as libc::socklen_t,
        ) < 0
        {
            return Err(io::Error::last_os_error());
        }

        let mut name = [0u8; libc::IFNAMSIZ];
        let mut len = name.len() as libc::socklen_t;
        if libc::getsockopt(
            fd,
            libc::SYSPROTO_CONTROL,
            UTUN_OPT_IFNAME,
            name.as_mut_ptr().cast(),
            &mut len,
        ) < 0
        {
            return Err(io::Error::last_os_error());
        }

        tun.name = CStr::from_bytes_until_nul(&name)
            .map_err(|_| io::Error::other("utun name not terminated"))?
            .to_string_lossy()
            .into_owned();
        Ok(tun)
    }
}
