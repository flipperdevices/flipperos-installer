// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Flipper FZCO

//! Addressing a network interface and bringing it up, through the classic
//! socket ioctls — the initramfs needs no `ip` binary for it.

use std::io;
use std::net::Ipv4Addr;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use super::Subnet;

pub type Result<T> = std::result::Result<T, String>;

/// Give `ifname` the board's address on `subnet`, and bring it up.
///
/// Only an address and its netmask: the kernel derives the on-link route from
/// them, and nothing here touches the default route.
pub fn configure(ifname: &str, subnet: Subnet) -> Result<()> {
    let sock = socket().map_err(|e| format!("socket: {e}"))?;
    let what = |op: &str, e: io::Error| format!("{op} on {ifname}: {e}");

    let mut req = ifreq(ifname)?;
    req.ifr_ifru.ifru_addr = sockaddr(subnet.addr);
    ioctl(&sock, libc::SIOCSIFADDR, &mut req).map_err(|e| what("set address", e))?;

    let mut req = ifreq(ifname)?;
    req.ifr_ifru.ifru_netmask = sockaddr(subnet.mask());
    ioctl(&sock, libc::SIOCSIFNETMASK, &mut req).map_err(|e| what("set netmask", e))?;

    let mut req = ifreq(ifname)?;
    ioctl(&sock, libc::SIOCGIFFLAGS, &mut req).map_err(|e| what("read flags", e))?;
    // SAFETY: SIOCGIFFLAGS filled in the flags member of the union.
    let flags = unsafe { req.ifr_ifru.ifru_flags };
    req.ifr_ifru.ifru_flags = flags | libc::IFF_UP as libc::c_short;
    ioctl(&sock, libc::SIOCSIFFLAGS, &mut req).map_err(|e| what("bring up", e))?;
    Ok(())
}

fn socket() -> io::Result<OwnedFd> {
    // SAFETY: a plain socket(2) call; the descriptor is owned from here on.
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is a freshly opened descriptor nobody else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn ifreq(ifname: &str) -> Result<libc::ifreq> {
    if ifname.is_empty() || ifname.len() >= libc::IFNAMSIZ {
        return Err(format!("'{ifname}' is not a valid interface name"));
    }
    // SAFETY: an all-zero ifreq is a valid value of this plain C struct.
    let mut req: libc::ifreq = unsafe { std::mem::zeroed() };
    for (dst, src) in req.ifr_name.iter_mut().zip(ifname.bytes()) {
        *dst = src as libc::c_char;
    }
    Ok(req)
}

fn sockaddr(addr: Ipv4Addr) -> libc::sockaddr {
    let sin = libc::sockaddr_in {
        sin_family: libc::AF_INET as libc::sa_family_t,
        sin_port: 0,
        sin_addr: libc::in_addr {
            s_addr: u32::from(addr).to_be(),
        },
        sin_zero: [0; 8],
    };
    // SAFETY: sockaddr_in and sockaddr are the same size, and the kernel reads
    // this one as the sockaddr_in its family says it is.
    unsafe { std::mem::transmute::<libc::sockaddr_in, libc::sockaddr>(sin) }
}

fn ioctl(sock: &OwnedFd, request: libc::Ioctl, req: &mut libc::ifreq) -> io::Result<()> {
    // SAFETY: every request used here takes a pointer to an ifreq.
    let rc = unsafe { libc::ioctl(sock.as_raw_fd(), request, req as *mut libc::ifreq) };
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}
