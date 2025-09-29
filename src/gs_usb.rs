use async_trait::async_trait;
use crosscan::can::CanFrame;
use libusb_async::{
    self as usb, Device, DeviceDescriptor, DeviceHandle, Direction, Recipient, RequestType,
    TransferType,
};
use std::cmp::min;
use std::io;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use crate::can_driver::CanDriver;

const USB_TIMEOUT: Duration = Duration::from_millis(1);
// Read tuning: number of frames to attempt per bulk read, and a tiny drain timeout.
const READ_CHUNK_FRAMES: usize = 1024;
const DRAIN_READ_TIMEOUT: Duration = Duration::from_micros(10);

const HOST_FRAME_SIZE: usize = 4 + 4 + 1 + 1 + 1 + 1 + 4 + 64;

const GS_USB_BREQ_HOST_FORMAT: u8 = 0;
const GS_USB_BREQ_BITTIMING: u8 = 1;
const GS_USB_BREQ_MODE: u8 = 2;
const GS_USB_BREQ_TIMESTAMP: u8 = 3;
const GS_CAN_MODE_RESET: u32 = 0x0000_0000;
const GS_CAN_MODE_START: u32 = 0x0000_0001;

const GS_CAN_ECHO_ID_UNUSED: u32 = 0xFFFF_FFFF;

const CAN_EFF_FLAG: u32 = 0x8000_0000;
const CAN_RTR_FLAG: u32 = 0x4000_0000;
const CAN_ERR_FLAG: u32 = 0x2000_0000;
const CAN_EFF_MASK: u32 = 0x1FFF_FFFF;
const CAN_SFF_MASK: u32 = 0x0000_07FF;
const CAN_ERR_MASK: u32 = 0x1FFF_FFFF;

const TARGET_SAMPLE_POINT: f64 = 0.875;
const GS_USB_MAX_ECHO_SLOTS: u32 = 64;

/// Information about the bulk endpoints exposed by the gs_usb interface.
#[derive(Clone, Copy, Debug)]
struct InterfaceInfo {
    interface: u8,
    in_ep: u8,
    out_ep: u8,
    int_ep: Option<u8>,
}

fn map_usb_err(err: usb::UsbAsyncError) -> io::Error {
    usb::map_usb_err(err)
}

fn request_type_out() -> u8 {
    usb::request_type(Direction::Out, RequestType::Vendor, Recipient::Interface)
}

#[derive(Clone, Copy, Debug, Default)]
struct GsDeviceBitTiming {
    prop_seg: u32,
    phase_seg1: u32,
    phase_seg2: u32,
    sjw: u32,
    brp: u32,
}

impl GsDeviceBitTiming {
    fn to_bytes(self) -> [u8; 20] {
        let mut buf = [0u8; 20];
        buf[0..4].copy_from_slice(&self.prop_seg.to_le_bytes());
        buf[4..8].copy_from_slice(&self.phase_seg1.to_le_bytes());
        buf[8..12].copy_from_slice(&self.phase_seg2.to_le_bytes());
        buf[12..16].copy_from_slice(&self.sjw.to_le_bytes());
        buf[16..20].copy_from_slice(&self.brp.to_le_bytes());
        buf
    }
}

fn encode_mode(mode: u32, flags: u32) -> [u8; 8] {
    let mut buf = [0u8; 8];
    buf[0..4].copy_from_slice(&mode.to_le_bytes());
    buf[4..8].copy_from_slice(&flags.to_le_bytes());
    buf
}

fn host_config_bytes() -> [u8; 4] {
    0x0000_beefu32.to_le_bytes()
}

