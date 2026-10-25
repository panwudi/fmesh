//! `<name>.mesh` answered locally from the roster: a minimal DNS responder on loopback.
//!
//! Only the `.mesh` suffix is routed here (macOS `/etc/resolver/mesh`, systemd-resolved
//! `~mesh` routing domain), so nothing else about the system's DNS changes, and no query ever
//! leaves the machine. A records only; other types for a known name get an empty answer.

use std::io;
use std::net::Ipv4Addr;

/// Loopback port of the responder. Not 53 on macOS / Linux (mDNSResponder, systemd-resolved
/// own it); 53 on Windows, whose NRPT rules cannot name a port (nothing listens there).
#[cfg(unix)]
pub const PORT: u16 = 53535;
#[cfg(windows)]
pub const PORT: u16 = 53;
const TTL: u32 = 60;
const QTYPE_A: u16 = 1;
const QCLASS_IN: u16 = 1;
const RCODE_NXDOMAIN: u8 = 3;
const RCODE_NOTIMP: u8 = 4;

/// Builds the reply to one query, or `None` for garbage (not worth answering).
pub fn answer(q: &[u8], lookup: impl Fn(&str) -> Option<Ipv4Addr>) -> Option<Vec<u8>> {
    if q.len() < 12 || q[2] & 0x80 != 0 || u16::from_be_bytes([q[4], q[5]]) != 1 {
        return None;
    }
    // QNAME: labels until a zero byte; no compression in a question.
    let mut i = 12;
    let mut labels: Vec<String> = vec![];
    loop {
        let len = *q.get(i)? as usize;
        i += 1;
        if len == 0 {
            break;
        }
        if len > 63 {
            return None;
        }
        labels.push(String::from_utf8_lossy(q.get(i..i + len)?).to_ascii_lowercase());
        i += len;
    }
    let qtype = u16::from_be_bytes([*q.get(i)?, *q.get(i + 1)?]);
    let qclass = u16::from_be_bytes([*q.get(i + 2)?, *q.get(i + 3)?]);
    let question_end = i + 4;

    let opcode = (q[2] >> 3) & 0x0F;
    let ip = match labels.as_slice() {
        [name, tld] if tld == "mesh" => lookup(name),
        _ => None,
    };
    let rcode = if opcode != 0 {
        RCODE_NOTIMP
    } else if ip.is_none() {
        RCODE_NXDOMAIN
    } else {
        0
    };
    let with_a = rcode == 0 && qtype == QTYPE_A && qclass == QCLASS_IN;

    let mut r = Vec::with_capacity(question_end + 16);
    r.extend_from_slice(&q[0..2]); // id
    r.push(0x84 | (q[2] & 0x01)); // QR, AA, copy RD
    r.push(rcode);
    r.extend_from_slice(&[0, 1, 0, with_a as u8, 0, 0, 0, 0]); // QD 1, AN 0/1, NS 0, AR 0
    r.extend_from_slice(&q[12..question_end]);
    if let (true, Some(ip)) = (with_a, ip) {
        r.extend_from_slice(&[0xC0, 0x0C]); // pointer to the question name
        r.extend_from_slice(&QTYPE_A.to_be_bytes());
        r.extend_from_slice(&QCLASS_IN.to_be_bytes());
        r.extend_from_slice(&TTL.to_be_bytes());
        r.extend_from_slice(&4u16.to_be_bytes());
        r.extend_from_slice(&ip.octets());
    }
    Some(r)
}

