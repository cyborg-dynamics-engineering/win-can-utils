//! Experimental real Windows network adapter using the signed Wintun miniport.
//! CAN v1 JSON is carried in UDP datagrams over a dedicated IPv4 link.
pub mod bridge;
pub mod ip;
#[cfg(windows)]
pub mod wintun;

use serde::{Deserialize, Serialize};
use std::{io, net::Ipv4Addr, time::Duration};

/// OS-facing layer-3 packet transport. Implementations must bound receive waits.
pub trait PacketDevice: Send + Sync + 'static {
    fn receive(&self, timeout: Duration) -> io::Result<Option<Vec<u8>>>;
    fn send(&self, packet: &[u8]) -> io::Result<()>;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NetworkConfig {
    pub local: Ipv4Addr,
    pub peer: Ipv4Addr,
    pub prefix_length: u8,
    pub port: u16,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            local: Ipv4Addr::new(198, 18, 0, 1),
            peer: Ipv4Addr::new(198, 18, 0, 2),
            prefix_length: 30,
            port: 29536,
        }
    }
}

impl NetworkConfig {
    pub fn validate(&self) -> io::Result<()> {
        let fail = || {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "Use distinct unicast host addresses in the same /1../30 subnet and a nonzero UDP port",
            )
        };
        if self.port == 0 || !(1..=30).contains(&self.prefix_length) || self.local == self.peer {
            return Err(fail());
        }
        let mask = u32::MAX << (32 - self.prefix_length);
        let local = u32::from(self.local);
        let peer = u32::from(self.peer);
        if local & mask != peer & mask {
            return Err(fail());
        }
        for ip in [self.local, self.peer] {
            let host = u32::from(ip) & !mask;
            if ip.is_unspecified()
                || ip.is_loopback()
                || ip.is_multicast()
                || ip.is_broadcast()
                || ip.octets()[0] == 0
                || ip.octets()[0] >= 240
                || host == 0
                || host == !mask
            {
                return Err(fail());
            }
        }
        Ok(())
    }
}
