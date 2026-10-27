//! IPv4/UDP framing for the point-to-point adapter. No IP forwarding or reassembly.
use super::NetworkConfig;
use std::net::Ipv4Addr;

pub const MAX_UDP_PAYLOAD: usize = 65_507;

#[derive(Debug, PartialEq, Eq)]
pub enum PacketError {
    Malformed(&'static str),
    Unsupported(&'static str),
    Foreign,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Incoming {
    Datagram { source_port: u16, payload: Vec<u8> },
    EchoReply(Vec<u8>),
}

fn sum(bytes: &[u8]) -> u32 {
    bytes
        .chunks(2)
        .map(|pair| u32::from(u16::from_be_bytes([pair[0], *pair.get(1).unwrap_or(&0)])))
        .sum()
}

fn finish(mut value: u32) -> u16 {
    while value >> 16 != 0 {
        value = (value & 0xffff) + (value >> 16);
    }
    !(value as u16)
}

pub fn checksum(bytes: &[u8]) -> u16 {
    finish(sum(bytes))
}

fn udp_checksum(source: Ipv4Addr, destination: Ipv4Addr, udp: &[u8]) -> u16 {
    finish(sum(&source.octets()) + sum(&destination.octets()) + 17 + udp.len() as u32 + sum(udp))
}

fn header(source: Ipv4Addr, destination: Ipv4Addr, protocol: u8, payload: &[u8]) -> Vec<u8> {
    let mut packet = vec![0u8; 20 + payload.len()];
    packet[0] = 0x45;
    let length = packet.len() as u16;
    packet[2..4].copy_from_slice(&length.to_be_bytes());
    packet[6] = 0x40; // Don't fragment; the adapter is configured with MTU 65535.
    packet[8] = 64;
    packet[9] = protocol;
    packet[12..16].copy_from_slice(&source.octets());
    packet[16..20].copy_from_slice(&destination.octets());
    let check = checksum(&packet[..20]);
    packet[10..12].copy_from_slice(&check.to_be_bytes());
    packet[20..].copy_from_slice(payload);
    packet
}

pub fn udp_packet(
    source: Ipv4Addr,
    destination: Ipv4Addr,
    source_port: u16,
    destination_port: u16,
    payload: &[u8],
) -> Result<Vec<u8>, PacketError> {
    if payload.len() > MAX_UDP_PAYLOAD || source_port == 0 || destination_port == 0 {
        return Err(PacketError::Malformed("UDP payload or port out of range"));
    }
    let mut udp = vec![0u8; 8 + payload.len()];
    udp[..2].copy_from_slice(&source_port.to_be_bytes());
    udp[2..4].copy_from_slice(&destination_port.to_be_bytes());
    let length = udp.len() as u16;
    udp[4..6].copy_from_slice(&length.to_be_bytes());
    udp[8..].copy_from_slice(payload);
    let check = udp_checksum(source, destination, &udp);
    udp[6..8].copy_from_slice(&if check == 0 { u16::MAX } else { check }.to_be_bytes());
    Ok(header(source, destination, 17, &udp))
}

pub fn parse(packet: &[u8], config: &NetworkConfig) -> Result<Incoming, PacketError> {
    use PacketError::*;
    if packet.len() < 20 {
        return Err(Malformed("Short IPv4 header"));
    }
    if packet[0] >> 4 != 4 {
        return Err(Unsupported("IPv4 only"));
    }
    let header_len = usize::from(packet[0] & 15) * 4;
    if header_len < 20 || header_len > packet.len() {
        return Err(Malformed("Invalid IPv4 header length"));
    }
    let length = usize::from(u16::from_be_bytes([packet[2], packet[3]]));
    if length != packet.len() || length < header_len {
        return Err(Malformed("Invalid IPv4 total length"));
    }
    if checksum(&packet[..header_len]) != 0 || packet[8] == 0 {
        return Err(Malformed("Invalid IPv4 checksum or TTL"));
    }
    if packet[12..16] != config.local.octets() || packet[16..20] != config.peer.octets() {
        return Err(Foreign);
    }
    let fragment = u16::from_be_bytes([packet[6], packet[7]]);
    if fragment & 0xbfff != 0 {
        return Err(Unsupported("Fragmented IPv4 is unsupported"));
    }
    let payload = &packet[header_len..];
    match packet[9] {
        17 => {
            if payload.len() < 8 {
                return Err(Malformed("Short UDP header"));
            }
            let source_port = u16::from_be_bytes([payload[0], payload[1]]);
            let destination_port = u16::from_be_bytes([payload[2], payload[3]]);
            let udp_len = usize::from(u16::from_be_bytes([payload[4], payload[5]]));
            if source_port == 0 || udp_len != payload.len() {
                return Err(Malformed("Invalid UDP length or source port"));
            }
            if destination_port != config.port {
                return Err(Foreign);
            }
            if payload[6..8] != [0, 0] && udp_checksum(config.local, config.peer, payload) != 0 {
                return Err(Malformed("Invalid UDP checksum"));
            }
            Ok(Incoming::Datagram {
                source_port,
                payload: payload[8..].to_vec(),
            })
        }
        1 => {
            if payload.len() < 8 || checksum(payload) != 0 {
                return Err(Malformed("Invalid ICMP checksum or length"));
            }
            if payload[..2] != [8, 0] {
                return Err(Unsupported("Only ICMP echo requests are supported"));
            }
            let mut reply = payload.to_vec();
            reply[0] = 0;
            reply[2..4].fill(0);
            let check = checksum(&reply);
            reply[2..4].copy_from_slice(&check.to_be_bytes());
            Ok(Incoming::EchoReply(header(
                config.peer,
                config.local,
                1,
                &reply,
            )))
        }
        _ => Err(Unsupported("Only UDP CAN and ICMP echo are supported")),
    }
}
