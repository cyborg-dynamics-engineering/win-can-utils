use async_trait::async_trait;
use crosscan::can::CanFrame;
use tokio::sync::Mutex;

use libloading::Library;
use peak_can_sys::*;
use std::sync::OnceLock;

use crate::drivers::CanDriver;

type CanInitializeFn = unsafe extern "system" fn(WORD, WORD, BYTE, DWORD, WORD) -> DWORD;
type CanUninitializeFn = unsafe extern "system" fn(WORD) -> DWORD;
type CanWriteFn = unsafe extern "system" fn(WORD, *mut CANTPMsg) -> DWORD;
type CanReadFn = unsafe extern "system" fn(WORD, *mut CANTPMsg, *mut CANTPTimestamp) -> DWORD;
type CanGetStatusFn = unsafe extern "system" fn(WORD) -> DWORD;

struct PcanApi {
    can_initialize: CanInitializeFn,
    can_uninitialize: CanUninitializeFn,
    can_write: CanWriteFn,
    can_read: CanReadFn,
    can_get_status: CanGetStatusFn,
}

static PCAN_API: OnceLock<Result<PcanApi, String>> = OnceLock::new();

fn load_pcan_api() -> Result<PcanApi, String> {
    unsafe {
        let lib = Library::new("PCANBasic.dll")
            .map_err(|e| format!("Failed to load PCANBasic.dll: {e}"))?;

        let can_initialize = *lib
            .get::<CanInitializeFn>(b"CAN_Initialize\0")
            .map_err(|e| format!("Failed to load CAN_Initialize: {e}"))?;
        let can_uninitialize = *lib
            .get::<CanUninitializeFn>(b"CAN_Uninitialize\0")
            .map_err(|e| format!("Failed to load CAN_Uninitialize: {e}"))?;
        let can_write = *lib
            .get::<CanWriteFn>(b"CAN_Write\0")
            .map_err(|e| format!("Failed to load CAN_Write: {e}"))?;
        let can_read = *lib
            .get::<CanReadFn>(b"CAN_Read\0")
            .map_err(|e| format!("Failed to load CAN_Read: {e}"))?;
        let can_get_status = *lib
            .get::<CanGetStatusFn>(b"CAN_GetStatus\0")
            .map_err(|e| format!("Failed to load CAN_GetStatus: {e}"))?;

        // Leak the library handle so the loaded symbols remain valid for the
        // remainder of the process. This avoids lifetime issues with the
        // dynamically loaded module while still allowing the rest of the
        // program to run when the DLL is absent.
        std::mem::forget(lib);

        Ok(PcanApi {
            can_initialize,
            can_uninitialize,
            can_write,
            can_read,
            can_get_status,
        })
    }
}

fn pcan_api() -> std::io::Result<&'static PcanApi> {
    match PCAN_API.get_or_init(load_pcan_api) {
        Ok(api) => Ok(api),
        Err(err) => Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            err.clone(),
        )),
    }
}

/// PCAN-Basic driver backed by `PCANBasic.dll` (libpcanbasic).
pub struct PcanDriver {
    channel: WORD,
    configured_bitrate: Option<u32>,
    is_open: bool,
    // PCAN calls are synchronous; keep a mutex to serialize access like the SLCAN driver does.
    io_lock: Mutex<()>,
}