async fn find_gs_usb_interface(device: &Device) -> io::Result<Option<InterfaceInfo>> {
    let config = device
        .active_config_descriptor()
        .await
        .or_else(|_| device.config_descriptor(0).await)
        .map_err(map_usb_err)?;

    for interface in config.interfaces() {
        let number = interface.number();
        for descriptor in interface.descriptors() {
            if descriptor.class_code() != 0xff {
                continue;
            }
            let mut info = InterfaceInfo {
                interface: number,
                in_ep: 0,
                out_ep: 0,
                int_ep: None,
            };
            for endpoint in descriptor.endpoint_descriptors() {
                match endpoint.transfer_type() {
                    TransferType::Bulk => match endpoint.direction() {
                        Direction::In => info.in_ep = endpoint.address(),
                        Direction::Out => info.out_ep = endpoint.address(),
                    },
                    TransferType::Interrupt if endpoint.direction() == Direction::In => {
                        info.int_ep = Some(endpoint.address())
                    }
                    _ => {}
                }
            }
            if info.in_ep != 0 && info.out_ep != 0 {
                return Ok(Some(info));
            }
        }
    }

    Ok(None)
}

async fn device_matches_identifier(
    identifier: &str,
    index: usize,
    device: &Device,
    desc: &DeviceDescriptor,
    handle: &DeviceHandle,
) -> io::Result<bool> {
    let ident = identifier.trim();
    if ident.eq_ignore_ascii_case("auto") {
        return Ok(true);
    }

    if let Ok(idx) = ident.parse::<usize>() {
        if idx == index {
            return Ok(true);
        }
    }

    if let Ok(serial) = handle.read_serial_number_string_ascii(desc).await {
        if serial.eq_ignore_ascii_case(ident) {
            return Ok(true);
        }
    }

    if let Ok(product) = handle.read_product_string_ascii(desc).await {
        if product.eq_ignore_ascii_case(ident) {
            return Ok(true);
        }
    }

    let bus_addr = format!("{:03}:{:03}", device.bus_number(), device.address());
    Ok(bus_addr.eq_ignore_ascii_case(ident))
}

async fn select_device(identifier: &str) -> io::Result<(DeviceHandle, InterfaceInfo, String)> {
    let devices = usb::devices().await.map_err(map_usb_err)?;
    let mut index = 0usize;

    for device in devices.iter() {
        let desc = device.device_descriptor().await.map_err(map_usb_err)?;

        let info = match find_gs_usb_interface(device).await? {
            Some(i) => i,
            None => continue,
        };

        let handle = device.open().await.map_err(map_usb_err)?;

        let matches = device_matches_identifier(identifier, index, device, &desc, &handle).await?;

        if matches {
            let label = handle
                .read_product_string_ascii(&desc)
                .await
                .ok()
                .or_else(|| handle.read_serial_number_string_ascii(&desc).await.ok())
                .unwrap_or_else(|| format!("{:04x}:{:04x}", desc.vendor_id(), desc.product_id()));
            return Ok((handle, info, label));
        }

        index += 1;
    }

    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("No gs_usb device matched identifier '{identifier}'"),
    ))
}

