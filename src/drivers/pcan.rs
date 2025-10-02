use async_trait::async_trait;
use crosscan::can::CanFrame;
use tokio::sync::Mutex;

use peak_can_sys::*;

use crate::drivers::CanDriver;

/// PCAN-Basic driver backed by `peak-can-sys` (PCANBasic.dll / libpcanbasic).
pub struct PcanDriver {
    channel: WORD,
    configured_bitrate: Option<u32>,
    // PCAN calls are synchronous; keep a mutex to serialize access like the SLCAN driver does.
    io_lock: Mutex<()>,
}

impl PcanDriver {
    /// Open by channel string (e.g., "USBBUS1", "PCIBUS1", "LANBUS1").
    pub async fn open(channel_name: &str) -> std::io::Result<Self> {
        let channel = parse_channel(channel_name).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "Unknown PCAN channel")
        })?;
        Ok(Self {
            channel,
            configured_bitrate: None,
            io_lock: Mutex::new(()),
        })
    }
}

fn parse_channel(s: &str) -> Option<WORD> {
    let mut t = s.trim().to_ascii_uppercase();
    if let Some(rest) = t.strip_prefix("PCAN_") {
        t = rest.to_string();
    }
    let take_idx = |prefix: &str| -> Option<usize> {
        t.strip_prefix(prefix).and_then(|n| n.parse::<usize>().ok())
    };

    if let Some(i) = take_idx("USBBUS") {
        if (1..=16).contains(&i) {
            // Constants are PEAK_USBBUS1..PEAK_USBBUS16
            let base = PEAK_USBBUS1 as u16;
            return Some((base + (i as u16 - 1)) as WORD);
        }
    }
    if let Some(i) = take_idx("PCIBUS") {
        if (1..=16).contains(&i) {
            let base = PEAK_PCIBUS1 as u16;
            return Some((base + (i as u16 - 1)) as WORD);
        }
    }
    if let Some(i) = take_idx("LANBUS") {
        if (1..=16).contains(&i) {
            let base = PEAK_LANBUS1 as u16;
            return Some((base + (i as u16 - 1)) as WORD);
        }
    }
    None
}

fn map_bitrate_to_const(bps: u32) -> Option<WORD> {
    Some(match bps {
        5_000 => PEAK_BAUD_5K,
        10_000 => PEAK_BAUD_10K,
        20_000 => PEAK_BAUD_20K,
        33_333 => PEAK_BAUD_33K,
        47_619 => PEAK_BAUD_47K,
        50_000 => PEAK_BAUD_50K,
        83_333 => PEAK_BAUD_83K,
        95_238 => PEAK_BAUD_95K,
        100_000 => PEAK_BAUD_100K,
        125_000 => PEAK_BAUD_125K,
        250_000 => PEAK_BAUD_250K,
        500_000 => PEAK_BAUD_500K,
        800_000 => PEAK_BAUD_800K,
        1_000_000 => PEAK_BAUD_1M,
        _ => return None,
    } as WORD)
}

#[async_trait]
impl CanDriver for PcanDriver {
    async fn enable_timestamp(&mut self) -> std::io::Result<()> {
        // PCAN-Basic always provides timestamps via CAN_Read’s third parameter; no switch needed.
        Ok(())
    }

    async fn set_bitrate(&mut self, bitrate: u32) -> std::io::Result<()> {
        let _btr_const = map_bitrate_to_const(bitrate).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("Unsupported CAN bitrate: {}", bitrate),
            )
        })?;
        self.configured_bitrate = Some(bitrate);

        // Defer actual hardware init to open_channel(), same as the SLCAN pattern.
        // We just remember the requested bitrate here; CAN_Initialize uses it later.
        Ok(())
    }

    async fn open_channel(&mut self) -> std::io::Result<()> {
        let _g = self.io_lock.lock().await;
        let btr_const = self
            .configured_bitrate
            .and_then(map_bitrate_to_const)
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::Other, "Bitrate not set"))?;

        // For plug-and-play hardware (USB/PCI/LAN), HwType/IOPort/Interrupt are zero.
        let status = unsafe { CAN_Initialize(self.channel, btr_const, 0u8, 0u32, 0u16) };
        if status != PEAK_ERROR_OK {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("CAN_Initialize failed: 0x{:08X}", status),
            ));
        }
        Ok(())
    }

    async fn send_frame(&mut self, frame: &CanFrame) -> std::io::Result<()> {
        let _g = self.io_lock.lock().await;

        // Build CANTPMsg (8-byte classic CAN).
        let mut msg = tagCANTPMsg {
            ID: frame.id(),
            MSGTYPE: if frame.is_extended() {
                PEAK_MESSAGE_EXTENDED as u8
            } else {
                PEAK_MESSAGE_STANDARD as u8
            },
            LEN: frame.dlc() as u8,
            DATA: [0u8; 8],
        };
        let data = frame.data();
        let copy_len = data.len().min(8);
        msg.DATA[..copy_len].copy_from_slice(&data[..copy_len]);

        let mut msg_alias: CANTPMsg = msg; // function takes alias pointer
        let status = unsafe { CAN_Write(self.channel, &mut msg_alias as *mut CANTPMsg) };
        if status != PEAK_ERROR_OK {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("CAN_Write failed: 0x{:08X}", status),
            ));
        }
        Ok(())
    }

    async fn read_frames(&mut self) -> std::io::Result<Vec<CanFrame>> {
        let _g = self.io_lock.lock().await;
        let mut frames = Vec::new();

        loop {
            // Prepare output buffers for CAN_Read
            let mut msg: CANTPMsg = tagCANTPMsg {
                ID: 0,
                MSGTYPE: 0,
                LEN: 0,
                DATA: [0u8; 8],
            };
            let mut ts: CANTPTimestamp = tagCANTPTimestamp {
                millis: 0,
                millis_overflow: 0,
                micros: 0,
            };

            let status = unsafe {
                CAN_Read(
                    self.channel,
                    &mut msg as *mut CANTPMsg,
                    &mut ts as *mut CANTPTimestamp,
                )
            };
            if status == PEAK_ERROR_QRCVEMPTY {
                break; // no more frames in RX queue
            }
            if status != PEAK_ERROR_OK {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("CAN_Read failed: 0x{:08X}", status),
                ));
            }

            // Extended frame?
            let extended = (msg.MSGTYPE & PEAK_MESSAGE_EXTENDED as u8) != 0;

            let dlc = (msg.LEN as usize).min(8);
            let data = &msg.DATA[..dlc];

            let mut frame = if extended {
                CanFrame::new_eff(msg.ID, data)
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?
            } else {
                CanFrame::new(msg.ID, data)
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?
            };

            // Timestamp: ((millis_overflow << 32) | millis) * 1000 + micros
            let ts_us = (((ts.millis_overflow as u64) << 32) | (ts.millis as u64)) * 1000
                + (ts.micros as u64);
            frame.set_timestamp(Some(ts_us));

            frames.push(frame);
        }

        Ok(frames)
    }

    async fn close_channel(&mut self) -> std::io::Result<()> {
        let _g = self.io_lock.lock().await;
        let status = unsafe { CAN_Uninitialize(self.channel) };
        if status != PEAK_ERROR_OK {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("CAN_Uninitialize failed: 0x{:08X}", status),
            ));
        }
        Ok(())
    }

    async fn get_bitrate(&self) -> Option<u32> {
        self.configured_bitrate
    }
}
