// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Flipper FZCO

//! The USB network link a host installs over, with no Ethernet anywhere.
//!
//! The dual-role Type-C port becomes a USB NCM network adapter ([`gadget`]),
//! the board takes the first address of a small private subnet on it
//! ([`iface`]), and a DHCP server hands the host the second one ([`dhcp`]) —
//! with no default route and no DNS server, so plugging a laptop in never
//! diverts its own traffic through the board.

pub mod dhcp;
pub mod gadget;
pub mod iface;

use std::net::Ipv4Addr;
use std::sync::Arc;

/// Bring the USB link up: the gadget on `udc` (or the only controller), the
/// board's address on it, and the DHCP server on a thread of its own.
///
/// Returns the interface name. `log` receives the DHCP server's lease lines,
/// and its failure should it ever stop.
pub fn start(
    udc: Option<&str>,
    subnet: Subnet,
    log: Arc<dyn Fn(String) + Send + Sync>,
) -> Result<String, String> {
    let g = gadget::setup(udc)?;
    iface::configure(&g.ifname, subnet)?;
    let ifname = g.ifname.clone();
    std::thread::Builder::new()
        .name("dhcp".to_string())
        .spawn(move || {
            if let Err(e) = dhcp::serve(&ifname, subnet, &*log) {
                log(format!("usb: the DHCP server stopped: {e}"));
            }
        })
        .map_err(|e| format!("start the DHCP server: {e}"))?;
    Ok(g.ifname)
}

/// The board's address on the USB link and the link's prefix length.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Subnet {
    pub addr: Ipv4Addr,
    pub prefix: u8,
}

impl Subnet {
    /// `192.168.241.1/24` — 241 is `0xf1`, for Flipper One.
    pub const DEFAULT: Subnet = Subnet {
        addr: Ipv4Addr::new(192, 168, 241, 1),
        prefix: 24,
    };

    /// Parse `ADDR/PREFIX`. The board takes `ADDR`; the host gets the next
    /// address, so both must be usable hosts of the subnet.
    pub fn parse(s: &str) -> Result<Subnet, String> {
        let (addr, prefix) = s
            .split_once('/')
            .ok_or_else(|| format!("'{s}' is not ADDR/PREFIX"))?;
        let addr: Ipv4Addr = addr
            .parse()
            .map_err(|e| format!("'{addr}' is not an IPv4 address: {e}"))?;
        let prefix: u8 = prefix
            .parse()
            .ok()
            .filter(|p| (1..=30).contains(p))
            .ok_or_else(|| format!("'{prefix}' is not a prefix length from 1 to 30"))?;
        let subnet = Subnet { addr, prefix };
        let board = u32::from(addr);
        let host = u32::from(subnet.host());
        let mask = u32::from(subnet.mask());
        if board & !mask == 0 || host & mask != board & mask || host & !mask == !mask {
            return Err(format!(
                "{s} leaves no room for the host: the board takes {addr} and the host the \
                 address after it, and neither may be the network or broadcast address"
            ));
        }
        Ok(subnet)
    }

    pub fn mask(&self) -> Ipv4Addr {
        Ipv4Addr::from(u32::MAX << (32 - self.prefix))
    }

    /// The one address leased to the host: the board's plus one.
    pub fn host(&self) -> Ipv4Addr {
        Ipv4Addr::from(u32::from(self.addr).wrapping_add(1))
    }
}

impl std::fmt::Display for Subnet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.addr, self.prefix)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_link_subnet() {
        assert_eq!(Subnet::parse("192.168.241.1/24").unwrap(), Subnet::DEFAULT);
        let s = Subnet::parse("10.0.0.5/30").unwrap();
        assert_eq!(s.mask(), Ipv4Addr::new(255, 255, 255, 252));
        assert_eq!(s.host(), Ipv4Addr::new(10, 0, 0, 6));
        assert_eq!(Subnet::DEFAULT.host(), Ipv4Addr::new(192, 168, 241, 2));
        assert_eq!(Subnet::DEFAULT.to_string(), "192.168.241.1/24");

        assert!(Subnet::parse("192.168.241.1").is_err());
        assert!(Subnet::parse("192.168.241.1/31").is_err());
        assert!(Subnet::parse("host/24").is_err());
        // The network address itself, and a board address whose successor is
        // the broadcast address.
        assert!(Subnet::parse("192.168.241.0/24").is_err());
        assert!(Subnet::parse("192.168.241.254/24").is_err());
        // The broadcast address itself, whose successor is in the next subnet.
        assert!(Subnet::parse("10.0.0.255/24").is_err());
    }
}
