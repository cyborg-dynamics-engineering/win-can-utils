use crosscan::can::CanFrame;

use super::constants::{
    CAN_EFF_FLAG, CAN_EFF_MASK, CAN_ERR_FLAG, CAN_ERR_MASK, CAN_RTR_FLAG, CAN_SFF_MASK,
    GS_CAN_ECHO_ID_UNUSED, GS_HEADER_LEN, GS_MAX_DATA, GS_MAX_FRAME_LEN,
};

#[inline]
pub(crate) fn dlc_to_len(dlc: u8) -> usize {
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
pub(crate) fn plausible_header(hdr: &[u8], expected_chan: u8) -> bool {
    if hdr.len() < GS_HEADER_LEN {
        return false;
    }

    let dlc = hdr[8];
    let chan = hdr[9];
    let len = dlc_to_len(dlc);

    len <= GS_MAX_DATA && chan == expected_chan
}

pub(crate) fn parse_host_frame_at(
    bytes: &[u8],
    channel_index: u8,
    timestamp_enabled: bool,
    _last_ts64: &mut Option<u64>,
) -> Option<(Option<CanFrame>, usize)> {
    if bytes.len() < GS_HEADER_LEN {
        return None;
    }

    if !plausible_header(bytes, channel_index) {
        // resync by 1 byte (same as your working version)
        return Some((None, 1));
    }

    let echo_id = u32::from_le_bytes(bytes[0..4].try_into().ok()?);
    let raw_id = u32::from_le_bytes(bytes[4..8].try_into().ok()?);
    let dlc = bytes[8];
    let chan = bytes[9];
    let data_len = dlc_to_len(dlc);

    // Base: header + payload (no alignment by default, like your “good” version)
    let mut total_len = GS_HEADER_LEN + data_len;

    // --- DLC=0 padding quirks ---
    // Some fw pads zero-length data to 20 bytes even when timestamps are OFF:
    //   12-byte header + 8 bytes of zeros.
    // With timestamps ON, older fw does:
    //   12-byte header + 8 pad + 4-byte ts = 24 bytes.
    if data_len == 0 {
        if timestamp_enabled {
            // If ts is ON and we have enough bytes, consume the padded+ts frame as 24
            if bytes.len() >= 24 {
                total_len = 24;
            } else {
                return None; // wait for full 24 bytes
            }
        } else {
            // If ts is OFF, prefer the padded 20 if it looks like padding
            if bytes.len() >= 20 && bytes[12..20].iter().all(|&b| b == 0) {
                total_len = 20;
            }
            // else leave total_len as 12 (un-padded case)
        }
    }

    if total_len > GS_MAX_FRAME_LEN {
        return Some((None, 1));
    }
    if bytes.len() < total_len {
        return None;
    }

    // Echo or wrong channel → skip this whole frame
    if echo_id != GS_CAN_ECHO_ID_UNUSED || chan != channel_index {
        return Some((None, total_len));
    }

    // Build CAN frame
    let data_off = GS_HEADER_LEN;
    let data = &bytes[data_off..data_off + data_len];

    let frame = if (raw_id & CAN_ERR_FLAG) != 0 {
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

    Some((Some(frame), total_len))
}
