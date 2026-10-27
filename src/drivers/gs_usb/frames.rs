use crosscan::can::CanFrame;

use super::constants::GS_HEADER_LEN;

#[inline]
fn dlc_to_len(dlc: u8) -> usize {
    match dlc {
        0..=8 => dlc as usize,
        9 => 12,
        10 => 16,
        11 => 20,
        12 => 24,
        13 => 32,
        14 => 48,
        15 => 64,
        _ => 0,
    }
}

#[inline]
fn align_up(x: usize, m: usize) -> usize {
    if m == 0 { x } else { (x + (m - 1)) / m * m }
}

fn plausible_header(bytes: &[u8], expected_chan: u8) -> bool {
    if bytes.len() < GS_HEADER_LEN {
        return false;
    }
    let dlc = bytes[8];
    let chan = bytes[9];
    // valid DLC range
    if dlc > 15 {
        return false;
    }
    // channel sanity
    if chan != expected_chan {
        return false;
    }
    true
}

pub(crate) fn parse_host_frame_at(
    bytes: &[u8],
    channel_index: u8,
    timestamp_enabled: bool,
    last_ts64: &mut Option<u64>,
    out_wmax: usize,
    pad_pkts_enabled: bool,
) -> Option<(Option<CanFrame>, usize)> {
    use super::constants::*;
    use log::{debug, trace, warn};

    if bytes.len() < GS_HEADER_LEN {
        trace!(
            "rx: not enough for header (have={}, need={})",
            bytes.len(),
            GS_HEADER_LEN
        );
        return None;
    }

    let echo_id = u32::from_le_bytes(bytes[0..4].try_into().ok()?);
    let raw_id = u32::from_le_bytes(bytes[4..8].try_into().ok()?);
    let dlc = bytes[8];
    let chan = bytes[9];

    let is_fd = bytes[10] & GS_CAN_FLAG_FD != 0;
    let data_len = if is_fd {
        dlc_to_len(dlc)
    } else {
        usize::from(dlc.min(8))
    };
    debug!(
        "rx: hdr echo_id=0x{:08x} raw_id=0x{:08x} dlc={} → data_len={} chan={} expected_chan={}",
        echo_id, raw_id, dlc, data_len, chan, channel_index
    );

    if dlc > 15 {
        warn!("rx: invalid dlc={} (raw_id=0x{:08x}), drop 1", dlc, raw_id);
        return Some((None, 1));
    }

    // gs_usb records reserve 8 classic or 64 FD payload bytes regardless of
    // DLC. The timestamp follows that entire field, including its unused bytes.
    let base = GS_HEADER_LEN + if is_fd { GS_MAX_DATA } else { 8 };
    let ts_off = base;

    // If timestamp is expected, require +4 bytes
    let mut consumed = base;
    if bytes.len() < base {
        return None;
    }
    if timestamp_enabled {
        if bytes.len() < base + 4 {
            trace!(
                "rx: waiting for timestamp (have={}, need={})",
                bytes.len(),
                base + 4
            );
            return None;
        }
        consumed = base + 4;
        debug!(
            "rx: timestamp expected, base={} → consumed={}",
            base, consumed
        );
    }

    // If PAD_PKTS is enabled, the device pads to wMaxPacketSize.
    // Always align to out_wmax. If not enough bytes, wait.
    if pad_pkts_enabled && out_wmax > 0 {
        let padded = align_up(consumed, out_wmax);
        if bytes.len() < padded {
            trace!("rx: waiting for pad to {} (have={})", padded, bytes.len());
            return None;
        }
        if padded != consumed {
            debug!("rx: apply PAD_PKTS alignment {} → {}", consumed, padded);
            consumed = padded;
        }
    }

    if !plausible_header(bytes, channel_index) {
        log::trace!("rx: implausible header → drop 1 byte and resync");
        return Some((None, 1));
    }

    // Skip TX echoes or wrong channel
    if echo_id != GS_CAN_ECHO_ID_UNUSED || chan != channel_index {
        debug!(
            "rx: skipping frame (echo_id=0x{:08x} chan={} expected={}) total_len={}",
            echo_id, chan, channel_index, consumed
        );
        return Some((None, consumed));
    }

    // Crosscan currently represents at most 8 data bytes. Retire the complete
    // FD record rather than leaving its payload/timestamp in the stream.
    if data_len > 8 {
        warn!("rx: CAN FD payload exceeds crosscan's 8-byte frame capacity");
        return Some((None, consumed));
    }

    // Build frame from real data_len (not aligned)
    let data_off = GS_HEADER_LEN;
    let data = &bytes[data_off..data_off + data_len];

    let mut frame = if (raw_id & CAN_ERR_FLAG) != 0 {
        CanFrame::new_error(raw_id & CAN_ERR_MASK).ok()?
    } else if (raw_id & CAN_RTR_FLAG) != 0 {
        CanFrame::new_remote(
            raw_id
                & if (raw_id & CAN_EFF_FLAG) != 0 {
                    CAN_EFF_MASK
                } else {
                    CAN_SFF_MASK
                },
            dlc.min(8) as usize,
            (raw_id & CAN_EFF_FLAG) != 0,
        )
        .ok()?
    } else if (raw_id & CAN_EFF_FLAG) != 0 {
        CanFrame::new_eff(raw_id & CAN_EFF_MASK, data).ok()?
    } else {
        CanFrame::new(raw_id & CAN_SFF_MASK, data).ok()?
    };

    // Hardware SOF captures can arrive slightly out of order relative to error
    // observations. Only a jump larger than half the counter range is a wrap;
    // a small backward step must not add 71 minutes to every following frame.
    if timestamp_enabled {
        let ts32 = u32::from_le_bytes(bytes[ts_off..ts_off + 4].try_into().unwrap());
        let ts64 = match *last_ts64 {
            Some(prev) => {
                let prev32 = (prev & 0xffff_ffff) as u32;
                let mut base64 = prev & !0xffff_ffff;
                if ts32 < prev32 && prev32 - ts32 > 0x8000_0000 {
                    base64 += 1u64 << 32;
                } else if ts32 > prev32 && ts32 - prev32 > 0x8000_0000 && base64 != 0 {
                    base64 -= 1u64 << 32;
                }
                base64 | ts32 as u64
            }
            None => ts32 as u64,
        };
        *last_ts64 = Some(last_ts64.unwrap_or(ts64).max(ts64));
        frame.set_timestamp(Some(ts64));
        debug!("rx: ts32=0x{:08x} → ts64={}", ts32, ts64);
    }

    debug!(
        "rx: accepted frame id=0x{:08x} dlc={} len={} pad_pkts={} out_wmax={} consumed={}",
        raw_id, dlc, data_len, pad_pkts_enabled, out_wmax, consumed
    );
    Some((Some(frame), consumed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classic_raw_dlc_15_timestamp_stays_at_offset_20() {
        let mut bytes = [0; 24];
        bytes[..4].copy_from_slice(&u32::MAX.to_le_bytes());
        bytes[8] = 15;
        bytes[12..20].copy_from_slice(&[1; 8]);
        bytes[20..24].copy_from_slice(&123u32.to_le_bytes());
        let (frame, consumed) = parse_host_frame_at(&bytes, 0, true, &mut None, 64, false).unwrap();
        assert_eq!(consumed, 24);
        let frame = frame.unwrap();
        assert_eq!(frame.data(), &[1; 8]);
        assert_eq!(frame.timestamp(), Some(123));
    }

    #[test]
    fn fd_echo_with_small_dlc_consumes_full_timestamped_record() {
        let mut bytes = [0; 80];
        bytes[8] = 1;
        bytes[10] = super::super::constants::GS_CAN_FLAG_FD;
        bytes[76..80].copy_from_slice(&456u32.to_le_bytes());
        for split in 0..80 {
            assert!(parse_host_frame_at(&bytes[..split], 0, true, &mut None, 64, false).is_none());
        }
        assert_eq!(
            parse_host_frame_at(&bytes, 0, true, &mut None, 64, false),
            Some((None, 80))
        );
    }

    #[test]
    fn timestamp_unwrap_distinguishes_reordering_from_counter_wrap() {
        let mut bytes = [0; 24];
        bytes[..4].copy_from_slice(&u32::MAX.to_le_bytes());
        let mut reference = None;
        for (raw, expected) in [
            (0xffff_fff0u32, 0xffff_fff0u64),
            (0xffff_ffe0, 0xffff_ffe0), // delayed SOF, same epoch
            (0x10, 0x1_0000_0010),
            (0xffff_fff8, 0xffff_fff8), // delayed frame from previous epoch
            (0x20, 0x1_0000_0020),
        ] {
            bytes[20..24].copy_from_slice(&raw.to_le_bytes());
            let (frame, _) =
                parse_host_frame_at(&bytes, 0, true, &mut reference, 64, false).unwrap();
            assert_eq!(frame.unwrap().timestamp(), Some(expected));
        }
        assert_eq!(reference, Some(0x1_0000_0020));
    }

    #[test]
    fn split_padded_classic_record_on_fd_adapter_has_one_frame_and_timestamp() {
        let mut bytes = vec![0; 128];
        bytes[..4].copy_from_slice(&u32::MAX.to_le_bytes());
        bytes[4..8].copy_from_slice(&0x123u32.to_le_bytes());
        bytes[8] = 8;
        bytes[12..20].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        bytes[20..24].copy_from_slice(&42u32.to_le_bytes());
        let mut timestamp = None;
        for split in 0..128 {
            assert!(
                parse_host_frame_at(&bytes[..split], 0, true, &mut timestamp, 128, true).is_none()
            );
        }
        let (frame, consumed) =
            parse_host_frame_at(&bytes, 0, true, &mut timestamp, 128, true).unwrap();
        let frame = frame.unwrap();
        assert_eq!(consumed, 128);
        assert_eq!(frame.id(), 0x123);
        assert_eq!(frame.data(), &[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(frame.timestamp(), Some(42));
    }

    #[test]
    fn incomplete_frame_without_timestamp_waits_for_payload() {
        let mut bytes = [0; 20];
        bytes[..4].copy_from_slice(&u32::MAX.to_le_bytes());
        bytes[8] = 8;
        for split in 0..20 {
            assert!(parse_host_frame_at(&bytes[..split], 0, false, &mut None, 0, false).is_none());
        }
        assert!(
            parse_host_frame_at(&bytes, 0, false, &mut None, 0, false)
                .unwrap()
                .0
                .is_some()
        );
    }
}
