use std::cmp::min;

use super::constants::TARGET_SAMPLE_POINT;

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct GsDeviceBitTiming {
    pub(crate) prop_seg: u32,
    pub(crate) phase_seg1: u32,
    pub(crate) phase_seg2: u32,
    pub(crate) sjw: u32,
    pub(crate) brp: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct GsBtConst {
    pub feature: u32, // LE
    fclk_can: u32,    // Hz
    tseg1_min: u32,
    tseg1_max: u32,
    tseg2_min: u32,
    tseg2_max: u32,
    sjw_max: u32,
    brp_min: u32,
    brp_max: u32,
    brp_inc: u32,
}

pub fn parse_bt_const(b: &[u8]) -> GsBtConst {
    let le32 = |i| u32::from_le_bytes(b[i..i + 4].try_into().unwrap());
    GsBtConst {
        feature: le32(0),
        fclk_can: le32(4),
        tseg1_min: le32(8),
        tseg1_max: le32(12),
        tseg2_min: le32(16),
        tseg2_max: le32(20),
        sjw_max: le32(24),
        brp_min: le32(28),
        brp_max: le32(32),
        brp_inc: le32(36),
    }
}

impl GsDeviceBitTiming {
    pub(crate) fn to_bytes(self) -> [u8; 20] {
        let mut buf = [0u8; 20];
        buf[0..4].copy_from_slice(&self.prop_seg.to_le_bytes());
        buf[4..8].copy_from_slice(&self.phase_seg1.to_le_bytes());
        buf[8..12].copy_from_slice(&self.phase_seg2.to_le_bytes());
        buf[12..16].copy_from_slice(&self.sjw.to_le_bytes());
        buf[16..20].copy_from_slice(&self.brp.to_le_bytes());
        buf
    }
}

pub(crate) fn encode_mode(mode: u32, flags: u32) -> [u8; 8] {
    let mut buf = [0u8; 8];
    buf[0..4].copy_from_slice(&mode.to_le_bytes());
    buf[4..8].copy_from_slice(&flags.to_le_bytes());
    buf
}

pub(crate) fn calc_bit_timing(bitrate: u32, caps: &GsBtConst) -> Option<GsDeviceBitTiming> {
    let fclk = caps.fclk_can;
    let tseg1_min = caps.tseg1_min;
    let tseg1_max = caps.tseg1_max;
    let tseg2_min = caps.tseg2_min;
    let tseg2_max = caps.tseg2_max;
    let sjw_max = caps.sjw_max;
    let brp_min = caps.brp_min;
    let brp_max = caps.brp_max;
    let brp_inc = caps.brp_inc.max(1); // safety

    log::info!(
        "BT_CONST: fclk_can={}Hz tseg1=[{}..{}] tseg2=[{}..{}] sjw_max={} brp=[{}..{}] inc={}",
        fclk,
        tseg1_min,
        tseg1_max,
        tseg2_min,
        tseg2_max,
        sjw_max,
        brp_min,
        brp_max,
        brp_inc
    );

    let mut best: Option<(GsDeviceBitTiming, f64)> = None;

    for brp in brp_min..=brp_max {
        for tseg1 in tseg1_min..=tseg1_max {
            for tseg2 in tseg2_min..=tseg2_max {
                let total_tq = 1 + tseg1 + tseg2;
                let actual_bitrate = fclk as f64 / (brp as f64 * total_tq as f64);
                let rate_error = (actual_bitrate - bitrate as f64).abs() / bitrate as f64;
                if rate_error > 0.05 {
                    continue;
                }

                let sample_point = (1 + tseg1) as f64 / total_tq as f64;
                let sample_error = (sample_point - TARGET_SAMPLE_POINT).abs();
                let score = rate_error * 10.0 + sample_error;

                let mut phase_seg1 = if tseg1 > 1 {
                    min(tseg1 / 2, tseg1_max)
                } else {
                    1
                };
                if phase_seg1 == 0 {
                    phase_seg1 = 1;
                }
                let mut prop_seg = tseg1.saturating_sub(phase_seg1);
                if prop_seg == 0 {
                    if phase_seg1 > 1 {
                        phase_seg1 -= 1;
                        prop_seg = 1;
                    } else {
                        continue;
                    }
                }
                let phase_seg2 = tseg2;
                let sjw = min(sjw_max, phase_seg2);

                let candidate = GsDeviceBitTiming {
                    prop_seg,
                    phase_seg1,
                    phase_seg2,
                    sjw,
                    brp,
                };

                match &best {
                    Some((_, best_score)) if *best_score <= score => {}
                    _ => best = Some((candidate, score)),
                }
            }
        }
    }

    best.map(|(cfg, _)| cfg)
}
