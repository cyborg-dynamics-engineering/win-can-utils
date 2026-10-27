use std::net::Ipv4Addr;
use win_can_utils::network_adapter::{
    NetworkConfig,
    ip::{self, Incoming, PacketError},
};

fn packet(payload: &[u8]) -> Vec<u8> {
    let config = NetworkConfig::default();
    ip::udp_packet(config.local, config.peer, 43000, config.port, payload).unwrap()
}

fn repair_header(packet: &mut [u8]) {
    let length = usize::from(packet[0] & 15) * 4;
    packet[10..12].fill(0);
    let checksum = ip::checksum(&packet[..length]);
    packet[10..12].copy_from_slice(&checksum.to_be_bytes());
}

#[test]
fn udp_roundtrip_handles_odd_binary_and_maximum_payloads() {
    for length in [0, 1, 8, 15, 512, ip::MAX_UDP_PAYLOAD] {
        let payload: Vec<u8> = (0..length).map(|n| n as u8).collect();
        let bytes = packet(&payload);
        assert_eq!(bytes.len(), length + 28);
        assert_eq!(
            ip::parse(&bytes, &NetworkConfig::default()).unwrap(),
            Incoming::Datagram {
                source_port: 43000,
                payload
            }
        );
    }
    let config = NetworkConfig::default();
    assert!(
        ip::udp_packet(
            config.local,
            config.peer,
            1,
            2,
            &vec![0; ip::MAX_UDP_PAYLOAD + 1]
        )
        .is_err()
    );
}

#[test]
fn malformed_lengths_checksums_and_truncated_headers_are_rejected_without_panics() {
    let original = packet(&[0, 10, 255, 128, 0]);
    for length in 0..original.len() {
        assert!(ip::parse(&original[..length], &NetworkConfig::default()).is_err());
    }
    for index in 0..original.len() {
        let mut bytes = original.clone();
        bytes[index] ^= 1;
        assert!(
            ip::parse(&bytes, &NetworkConfig::default()).is_err(),
            "Undetected corruption at {index}"
        );
    }
    let mut zero_udp_checksum = original.clone();
    zero_udp_checksum[26..28].fill(0);
    assert!(
        ip::parse(&zero_udp_checksum, &NetworkConfig::default()).is_ok(),
        "IPv4 permits an absent UDP checksum"
    );
    let mut bad_ihl = original.clone();
    bad_ihl[0] = 0x4f;
    assert!(ip::parse(&bad_ihl, &NetworkConfig::default()).is_err());
}

#[test]
fn fragmentation_other_protocols_and_foreign_destinations_are_not_forwarded() {
    let mut fragmented = packet(b"test");
    fragmented[6] = 0x20;
    repair_header(&mut fragmented);
    assert!(matches!(
        ip::parse(&fragmented, &NetworkConfig::default()),
        Err(PacketError::Unsupported(_))
    ));
    let mut tcp = packet(b"test");
    tcp[9] = 6;
    repair_header(&mut tcp);
    assert!(matches!(
        ip::parse(&tcp, &NetworkConfig::default()),
        Err(PacketError::Unsupported(_))
    ));
    let config = NetworkConfig::default();
    let foreign = ip::udp_packet(
        Ipv4Addr::new(192, 0, 2, 1),
        config.peer,
        123,
        config.port,
        b"test",
    )
    .unwrap();
    assert_eq!(ip::parse(&foreign, &config), Err(PacketError::Foreign));
    let other_port =
        ip::udp_packet(config.local, config.peer, 123, config.port + 1, b"test").unwrap();
    assert_eq!(ip::parse(&other_port, &config), Err(PacketError::Foreign));
}

#[test]
fn ipv4_options_do_not_shift_the_udp_payload() {
    let mut bytes = packet(b"CAN");
    bytes.splice(20..20, [1, 1, 0, 0]);
    bytes[0] = 0x46;
    let length = bytes.len() as u16;
    bytes[2..4].copy_from_slice(&length.to_be_bytes());
    repair_header(&mut bytes);
    assert_eq!(
        ip::parse(&bytes, &NetworkConfig::default()).unwrap(),
        Incoming::Datagram {
            source_port: 43000,
            payload: b"CAN".to_vec()
        }
    );
}

#[test]
fn address_plan_rejects_ambiguous_or_non_unicast_links() {
    let base = NetworkConfig::default();
    assert!(base.validate().is_ok());
    for config in [
        NetworkConfig {
            peer: base.local,
            ..base.clone()
        },
        NetworkConfig {
            prefix_length: 0,
            ..base.clone()
        },
        NetworkConfig {
            prefix_length: 32,
            ..base.clone()
        },
        NetworkConfig {
            port: 0,
            ..base.clone()
        },
        NetworkConfig {
            peer: Ipv4Addr::new(198, 18, 0, 4),
            ..base.clone()
        },
        NetworkConfig {
            local: Ipv4Addr::new(198, 18, 0, 0),
            ..base.clone()
        },
        NetworkConfig {
            peer: Ipv4Addr::new(198, 18, 0, 3),
            ..base.clone()
        },
    ] {
        assert!(config.validate().is_err(), "{config:?}");
    }
}

#[test]
fn icmp_echo_reply_swaps_addresses_and_preserves_identifier_and_payload() {
    let config = NetworkConfig::default();
    let mut packet = vec![0u8; 33];
    packet[0] = 0x45;
    packet[2..4].copy_from_slice(&33u16.to_be_bytes());
    packet[8] = 64;
    packet[9] = 1;
    packet[12..16].copy_from_slice(&config.local.octets());
    packet[16..20].copy_from_slice(&config.peer.octets());
    packet[20] = 8;
    packet[24..28].copy_from_slice(&[0x12, 0x34, 0, 7]);
    packet[28..].copy_from_slice(b"hello");
    let check = ip::checksum(&packet[20..]);
    packet[22..24].copy_from_slice(&check.to_be_bytes());
    repair_header(&mut packet);
    let Incoming::EchoReply(reply) = ip::parse(&packet, &config).unwrap() else {
        panic!("Not an echo reply")
    };
    assert_eq!(reply[12..16], config.peer.octets());
    assert_eq!(reply[16..20], config.local.octets());
    assert_eq!(reply[20], 0);
    assert_eq!(&reply[24..], &packet[24..]);
    assert_eq!(ip::checksum(&reply[..20]), 0);
    assert_eq!(ip::checksum(&reply[20..]), 0);
}
