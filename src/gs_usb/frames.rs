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
    _timestamp_enabled: bool,
    _last_ts64: &mut Option<u64>,
) -> Option<(Option<CanFrame>, usize)> {
    if bytes.len() < GS_HEADER_LEN {
        return None;
    }

    if !plausible_header(bytes, channel_index) {
        return Some((None, 1));
    }

    let echo_id = u32::from_le_bytes(bytes[0..4].try_into().ok()?);
    let raw_id = u32::from_le_bytes(bytes[4..8].try_into().ok()?);
    let dlc = bytes[8];
    let chan = bytes[9];
    let data_len = dlc_to_len(dlc);

    let total_len = GS_HEADER_LEN + data_len;
    if total_len > GS_MAX_FRAME_LEN {
        return Some((None, 1));
    }
    if bytes.len() < total_len {
        return None;
    }

    let data_off = GS_HEADER_LEN;
    let data = &bytes[data_off..data_off + data_len];

    if echo_id != GS_CAN_ECHO_ID_UNUSED || chan != channel_index {
        return Some((None, total_len));
    }

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
