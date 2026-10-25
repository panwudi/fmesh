//! Local IPv4 addresses worth offering a peer as direct-path candidates.

use std::net::{Ipv4Addr, UdpSocket};

/// Up, non-loopback IPv4 addresses, excluding our own TUN and address ranges that cannot be
/// a path to a peer: link-local, Tailscale (100.64/10), VPN fake-IP ranges (198.18/15) and
/// the mesh itself (10.249/16).
pub struct LocalAddr {
    pub ifname: String,
    pub ip: Ipv4Addr,
    pub mask: Ipv4Addr,
}

impl LocalAddr {
    /// `dst` is on this interface's subnet, reachable without a gateway.
    pub fn on_link(&self, dst: Ipv4Addr) -> bool {
        let m = u32::from(self.mask);
        m != 0 && u32::from(self.ip) & m == u32::from(dst) & m
    }
}

pub fn is_tunnel(ifname: &str) -> bool {
    ["utun", "ipsec", "ppp", "tun", "wg"].iter().any(|p| ifname.starts_with(p))
}

#[cfg(unix)]
pub fn local_ipv4(own_tun: &str) -> Vec<LocalAddr> {
    use std::ffi::CStr;
    let mut out = vec![];
    unsafe {
        let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut head) != 0 {
            return out;
        }
        let mut cur = head;
        while !cur.is_null() {
            let ifa = &*cur;
            cur = ifa.ifa_next;
            if ifa.ifa_addr.is_null() || (*ifa.ifa_addr).sa_family as i32 != libc::AF_INET {
                continue;
            }
            let flags = ifa.ifa_flags as libc::c_int;
            if flags & libc::IFF_UP == 0 || flags & libc::IFF_LOOPBACK != 0 {
                continue;
            }
            let name = CStr::from_ptr(ifa.ifa_name).to_string_lossy();
            if name == own_tun {
                continue;
            }
            let sin = &*(ifa.ifa_addr as *const libc::sockaddr_in);
            let ip = Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
            let mask = if ifa.ifa_netmask.is_null() {
                Ipv4Addr::UNSPECIFIED
            } else {
                let m = &*(ifa.ifa_netmask as *const libc::sockaddr_in);
                Ipv4Addr::from(u32::from_be(m.sin_addr.s_addr))
            };
            if usable(ip) && !out.iter().any(|a: &LocalAddr| a.ip == ip) {
                out.push(LocalAddr { ifname: name.into_owned(), ip, mask });
            }
        }
        libc::freeifaddrs(head);
    }
    out
}

/// Windows: the address of the interface that carries the default route, found by asking the
/// kernel which source address it would use (no packet is sent). Enough for candidates; the
/// netmask only matters for VPN pinning, which Windows does not do.
#[cfg(windows)]
pub fn local_ipv4(_own_tun: &str) -> Vec<LocalAddr> {
    let probe = UdpSocket::bind(("0.0.0.0", 0)).and_then(|s| s.connect(("1.1.1.1", 53)).map(|_| s));
    match probe.and_then(|s| s.local_addr()) {
        Ok(std::net::SocketAddr::V4(a)) if usable(*a.ip()) => {
            vec![LocalAddr { ifname: "default".into(), ip: *a.ip(), mask: Ipv4Addr::UNSPECIFIED }]
        }
        _ => vec![],
    }
}

fn usable(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    !(ip.is_link_local()
        || ip.is_unspecified()
        || (o[0] == 100 && (o[1] & 0xC0) == 64)
        || (o[0] == 198 && (o[1] & 0xFE) == 18)
        || (o[0] == 10 && o[1] == 249))
}

/// The physical interface to pin our socket to, when a VPN owns the default route.
///
/// A system VPN (e.g. a VPN app) makes a utun the primary default route, so
/// our UDP would go through it. Like Tailscale, we then bind the socket to the interface of
/// the first default route that has a real gateway. `None` means use normal routing (no VPN
/// in front, or no physical default route).
#[cfg(target_os = "macos")]
pub fn vpn_bypass_interface() -> Option<String> {
    let out = std::process::Command::new("netstat").args(["-rn", "-f", "inet"]).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let defaults: Vec<(String, String)> = text
        .lines()
        .filter(|l| l.starts_with("default"))
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            Some((f.get(1)?.to_string(), f.get(3)?.to_string()))
        })
        .collect();
    let tunnel = is_tunnel;
    let (_, primary) = defaults.first()?;
    if !tunnel(primary) {
        return None;
    }
    defaults
        .iter()
        .find(|(gw, ifname)| gw.parse::<Ipv4Addr>().is_ok() && !tunnel(ifname))
        .map(|(_, ifname)| ifname.clone())
}

#[cfg(not(target_os = "macos"))]
pub fn vpn_bypass_interface() -> Option<String> {
    None
}

/// Pins (or with `None`, unpins) the socket's outgoing interface.
#[cfg(target_os = "macos")]
pub fn bind_interface(sock: &UdpSocket, ifname: Option<&str>) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    let fd = sock.as_raw_fd();
    let idx: libc::c_uint = match ifname {
        Some(n) => {
            let c = std::ffi::CString::new(n).map_err(|_| std::io::Error::other("bad ifname"))?;
            match unsafe { libc::if_nametoindex(c.as_ptr()) } {
                0 => return Err(std::io::Error::last_os_error()),
                i => i,
            }
        }
        None => 0,
    };
    let r = unsafe {
        libc::setsockopt(
            fd,
            libc::IPPROTO_IP,
            libc::IP_BOUND_IF,
            (&idx as *const libc::c_uint).cast(),
            std::mem::size_of::<libc::c_uint>() as libc::socklen_t,
        )
    };
    if r < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
pub fn bind_interface(_sock: &UdpSocket, _ifname: Option<&str>) -> std::io::Result<()> {
    Ok(())
}