/// Routes `.mesh` lookups to the responder. macOS: `/etc/resolver/mesh`. Linux: the TUN link
/// gets the responder as DNS server and `~mesh` as routing domain in systemd-resolved; without
/// resolved, a marked block of `/etc/hosts` lists every node instead.
pub fn configure(ifname: &str, names: &[(String, Ipv4Addr)]) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        let _ = (ifname, names);
        std::fs::create_dir_all("/etc/resolver")?;
        std::fs::write("/etc/resolver/mesh", format!("# flytoMesh: written by meshd\nnameserver 127.0.0.1\nport {PORT}\n"))?;
    }
    #[cfg(target_os = "linux")]
    {
        let server = format!("127.0.0.1:{PORT}");
        let ok = std::process::Command::new("resolvectl")
            .args(["dns", ifname, &server])
            .status()
            .is_ok_and(|s| s.success())
            && std::process::Command::new("resolvectl")
                .args(["domain", ifname, "~mesh"])
                .status()
                .is_ok_and(|s| s.success());
        if !ok {
            write_hosts(names)?;
            eprintln!("meshd: no systemd-resolved, .mesh names written to /etc/hosts");
        }
    }
    // Windows: a Name Resolution Policy Table rule sends `.mesh` queries to the responder.
    #[cfg(windows)]
    {
        let _ = (ifname, names);
        crate::node::powershell(
            "Get-DnsClientNrptRule | Where-Object { $_.Namespace -eq '.mesh' } | Remove-DnsClientNrptRule -Force; \
             Add-DnsClientNrptRule -Namespace '.mesh' -NameServers '127.0.0.1'; Clear-DnsClientCache",
        );
    }
    Ok(())
}

#[cfg(target_os = "linux")]
const HOSTS_BEGIN: &str = "# flytomesh begin (managed by meshd, do not edit)";
#[cfg(target_os = "linux")]
const HOSTS_END: &str = "# flytomesh end";

/// Replaces (or appends) the managed block of /etc/hosts, leaving every other line alone.
#[cfg(target_os = "linux")]
fn write_hosts(names: &[(String, Ipv4Addr)]) -> io::Result<()> {
    let old = std::fs::read_to_string("/etc/hosts")?;
    let mut out = String::new();
    let mut skipping = false;
    for line in old.lines() {
        if line == HOSTS_BEGIN {
            skipping = true;
        } else if line == HOSTS_END {
            skipping = false;
        } else if !skipping {
            out.push_str(line);
            out.push('\n');
        }
    }
    out.push_str(HOSTS_BEGIN);
    out.push('\n');
    for (name, ip) in names {
        out.push_str(&format!("{ip} {name}.mesh\n"));
    }
    out.push_str(HOSTS_END);
    out.push('\n');
    if out != old {
        let tmp = "/etc/hosts.flytomesh.tmp";
        std::fs::write(tmp, &out)?;
        std::fs::rename(tmp, "/etc/hosts")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(name: &str, qtype: u16) -> Vec<u8> {
        let mut q = vec![0x12, 0x34, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
        for l in name.split('.') {
            q.push(l.len() as u8);
            q.extend_from_slice(l.as_bytes());
        }
        q.push(0);
        q.extend_from_slice(&qtype.to_be_bytes());
        q.extend_from_slice(&1u16.to_be_bytes());
        q
    }

    fn lookup(n: &str) -> Option<Ipv4Addr> {
        (n == "laptop2").then_some(Ipv4Addr::new(10, 249, 0, 2))
    }

    #[test]
    fn a_record() {
        let r = answer(&query("Laptop2.mesh", 1), lookup).unwrap();
        assert_eq!(&r[0..2], &[0x12, 0x34]);
        assert_eq!(r[3] & 0x0F, 0);
        assert_eq!(u16::from_be_bytes([r[6], r[7]]), 1);
        assert_eq!(&r[r.len() - 4..], &[10, 249, 0, 2]);
    }

    #[test]
    fn unknown_name_is_nxdomain() {
        let r = answer(&query("nope.mesh", 1), lookup).unwrap();
        assert_eq!(r[3] & 0x0F, RCODE_NXDOMAIN);
    }

    #[test]
    fn aaaa_for_known_name_is_empty() {
        let r = answer(&query("laptop2.mesh", 28), lookup).unwrap();
        assert_eq!(r[3] & 0x0F, 0);
        assert_eq!(u16::from_be_bytes([r[6], r[7]]), 0);
    }
}
