use crosscan::can::CanFrame;
use log::{debug, trace, warn};

use super::constants::{
    CAN_EFF_FLAG, CAN_EFF_MASK, CAN_ERR_FLAG, CAN_ERR_MASK, CAN_RTR_FLAG, CAN_SFF_MASK,
    GS_CAN_ECHO_ID_UNUSED, GS_HEADER_LEN, GS_MAX_DATA, GS_MAX_FRAME_LEN,
};

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
fn roundup8(x: usize) -> usize {
    (x + 7) & !7
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

    let data_len = dlc_to_len(dlc);
    debug!(
        "rx: hdr echo_id=0x{:08x} raw_id=0x{:08x} dlc={} → data_len={} chan={} expected_chan={}",
        echo_id, raw_id, dlc, data_len, chan, channel_index
    );

    if dlc > 15 {
        warn!("rx: invalid dlc={} (raw_id=0x{:08x}), drop 1", dlc, raw_id);
        return Some((None, 1));
    }

    // Aligned payload for Classic CAN: <=8 → 8; otherwise roundup8
    let aligned_payload = if data_len <= 8 { 8 } else { roundup8(data_len) };

    // Base size = header + aligned payload
    let mut base = GS_HEADER_LEN + aligned_payload;
    let mut ts_off = base;

    // DLC=0 quirk: many fw send 12 hdr + 8 pad; ts (if present) is at 20
    if data_len == 0 {
        if bytes.len() >= 20 {
            base = 20;
            ts_off = 20;
            debug!("rx: DLC=0 quirk → base=20 ts_off=20");
        } else {
            trace!("rx: DLC=0 waiting (have={}, need=20)", bytes.len());
            return None;
        }
    }

    // If timestamp is expected, require +4 bytes
    let mut consumed = base;
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

    // Timestamp (if present, may be zero on your device when only MODE flag is set)
    if timestamp_enabled {
        let ts32 = u32::from_le_bytes(bytes[ts_off..ts_off + 4].try_into().unwrap());
        let ts64 = match *last_ts64 {
            Some(prev) => {
                let prev32 = (prev & 0xffff_ffff) as u32;
                let mut base64 = prev & !0xffff_ffff;
                if ts32 < prev32 {
                    base64 += 1u64 << 32;
                }
                base64 | ts32 as u64
            }
            None => ts32 as u64,
        };
        *last_ts64 = Some(ts64);
        frame.set_timestamp(Some(ts64));
        debug!("rx: ts32=0x{:08x} → ts64={}", ts32, ts64);
    }

    debug!(
        "rx: accepted frame id=0x{:08x} dlc={} len={} pad_pkts={} out_wmax={} consumed={}",
        raw_id, dlc, data_len, pad_pkts_enabled, out_wmax, consumed
    );
    Some((Some(frame), consumed))
}
