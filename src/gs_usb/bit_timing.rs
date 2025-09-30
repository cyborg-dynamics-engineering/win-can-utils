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

pub(crate) fn calc_bit_timing(bitrate: u32) -> Option<GsDeviceBitTiming> {
    const FCLK: u32 = 48_000_000;
    const TSEG1_MIN: u32 = 1;
    const TSEG1_MAX: u32 = 16;
    const TSEG2_MIN: u32 = 1;
    const TSEG2_MAX: u32 = 8;
    const SJW_MAX: u32 = 4;
    const BRP_MIN: u32 = 1;
    const BRP_MAX: u32 = 1024;

    let mut best: Option<(GsDeviceBitTiming, f64)> = None;

    for brp in BRP_MIN..=BRP_MAX {
        for tseg1 in TSEG1_MIN..=TSEG1_MAX {
            for tseg2 in TSEG2_MIN..=TSEG2_MAX {
                let total_tq = 1 + tseg1 + tseg2;
                let actual_bitrate = FCLK as f64 / (brp as f64 * total_tq as f64);
                let rate_error = (actual_bitrate - bitrate as f64).abs() / bitrate as f64;
                if rate_error > 0.05 {
                    continue;
                }

                let sample_point = (1 + tseg1) as f64 / total_tq as f64;
                let sample_error = (sample_point - TARGET_SAMPLE_POINT).abs();
                let score = rate_error * 10.0 + sample_error;

                let mut phase_seg1 = if tseg1 > 1 {
                    min(tseg1 / 2, TSEG1_MAX)
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
                let sjw = min(SJW_MAX, phase_seg2);

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
