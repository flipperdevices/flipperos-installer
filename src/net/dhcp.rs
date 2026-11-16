// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Flipper FZCO

//! A one-lease DHCP server for the USB link.
//!
//! The protocol, the lease table and the packet format are `edge-dhcp`'s; this
//! is the socket around them. One host sits on the far end of the cable, so the
//! pool is a single address — the board's plus one — and the replies carry the
//! address, the netmask and the lease time, but never a router or a DNS server:
//! the host must not send its own traffic through the board.

use std::io;
use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
use std::os::fd::FromRawFd;
use std::time::{SystemTime, UNIX_EPOCH};

use edge_dhcp::server::{Server, ServerOptions};
use edge_dhcp::{DhcpOption, MessageType, Options, Packet};

use super::Subnet;

/// How long a lease lasts. The host renews it at half that, and nothing about
/// the link ever changes, so long is fine.
const LEASE_SECS: u32 = 24 * 60 * 60;

const SERVER_PORT: u16 = 67;
const CLIENT_PORT: u16 = 68;

/// The lease table and the reply options for one link.
pub struct Responder {
    server: Server<fn() -> u64, 4>,
    subnet: Subnet,
}

impl Responder {
    pub fn new(subnet: Subnet) -> Self {
        let mut server = Server::new(now as fn() -> u64, subnet.addr);
        server.range_start = subnet.host();
        server.range_end = subnet.host();
        Self { server, subnet }
    }

    /// Answer one request datagram, encoding the reply into `out`.
    ///
    /// `None` for anything that warrants no answer: a malformed packet, another
    /// server's business, a release.
    pub fn answer<'o>(&mut self, request: &[u8], out: &'o mut [u8]) -> Option<Answer<'o>> {
        let request = Packet::decode(request).ok()?;
        // No gateway, no DNS: `ServerOptions` leaves both options out when
        // these are empty.
        let mut options = ServerOptions::new(self.subnet.addr, None);
        options.subnet = Some(self.subnet.mask());
        options.lease_duration_secs = LEASE_SECS;
        let mut opt_buf = Options::buf();
        let reply = self
            .server
            .handle_request(&mut opt_buf, &options, &request)?;
        let kind = reply.options.iter().find_map(|o| match o {
            DhcpOption::MessageType(t) => Some(t),
            _ => None,
        })?;
        let mac = reply.chaddr;
        let yiaddr = reply.yiaddr;
        let bytes = reply.encode(out).ok()?;
        Some(Answer {
            bytes,
            kind,
            yiaddr,
            mac,
        })
    }
}

/// An encoded reply, with what is worth logging about it.
pub struct Answer<'o> {
    pub bytes: &'o [u8],
    pub kind: MessageType,
    pub yiaddr: Ipv4Addr,
    pub mac: [u8; 16],
}