fn calc_bit_timing(bitrate: u32) -> Option<GsDeviceBitTiming> {
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

fn parse_host_frame(bytes: &[u8], channel_index: u8) -> Option<CanFrame> {
    if bytes.len() < HOST_FRAME_SIZE {
        return None;
    }

    let echo_id = u32::from_le_bytes(bytes[0..4].try_into().ok()?);
    if echo_id != GS_CAN_ECHO_ID_UNUSED {
        return None;
    }

    let raw_id = u32::from_le_bytes(bytes[4..8].try_into().ok()?);
    let dlc = bytes[8];
    let channel = bytes[9];
    let _flags = bytes[10];
    let _reserved = bytes[11];

    if channel != channel_index {
        return None;
    }

    let data_len = dlc_to_len(dlc);
    if data_len > 64 {
        return None;
    }

    let data = &bytes[16..16 + data_len];

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

    Some(frame)
}

/// Driver for devices implementing the gs_usb protocol (e.g. candleLight / CANable).
pub struct GsUsbDriver {
    handle: DeviceHandle,
    interface: u8,
    in_ep: u8,
    out_ep: u8,
    #[allow(dead_code)]
    int_ep: Option<u8>,
    channel_index: u8,
    configured_bitrate: Option<u32>,
    timestamp_enabled: bool,
    rx_leftover: Vec<u8>,
    tx_counter: AtomicU32,
    device_label: String,
    last_timestamp64: Option<u64>,
}

impl GsUsbDriver {
    pub async fn open(identifier: &str) -> io::Result<Self> {
        let (handle, info, label) = select_device(identifier).await?;

        let _ = handle.set_auto_detach_kernel_driver(true).await;

        if handle.claim_interface(info.interface).await.is_err() {
            handle
                .claim_interface(info.interface)
                .await
                .map_err(map_usb_err)?;
        }

        let req_type = request_type_out();
        let host_cfg = host_config_bytes();
        let written = handle
            .write_control(
                req_type,
                GS_USB_BREQ_HOST_FORMAT,
                0,
                info.interface as u16,
                &host_cfg,
                USB_TIMEOUT,
            )
            .await
            .map_err(map_usb_err)?;
        if written != host_cfg.len() {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "Failed to send host format command to gs_usb device",
            ));
        }

        let mut driver = GsUsbDriver {
            handle,
            interface: info.interface,
            in_ep: info.in_ep,
            out_ep: info.out_ep,
            int_ep: info.int_ep,
            channel_index: 0,
            configured_bitrate: None,
            timestamp_enabled: false,
            rx_leftover: Vec::with_capacity(HOST_FRAME_SIZE * 4),
            tx_counter: AtomicU32::new(0),
            device_label: label,
            last_timestamp64: None,
        };

        let reset_bytes = encode_mode(GS_CAN_MODE_RESET, 0);
        driver.send_control(GS_USB_BREQ_MODE, &reset_bytes).await?;

        Ok(driver)
    }

    pub fn device_label(&self) -> &str {
        &self.device_label
    }

    async fn send_control(&self, request: u8, data: &[u8]) -> io::Result<()> {
        let written = self
            .handle
            .write_control(
                request_type_out(),
                request,
                0,
                self.interface as u16,
                data,
                USB_TIMEOUT,
            )
            .await
            .map_err(map_usb_err)?;
        if written != data.len() {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "Incomplete control transfer to gs_usb device",
            ));
        }
        Ok(())
    }
}

#[async_trait]
impl CanDriver for GsUsbDriver {
    async fn enable_timestamp(&mut self) -> io::Result<()> {
        let data = 1u32.to_le_bytes();
        self.send_control(GS_USB_BREQ_TIMESTAMP, &data).await?;
        self.timestamp_enabled = true;
        Ok(())
    }