impl PcanDriver {
    /// Open by channel string (e.g., "USBBUS1", "PCIBUS1", "LANBUS1").
    pub async fn open(channel_name: &str) -> std::io::Result<Self> {
        // Validate that the PCAN runtime is available before reporting success.
        pcan_api()?;

        let channel = parse_channel(channel_name).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "Unknown PCAN channel")
        })?;
        Ok(Self {
            channel,
            configured_bitrate: None,
            is_open: false,
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

fn status_error(operation: &str, status: u32) -> std::io::Error {
    use std::io::ErrorKind;
    let transient = PEAK_ERROR_ANYBUSERR
        | PEAK_ERROR_XMTFULL
        | PEAK_ERROR_QXMTFULL
        | PEAK_ERROR_OVERRUN
        | PEAK_ERROR_QOVERRUN;
    let kind = if status & !transient == 0 {
        ErrorKind::WouldBlock
    } else if matches!(
        status,
        PEAK_ERROR_ILLHW
            | PEAK_ERROR_ILLNET
            | PEAK_ERROR_ILLCLIENT
            | PEAK_ERROR_NODRIVER
            | PEAK_ERROR_INITIALIZE
    ) {
        ErrorKind::NotConnected
    } else if status & (PEAK_ERROR_ILLPARAMVAL | PEAK_ERROR_ILLDATA) != 0 {
        ErrorKind::InvalidInput
    } else {
        ErrorKind::Other
    };
    std::io::Error::new(kind, format!("{operation} failed: 0x{status:08X}"))
}

#[async_trait]
impl CanDriver for PcanDriver {
    fn capabilities(&self) -> crosscan::protocol::Capabilities {
        crosscan::protocol::Capabilities {
            hardware_can_fd: None,
            can_fd: false,
            hardware_timestamps: Some(true),
            bus_state: true,
            software_tx_echo: true,
        }
    }

    async fn health(&mut self) -> std::io::Result<crate::drivers::can_driver::DriverHealth> {
        use crosscan::protocol::BusState;
        let status = unsafe { (pcan_api()?.can_get_status)(self.channel) };
        let known = PEAK_ERROR_BUSOFF
            | PEAK_ERROR_BUSPASSIVE
            | PEAK_ERROR_BUSHEAVY
            | PEAK_ERROR_BUSLIGHT
            | PEAK_ERROR_QOVERRUN
            | PEAK_ERROR_OVERRUN
            | PEAK_ERROR_QXMTFULL
            | PEAK_ERROR_XMTFULL;
        if status & !known != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                format!("CAN_GetStatus failed: 0x{status:08X}"),
            ));
        }
        Ok(crate::drivers::can_driver::DriverHealth {
            bus_state: if status & PEAK_ERROR_BUSOFF != 0 {
                BusState::BusOff
            } else if status & PEAK_ERROR_BUSPASSIVE != 0 {
                BusState::ErrorPassive
            } else if status & (PEAK_ERROR_BUSHEAVY | PEAK_ERROR_BUSLIGHT) != 0 {
                BusState::ErrorWarning
            } else {
                BusState::ErrorActive
            },
            ..Default::default()
        })
    }
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
        let api = pcan_api()?;
        let btr_const = self
            .configured_bitrate
            .and_then(map_bitrate_to_const)
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::Other, "Bitrate not set"))?;

        // For plug-and-play hardware (USB/PCI/LAN), HwType/IOPort/Interrupt are zero.
        let status = unsafe { (api.can_initialize)(self.channel, btr_const, 0u8, 0u32, 0u16) };
        if status != PEAK_ERROR_OK {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("CAN_Initialize failed: 0x{:08X}", status),
            ));
        }
        self.is_open = true;
        Ok(())
    }

    async fn send_frame(&mut self, frame: &CanFrame) -> std::io::Result<()> {
        let _g = self.io_lock.lock().await;
        let api = pcan_api()?;

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
        if frame.is_rtr() {
            msg.MSGTYPE |= PEAK_MESSAGE_RTR as u8;
        }
        let data = frame.data();
        let copy_len = data.len().min(8);
        msg.DATA[..copy_len].copy_from_slice(&data[..copy_len]);

        let mut msg_alias: CANTPMsg = msg; // function takes alias pointer
        let status = unsafe { (api.can_write)(self.channel, &mut msg_alias as *mut CANTPMsg) };
        if status != PEAK_ERROR_OK {
            return Err(status_error("CAN_Write", status));
        }
        Ok(())
    }

    async fn read_frames(&mut self) -> std::io::Result<Vec<CanFrame>> {
        let _g = self.io_lock.lock().await;
        let api = pcan_api()?;
        let mut frames = Vec::new();

        for _ in 0..1024 {
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
                (api.can_read)(
                    self.channel,
                    &mut msg as *mut CANTPMsg,
                    &mut ts as *mut CANTPTimestamp,
                )
            };
            if status == PEAK_ERROR_QRCVEMPTY {
                break; // no more frames in RX queue
            }
            if status != PEAK_ERROR_OK {
                // Bus state and overruns are reported by health(), not transport loss.
                if status
                    & !(PEAK_ERROR_BUSOFF
                        | PEAK_ERROR_BUSPASSIVE
                        | PEAK_ERROR_BUSHEAVY
                        | PEAK_ERROR_BUSLIGHT
                        | PEAK_ERROR_QOVERRUN
                        | PEAK_ERROR_OVERRUN
                        | PEAK_ERROR_QRCVEMPTY)
                    == 0
                {
                    break;
                }
                return Err(status_error("CAN_Read", status));
            }

            if msg.MSGTYPE & PEAK_MESSAGE_STATUS as u8 != 0 {
                continue;
            }

            // Extended frame?
            let extended = (msg.MSGTYPE & PEAK_MESSAGE_EXTENDED as u8) != 0;

            let dlc = (msg.LEN as usize).min(8);
            let data = &msg.DATA[..dlc];

            let mut frame = if msg.MSGTYPE & PEAK_MESSAGE_RTR as u8 != 0 {
                CanFrame::new_remote(msg.ID, dlc, extended)
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?
            } else if extended {
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
        if !self.is_open {
            return Ok(());
        }
        let _g = self.io_lock.lock().await;
        let api = pcan_api()?;
        let status = unsafe { (api.can_uninitialize)(self.channel) };
        if status != PEAK_ERROR_OK {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("CAN_Uninitialize failed: 0x{:08X}", status),
            ));
        }
        self.is_open = false;
        Ok(())
    }

    async fn get_bitrate(&self) -> Option<u32> {
        self.configured_bitrate
    }
}

impl Drop for PcanDriver {
    fn drop(&mut self) {
        if self.is_open {
            if let Ok(api) = pcan_api() {
                unsafe {
                    (api.can_uninitialize)(self.channel);
                }
            }
        }
    }
}