impl Answer<'_> {
    /// A line for the activity log, for the replies worth one.
    pub fn describe(&self) -> Option<String> {
        let mac = self.mac[..6]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join(":");
        match self.kind {
            MessageType::Ack => Some(format!("usb: leased {} to {mac}", self.yiaddr)),
            MessageType::Nak => Some(format!("usb: refused {mac} the address it asked for")),
            _ => None,
        }
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Serve DHCP on `ifname` until the socket fails. Blocks; run it on a thread
/// of its own. `log` gets one line per lease.
pub fn serve(ifname: &str, subnet: Subnet, log: &dyn Fn(String)) -> io::Result<()> {
    let socket = bind(ifname)?;
    let mut responder = Responder::new(subnet);
    let mut buf = [0u8; 1500];
    let mut out = [0u8; 1500];
    let broadcast = SocketAddrV4::new(Ipv4Addr::BROADCAST, CLIENT_PORT);
    loop {
        let (n, _) = socket.recv_from(&mut buf)?;
        let Some(answer) = responder.answer(&buf[..n], &mut out) else {
            continue;
        };
        // The host has no address yet, so it can only hear a broadcast.
        if let Err(e) = socket.send_to(answer.bytes, broadcast) {
            log(format!("usb: dhcp reply failed: {e}"));
            continue;
        }
        if let Some(line) = answer.describe() {
            log(line);
        }
    }
}

/// A UDP socket on the DHCP server port that only sees, and only sends on,
/// `ifname`: binding to the interface before the port keeps a server meant for
/// the USB cable off any Ethernet the board is also on.
fn bind(ifname: &str) -> io::Result<UdpSocket> {
    // SAFETY: plain socket(2); the descriptor is handed to UdpSocket below,
    // or closed on the way out of each failure.
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is ours; from here on UdpSocket closes it, failure or not.
    let socket = unsafe { UdpSocket::from_raw_fd(fd) };
    let one: libc::c_int = 1;
    setsockopt(fd, libc::SO_REUSEADDR, &one.to_ne_bytes())?;
    setsockopt(fd, libc::SO_BROADCAST, &one.to_ne_bytes())?;
    setsockopt(fd, libc::SO_BINDTODEVICE, ifname.as_bytes())?;
    let addr = libc::sockaddr_in {
        sin_family: libc::AF_INET as libc::sa_family_t,
        sin_port: SERVER_PORT.to_be(),
        sin_addr: libc::in_addr { s_addr: 0 },
        sin_zero: [0; 8],
    };
    // SAFETY: a complete sockaddr_in and its size.
    let rc = unsafe {
        libc::bind(
            fd,
            &addr as *const libc::sockaddr_in as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(socket)
}

fn setsockopt(fd: libc::c_int, option: libc::c_int, value: &[u8]) -> io::Result<()> {
    // SAFETY: `value` is a live buffer of the length passed.
    let rc = unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            option,
            value.as_ptr() as *const libc::c_void,
            value.len() as libc::socklen_t,
        )
    };
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAC: [u8; 6] = [0x06, 0x11, 0x22, 0x33, 0x44, 0x55];

    /// What a typical client asks for: mask, router, DNS, domain, lease time.
    const WANTS: &[u8] = &[1, 3, 6, 15, 51];

    fn request(kind: MessageType, requested: Option<Ipv4Addr>) -> Vec<u8> {
        let mut opts = vec![
            DhcpOption::MessageType(kind),
            DhcpOption::ParameterRequestList(WANTS),
        ];
        if let Some(ip) = requested {
            opts.push(DhcpOption::RequestedIpAddress(ip));
        }
        let packet = Packet::new_request(MAC, 0x1234, 0, None, true, Options::new(&opts));
        let mut buf = [0u8; 600];
        packet.encode(&mut buf).unwrap().to_vec()
    }

    #[test]
    fn offers_the_one_address_with_no_route_and_no_dns() {
        let mut r = Responder::new(Subnet::DEFAULT);
        let mut out = [0u8; 600];
        let answer = r
            .answer(&request(MessageType::Discover, None), &mut out)
            .expect("an offer");
        assert_eq!(answer.kind, MessageType::Offer);
        let bytes = answer.bytes.to_vec();
        // A BOOTP reply fills at least the classic 300-byte datagram.
        assert!(bytes.len() >= 272);

        let offer = Packet::decode(&bytes).unwrap();
        assert!(offer.reply);
        assert_eq!(offer.xid, 0x1234);
        assert_eq!(offer.yiaddr, Ipv4Addr::new(192, 168, 241, 2));
        let opts: Vec<DhcpOption> = offer.options.iter().collect();
        assert!(opts.contains(&DhcpOption::SubnetMask(Ipv4Addr::new(255, 255, 255, 0))));
        assert!(opts.contains(&DhcpOption::ServerIdentifier(Ipv4Addr::new(
            192, 168, 241, 1
        ))));
        assert!(opts.contains(&DhcpOption::IpAddressLeaseTime(LEASE_SECS)));
        // Asked for, and still not given.
        assert!(!opts.iter().any(|o| matches!(o, DhcpOption::Router(_))));
        assert!(!opts
            .iter()
            .any(|o| matches!(o, DhcpOption::DomainNameServer(_))));
    }

    #[test]
    fn acks_the_offered_address_and_naks_any_other() {
        let mut r = Responder::new(Subnet::DEFAULT);
        let mut out = [0u8; 600];
        let host = Ipv4Addr::new(192, 168, 241, 2);
        let ack = r
            .answer(&request(MessageType::Request, Some(host)), &mut out)
            .unwrap();
        assert_eq!(ack.kind, MessageType::Ack);
        assert_eq!(ack.yiaddr, host);
        assert_eq!(
            ack.describe().as_deref(),
            Some("usb: leased 192.168.241.2 to 06:11:22:33:44:55")
        );

        // A host remembering an address from some other network is told no,
        // and will start over with a DISCOVER.
        let nak = r
            .answer(
                &request(MessageType::Request, Some(Ipv4Addr::new(10, 0, 0, 7))),
                &mut out,
            )
            .unwrap();
        assert_eq!(nak.kind, MessageType::Nak);
    }

    #[test]
    fn ignores_what_is_not_a_request() {
        let mut r = Responder::new(Subnet::DEFAULT);
        let mut out = [0u8; 600];
        assert!(r.answer(b"not dhcp", &mut out).is_none());
        assert!(r
            .answer(&request(MessageType::Release, None), &mut out)
            .is_none());
    }
}