    async fn set_bitrate(&mut self, bitrate: u32) -> io::Result<()> {
        let timing = calc_bit_timing(bitrate).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("Unable to compute bit timing for bitrate {bitrate}"),
            )
        })?;

        let reset = encode_mode(GS_CAN_MODE_RESET, 0);
        self.send_control(GS_USB_BREQ_MODE, &reset).await?;

        let data = timing.to_bytes();
        self.send_control(GS_USB_BREQ_BITTIMING, &data).await?;
        self.configured_bitrate = Some(bitrate);
        Ok(())
    }

    async fn get_bitrate(&self) -> Option<u32> {
        self.configured_bitrate
    }

    async fn open_channel(&mut self) -> io::Result<()> {
        let start = encode_mode(GS_CAN_MODE_START, 0);
        self.send_control(GS_USB_BREQ_MODE, &start).await
    }

    async fn send_frame(&mut self, frame: &CanFrame) -> io::Result<()> {
        let mut buffer = [0u8; HOST_FRAME_SIZE];

        // echo id
        let echo_id = self.tx_counter.fetch_add(1, Ordering::Relaxed) % GS_USB_MAX_ECHO_SLOTS;
        buffer[0..4].copy_from_slice(&echo_id.to_le_bytes());

        // can_id (+ flags)
        let mut can_id = frame.id();
        if frame.is_extended() {
            can_id |= CAN_EFF_FLAG;
        }
        if frame.is_rtr() {
            can_id |= CAN_RTR_FLAG;
        }
        if frame.is_error() {
            can_id |= CAN_ERR_FLAG;
        }
        buffer[4..8].copy_from_slice(&can_id.to_le_bytes());

        // dlc/channel/flags/reserved
        buffer[8] = frame.dlc() as u8;
        buffer[9] = self.channel_index;
        buffer[10] = 0; // flags
        buffer[11] = 0; // reserved

        // 32-bit timestamp for TX should be zeroed
        buffer[12..16].fill(0);

        // data (start at 16)
        let data_len = std::cmp::min(frame.data().len(), 64);
        buffer[16..16 + data_len].copy_from_slice(&frame.data()[..data_len]);

        let written = self
            .handle
            .write_bulk(self.out_ep, &buffer, USB_TIMEOUT)
            .await
            .map_err(map_usb_err)?;
        if written != HOST_FRAME_SIZE {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "Incomplete bulk transfer when sending CAN frame",
            ));
        }
        Ok(())
    }

    async fn read_frames(&mut self) -> io::Result<Vec<CanFrame>> {
        let mut tmp = vec![0u8; HOST_FRAME_SIZE * READ_CHUNK_FRAMES];
        let mut data = Vec::with_capacity(tmp.len() * 2);

        match self
            .handle
            .read_bulk(self.in_ep, &mut tmp, USB_TIMEOUT)
            .await
        {
            Ok(n) if n > 0 => data.extend_from_slice(&tmp[..n]),
            Ok(_) => return Ok(Vec::new()),
            Err(err) => {
                let io_err = map_usb_err(err);
                if io_err.kind() == io::ErrorKind::WouldBlock {
                    return Ok(Vec::new());
                }
                return Err(io_err);
            }
        }

        loop {
            match self
                .handle
                .read_bulk(self.in_ep, &mut tmp, DRAIN_READ_TIMEOUT)
                .await
            {
                Ok(n) if n > 0 => {
                    data.extend_from_slice(&tmp[..n]);
                    if n < tmp.len() {
                        break;
                    }
                }
                Ok(_) => break,
                Err(err) => {
                    let io_err = map_usb_err(err);
                    match io_err.kind() {
                        io::ErrorKind::WouldBlock => break,
                        io::ErrorKind::BrokenPipe => {
                            let _ = self.handle.clear_halt(self.in_ep).await;
                            break;
                        }
                        _ => return Err(io_err),
                    }
                }
            }
        }

        if data.is_empty() {
            return Ok(Vec::new());
        }

        self.rx_leftover.append(&mut data);

        let mut frames = Vec::new();
        let mut processed = 0usize;
        while self.rx_leftover.len() >= processed + HOST_FRAME_SIZE {
            let slice = &self.rx_leftover[processed..processed + HOST_FRAME_SIZE];
            if let Some(mut frame) = parse_host_frame(slice, self.channel_index) {
                // If timestamping is enabled, parse the 32-bit µs counter and extend to 64-bit.
                if self.timestamp_enabled && frame.timestamp().is_none() {
                    // On-wire is always 32-bit little-endian microseconds since device boot.
                    let ts32 = u32::from_le_bytes(slice[12..16].try_into().unwrap()) as u64;

                    // Extend to a monotonic 64-bit counter across wraparounds (~71 min period).
                    let ts64 = match self.last_timestamp64 {
                        None => ts32, // first observation
                        Some(last) => {
                            // Take upper 32 bits from last, splice in current 32-bit value.
                            let base = last & !0xFFFF_FFFFu64;
                            let mut candidate = base | ts32;
                            if candidate < last {
                                // 32-bit counter wrapped; bump upper bits by 1.
                                candidate = candidate.wrapping_add(1u64 << 32);
                            }
                            candidate
                        }
                    };

                    self.last_timestamp64 = Some(ts64);
                    frame.set_timestamp(Some(ts64));
                }
                frames.push(frame);
            }
            processed += HOST_FRAME_SIZE;
        }

        if processed > 0 {
            self.rx_leftover.drain(..processed);
        }

        Ok(frames)
    }

    async fn close_channel(&mut self) -> io::Result<()> {
        let reset = encode_mode(GS_CAN_MODE_RESET, 0);
        self.send_control(GS_USB_BREQ_MODE, &reset).await
    }
}
