use std::io;
use std::sync::atomic::AtomicU32;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use crosscan::can::CanFrame;
use tokio::sync::mpsc;

use crate::can_driver::CanDriver;

use super::bit_timing::{calc_bit_timing, encode_mode};
use super::constants::*;
use super::context::{LibusbContext, LibusbDeviceHandle};
use super::device::select_device;
use super::frames::parse_host_frame_at;

/// High level driver used by the rest of the crate to talk to gs_usb adapters.
pub struct GsUsbDriver {
    handle: LibusbDeviceHandle,
    interface: u8,
    in_ep: u8,
    out_ep: u8,
    int_ep: Option<u8>,
    channel_index: u8,
    configured_bitrate: Option<u32>,
    timestamp_enabled: bool,
    rx_leftover: Vec<u8>,
    tx_counter: AtomicU32,
    device_label: String,
    last_timestamp64: Option<u64>,
    frame_rx: Arc<Mutex<mpsc::Receiver<CanFrame>>>,
}

impl GsUsbDriver {
    /// Open a gs_usb adapter matching `identifier` and spin up background IO helpers.
    pub async fn open(identifier: &str) -> io::Result<Self> {
        let context = LibusbContext::new()?;
        let (handle, info, label) = select_device(&context, identifier)?;

        let _ = handle.set_auto_detach_kernel_driver(true);
        handle.claim_interface(info.interface as i32)?;

        // Channel used to hand off frames from the blocking reader thread to async context.
        let (tx, rx) = mpsc::channel::<CanFrame>(1024);

        let mut driver = GsUsbDriver {
            handle: handle.clone(),
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
            frame_rx: Arc::new(Mutex::new(rx)),
        };

        // Launch a blocking reader thread so that we keep draining the USB queue even
        // when the async runtime is busy. The thread simply forwards decoded CAN frames
        // through `tx`.
        driver.spawn_blocking_reader(handle, info.in_ep, tx);

        Ok(driver)
    }

    /// Clone driver state for the background reader thread.
    fn clone_for_reader(&self) -> Self {
        GsUsbDriver {
            handle: self.handle.clone(),
            interface: self.interface,
            in_ep: self.in_ep,
            out_ep: self.out_ep,
            int_ep: self.int_ep,
            channel_index: self.channel_index,
            configured_bitrate: self.configured_bitrate,
            timestamp_enabled: self.timestamp_enabled,
            rx_leftover: Vec::with_capacity(HOST_FRAME_SIZE * 4),
            tx_counter: AtomicU32::new(0),
            device_label: self.device_label.clone(),
            last_timestamp64: None,
            frame_rx: self.frame_rx.clone(),
        }
    }

    fn spawn_blocking_reader(
        &mut self,
        handle: LibusbDeviceHandle,
        in_ep: u8,
        frame_tx: mpsc::Sender<CanFrame>,
    ) {
        let mut bg_driver = self.clone_for_reader();
        std::thread::spawn(move || {
            loop {
                match handle.bulk_read_blocking(in_ep, USB_READ_BYTES, USB_TIMEOUT) {
                    Ok(chunk) if !chunk.is_empty() => {
                        bg_driver.rx_leftover.extend_from_slice(&chunk);

                        let mut offset = 0;
                        while bg_driver.rx_leftover.len() >= offset + GS_HEADER_LEN {
                            let slice = &bg_driver.rx_leftover[offset..];
                            match parse_host_frame_at(
                                slice,
                                bg_driver.channel_index,
                                bg_driver.timestamp_enabled,
                                &mut bg_driver.last_timestamp64,
                            ) {
                                None => break,
                                Some((maybe_frame, consumed)) => {
                                    if let Some(frame) = maybe_frame {
                                        if frame_tx.blocking_send(frame).is_err() {
                                            return;
                                        }
                                    }
                                    offset += consumed;
                                }
                            }
                        }
                        if offset > 0 {
                            bg_driver.rx_leftover.drain(..offset);
                        }
                    }
                    Ok(_) => {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    Err(err) => {
                        eprintln!("[blocking reader][error] {:?}", err);
                        return;
                    }
                }
            }
        });
    }

    pub fn device_label(&self) -> &str {
        &self.device_label
    }

    async fn send_control(&self, request: u8, data: &[u8]) -> io::Result<()> {
        let written = self
            .handle
            .control_out(
                request_type_out(),
                request,
                0,
                self.interface as u16,
                data.to_vec(),
                USB_TIMEOUT,
            )
            .await?;
        if written != data.len() {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "Incomplete control transfer to gs_usb device",
            ));
        }
        Ok(())
    }

    async fn send_frame(&mut self, frame: &CanFrame) -> io::Result<()> {
        let mut buffer = vec![0u8; HOST_FRAME_SIZE];

        let mut can_id = if frame.is_extended() {
            frame.id() & CAN_EFF_MASK
        } else {
            frame.id() & CAN_SFF_MASK
        };
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

        buffer[8] = frame.dlc() as u8;
        buffer[9] = self.channel_index;
        buffer[10] = 0;
        buffer[11] = 0;

        let mut data_off = GS_HEADER_LEN;
        if self.timestamp_enabled {
            buffer[12..16].fill(0);
            data_off += GS_TS_LEN;
        }

        let frame_data = frame.data();
        let data_len = frame_data.len().min(64);
        buffer[data_off..data_off + data_len].copy_from_slice(&frame_data[..data_len]);

        let written = self
            .handle
            .bulk_write(self.out_ep, buffer, USB_TIMEOUT)
            .await?;
        if written != HOST_FRAME_SIZE {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "Incomplete bulk transfer when sending CAN frame",
            ));
        }
        Ok(())
    }

    async fn read_frames(&mut self) -> io::Result<Vec<CanFrame>> {
        let mut frames = Vec::new();
        if let Ok(mut rx) = self.frame_rx.lock() {
            while let Ok(frame) = rx.try_recv() {
                frames.push(frame);
            }
        }
        Ok(frames)
    }

    async fn open_channel(&mut self) -> io::Result<()> {
        let start = encode_mode(GS_CAN_MODE_START, 0);
        self.send_control(GS_USB_BREQ_MODE, &start).await
    }

    async fn close_channel(&mut self) -> io::Result<()> {
        let reset = encode_mode(GS_CAN_MODE_RESET, 0);
        self.send_control(GS_USB_BREQ_MODE, &reset).await
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
        self.open_channel().await
    }

    async fn send_frame(&mut self, frame: &CanFrame) -> io::Result<()> {
        self.send_frame(frame).await
    }

    async fn read_frames(&mut self) -> io::Result<Vec<CanFrame>> {
        self.read_frames().await
    }

    async fn close_channel(&mut self) -> io::Result<()> {
        self.close_channel().await
    }
}
