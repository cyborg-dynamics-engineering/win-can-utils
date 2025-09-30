use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use crosscan::can::CanFrame;
use tokio::sync::{mpsc, oneshot};
use tokio::time::sleep;

use crate::can_driver::CanDriver;

use super::bit_timing::{calc_bit_timing, encode_mode};
use super::constants::*;
use super::context::{LibusbContext, LibusbDeviceHandle};
use super::device::select_device;
use super::frames::parse_host_frame_at;

/// State owned by the dedicated USB thread.
///
/// The gs_usb protocol requires that all libusb operations are serialized from a
/// single thread.  The thread receives [`UsbCommand`] messages from the async
/// side and streams incoming CAN frames back through an [`mpsc`] channel.
struct UsbEventLoop {
    handle: LibusbDeviceHandle,
    iface: u8,
    in_ep: u8,
    _out_ep: u8,
    cmd_rx: mpsc::Receiver<UsbCommand>,
    frame_tx: mpsc::Sender<CanFrame>,
    rx_buffer: Vec<u8>,
    last_timestamp64: Option<u64>,
    channel_index: u8,
    timestamp_enabled: bool,
}

impl UsbEventLoop {
    fn new(
        handle: LibusbDeviceHandle,
        iface: u8,
        in_ep: u8,
        out_ep: u8,
        cmd_rx: mpsc::Receiver<UsbCommand>,
        frame_tx: mpsc::Sender<CanFrame>,
    ) -> Self {
        Self {
            handle,
            iface,
            in_ep,
            _out_ep: out_ep,
            cmd_rx,
            frame_tx,
            rx_buffer: Vec::with_capacity(GS_MAX_FRAME_LEN * 4),
            last_timestamp64: None,
            channel_index: 0,
            timestamp_enabled: false,
        }
    }

    async fn run(mut self) -> io::Result<()> {
        // Keep a dedicated handle reference alive for the in-flight bulk read
        // future so that we can continue issuing control requests through
        // `self.handle` while the transfer is pending.
        let read_handle = self.handle.clone();
        let mut rx_transfer =
            Box::pin(read_handle.bulk_read(self.in_ep, USB_READ_BYTES, Duration::ZERO));

        loop {
            tokio::select! {
                biased;

                maybe_cmd = self.cmd_rx.recv() => {
                    let Some(command) = maybe_cmd else {
                        return Ok(());
                    };

                    if !self.handle_command(command).await? {
                        return Ok(());
                    }
                }

                result = &mut rx_transfer => {
                    self.handle_rx_completion(result).await?;
                    rx_transfer = Box::pin(read_handle.bulk_read(
                        self.in_ep,
                        USB_READ_BYTES,
                        Duration::ZERO,
                    ));
                }
            }
        }
    }

    async fn handle_command(&mut self, command: UsbCommand) -> io::Result<bool> {
        match command {
            UsbCommand::ControlOut {
                request_type,
                request,
                value,
                index,
                data,
                resp,
            } => {
                let result = self.handle.control_out_blocking(
                    request_type,
                    request,
                    value,
                    index,
                    &data,
                    USB_TIMEOUT,
                );

                if request == GS_USB_BREQ_MODE {
                    self.channel_index = value as u8;
                }

                if request == GS_USB_BREQ_TIMESTAMP {
                    self.timestamp_enabled = data.first().map(|b| *b != 0).unwrap_or(false);
                }

                let _ = resp.send(result);
                Ok(true)
            }
            UsbCommand::ControlIn {
                request_type,
                request,
                value,
                index,
                len,
                resp,
            } => {
                let mut buffer = vec![0u8; len];
                let result = self
                    .handle
                    .control_in_blocking(
                        request_type,
                        request,
                        value,
                        index,
                        &mut buffer,
                        USB_TIMEOUT,
                    )
                    .map(|written| {
                        buffer.truncate(written);
                        buffer
                    });
                let _ = resp.send(result);
                Ok(true)
            }
            UsbCommand::BulkWrite {
                endpoint,
                data,
                resp,
            } => {
                let result = self.bulk_write(endpoint, data).await;
                let _ = resp.send(result);
                Ok(true)
            }
            UsbCommand::Shutdown {} => Ok(false),
        }
    }

    async fn handle_rx_completion(&mut self, result: io::Result<Vec<u8>>) -> io::Result<()> {
        match result {
            Ok(chunk) => self.process_rx_chunk(&chunk).await,
            Err(error) if error.kind() == io::ErrorKind::NotConnected => Err(error),
            Err(_) => {
                let _ = self.handle.clear_halt(self.in_ep);
                tokio::time::sleep(Duration::from_millis(5)).await;
                Ok(())
            }
        }
    }

    async fn process_rx_chunk(&mut self, chunk: &[u8]) -> io::Result<()> {
        if chunk.is_empty() {
            return Ok(());
        }

        self.rx_buffer.extend_from_slice(chunk);

        let mut offset = 0usize;
        while self.rx_buffer.len() >= offset + GS_HEADER_LEN {
            let slice = &self.rx_buffer[offset..];
            match parse_host_frame_at(
                slice,
                self.channel_index,
                self.timestamp_enabled,
                &mut self.last_timestamp64,
            ) {
                None => break,
                Some((maybe_frame, consumed)) => {
                    if let Some(frame) = maybe_frame {
                        let _ = self.frame_tx.send(frame).await;
                    }
                    offset += consumed;
                }
            }
        }

        if offset > 0 {
            self.rx_buffer.drain(..offset);
        }

        Ok(())
    }

    async fn bulk_write(&mut self, endpoint: u8, data: Vec<u8>) -> io::Result<usize> {
        const TX_TIMEOUT: Duration = Duration::from_millis(5);

        let result = self.handle.bulk_write_blocking(endpoint, data, TX_TIMEOUT);

        if let Err(error) = &result {
            match error.kind() {
                io::ErrorKind::WouldBlock | io::ErrorKind::BrokenPipe => {
                    let _ = self.handle.clear_halt(endpoint);
                    if error.kind() == io::ErrorKind::BrokenPipe {
                        self.recover_after_stall().await?;
                    }
                }
                _ => {}
            }
        }

        result
    }

    async fn recover_after_stall(&mut self) -> io::Result<()> {
        let reset = encode_mode(GS_CAN_MODE_RESET, 0);
        self.handle.control_out_blocking(
            request_type_out(),
            GS_USB_BREQ_MODE,
            self.channel_index as u16,
            self.iface as u16,
            &reset,
            Duration::from_millis(50),
        )?;

        let start = encode_mode(GS_CAN_MODE_START, 0);
        self.handle.control_out_blocking(
            request_type_out(),
            GS_USB_BREQ_MODE,
            self.channel_index as u16,
            self.iface as u16,
            &start,
            Duration::from_millis(50),
        )?;

        Ok(())
    }
}

/// Commands sent to the USB event loop thread.
enum UsbCommand {
    ControlOut {
        request_type: u8,
        request: u8,
        value: u16,
        index: u16,
        data: Vec<u8>,
        resp: oneshot::Sender<io::Result<usize>>,
    },
    ControlIn {
        request_type: u8,
        request: u8,
        value: u16,
        index: u16,
        len: usize,
        resp: oneshot::Sender<io::Result<Vec<u8>>>,
    },
    BulkWrite {
        endpoint: u8,
        data: Vec<u8>,
        resp: oneshot::Sender<io::Result<usize>>,
    },
    #[allow(dead_code)]
    Shutdown,
}

/// High level driver used by the rest of the crate to talk to gs_usb adapters.
pub struct GsUsbDriver {
    // device info
    interface: u8,
    _in_ep: u8,
    out_ep: u8,
    _int_ep: Option<u8>,
    channel_index: u8,
    device_label: String,

    // state
    configured_bitrate: Option<u32>,
    timestamp_enabled: bool,
    tx_counter: AtomicU32,

    // feature discovery
    features: u32,  // feature bitmask from BT_CONST(_EXT)
    out_wmax: u16,  // OUT endpoint wMaxPacketSize (e.g., 32)
    pad_pkts: bool, // true if device wants padding

    // async integration
    frame_rx: Arc<Mutex<mpsc::Receiver<CanFrame>>>,
    cmd_tx: mpsc::Sender<UsbCommand>, // to USB event loop
}

impl GsUsbDriver {
    /// Open a gs_usb adapter matching `identifier` and spin up the USB event loop.
    pub async fn open(identifier: &str) -> io::Result<Self> {
        let context = LibusbContext::new()?;
        let (handle, info, label) = select_device(&context, identifier)?;

        let _ = handle.set_auto_detach_kernel_driver(true);
        handle.claim_interface(info.interface as i32)?;

        // Channel: event loop <-> driver commands
        let (cmd_tx, cmd_rx) = mpsc::channel::<UsbCommand>(128);

        // Channel: frames to async side
        let (frame_tx, frame_rx) = mpsc::channel::<CanFrame>(1024);

        // Driver instance
        let mut driver = GsUsbDriver {
            interface: info.interface,
            _in_ep: info.in_ep,
            out_ep: info.out_ep,
            _int_ep: info.int_ep,
            channel_index: 0,
            device_label: label,

            configured_bitrate: None,
            timestamp_enabled: false,
            tx_counter: AtomicU32::new(0),

            features: 0,
            out_wmax: 32, // candleLight FS devices: 32-byte packets
            pad_pkts: false,

            frame_rx: Arc::new(Mutex::new(frame_rx)),
            cmd_tx: cmd_tx.clone(),
        };

        // Spawn the single-owner USB event loop thread. It owns `handle` and
        // serializes all libusb access behind the [`UsbCommand`] channel.
        std::thread::spawn(move || {
            let _ = catch_unwind(AssertUnwindSafe(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("tokio runtime");

                let event_loop = UsbEventLoop::new(
                    handle,
                    info.interface,
                    info.in_ep,
                    info.out_ep,
                    cmd_rx,
                    frame_tx,
                );
                let _ = runtime.block_on(event_loop.run());
            }));
        });

        // === Handshake & feature discovery (through the event loop) ===
        driver.send_host_format().await?;
        let _dev_conf = driver.read_device_config().await?; // validates comms
        let features = driver.read_features().await.unwrap_or(0);
        driver.features = features;
        driver.out_wmax = 32; // from your descriptor dump
        driver.pad_pkts = (features & GS_CAN_FEATURE_PAD_PKTS_TO_MAX_PKT_SIZE) != 0;

        Ok(driver)
    }

    // === Command helpers that talk to the event loop ===

    async fn cmd_control_out(
        &self,
        request_type: u8,
        request: u8,
        value: u16,
        index: u16,
        data: Vec<u8>,
    ) -> io::Result<usize> {
        let (resp_tx, resp_rx) = oneshot::channel();
        self.cmd_tx
            .send(UsbCommand::ControlOut {
                request_type,
                request,
                value,
                index,
                data,
                resp: resp_tx,
            })
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "USB event loop closed"))?;
        resp_rx
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "USB event loop dropped"))?
    }

    async fn cmd_control_in(
        &self,
        request_type: u8,
        request: u8,
        value: u16,
        index: u16,
        len: usize,
    ) -> io::Result<Vec<u8>> {
        let (resp_tx, resp_rx) = oneshot::channel();
        self.cmd_tx
            .send(UsbCommand::ControlIn {
                request_type,
                request,
                value,
                index,
                len,
                resp: resp_tx,
            })
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "USB event loop closed"))?;
        resp_rx
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "USB event loop dropped"))?
    }

    async fn cmd_bulk_write(&self, endpoint: u8, data: Vec<u8>) -> io::Result<usize> {
        let (resp_tx, resp_rx) = oneshot::channel();
        self.cmd_tx
            .send(UsbCommand::BulkWrite {
                endpoint,
                data,
                resp: resp_tx,
            })
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "USB event loop closed"))?;
        resp_rx
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "USB event loop dropped"))?
    }

    // === gs_usb protocol ===

    async fn send_host_format(&self) -> io::Result<()> {
        // Host-format handshake: 0x0000BEEF (little-endian on wire)
        let byte_order = 0x0000_beefu32.to_le_bytes().to_vec();
        // HOST_FORMAT
        let written = self
            .cmd_control_out(
                request_type_out(),
                GS_USB_BREQ_HOST_FORMAT,
                1,                     // value = 1  (match Linux driver)
                self.interface as u16, // index = interface
                0x0000_beefu32.to_le_bytes().to_vec(),
            )
            .await?;
        if written != byte_order.len() {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "host_format short write",
            ));
        }
        Ok(())
    }

    async fn read_device_config(&self) -> io::Result<[u8; 8]> {
        // DEVICE_CONFIG
        let buf = self
            .cmd_control_in(
                request_type_in(),
                GS_USB_BREQ_DEVICE_CONFIG,
                1,                     // value = 1
                self.interface as u16, // index = interface
                8,
            )
            .await?;

        if buf.len() < 4 {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                format!("device_config too short: {} bytes", buf.len()),
            ));
        }

        let mut arr = [0u8; 8];
        let available = buf.len().min(arr.len());
        arr[..available].copy_from_slice(&buf[..available]);
        Ok(arr)
    }

    async fn read_features(&self) -> io::Result<u32> {
        // Try BT_CONST_EXT first
        if let Ok(buf) = self
            .cmd_control_in(
                request_type_in(),
                GS_USB_BREQ_BT_CONST_EXT,
                0,
                self.interface as u16,
                4 + 4 * 12, // big enough
            )
            .await
        {
            if buf.len() >= 4 {
                return Ok(u32::from_le_bytes(buf[0..4].try_into().unwrap()));
            }
        }
        // Fallback to BT_CONST
        let b = self
            .cmd_control_in(
                request_type_in(),
                GS_USB_BREQ_BT_CONST,
                0,
                self.interface as u16,
                4,
            )
            .await?;
        if b.len() != 4 {
            return Err(io::Error::new(io::ErrorKind::Other, "bt_const short read"));
        }
        Ok(u32::from_le_bytes(b.try_into().unwrap()))
    }

    #[allow(dead_code)]
    async fn send_control(&self, request: u8, data: &[u8]) -> io::Result<()> {
        let written = self
            .cmd_control_out(request_type_out(), request, 0, 0, data.to_vec())
            .await?;
        if written != data.len() {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "incomplete control transfer",
            ));
        }
        Ok(())
    }

    fn encode_frame_auto(&self, frame: &CanFrame) -> Vec<u8> {
        // If the device reports CAN-FD support, use 76-byte frames
        if self.features & GS_CAN_FEATURE_FD != 0 {
            self.encode_frame_tx_76(frame)
        } else {
            self.encode_frame_minimal(frame)
        }
    }

    fn encode_frame_tx_76(&self, frame: &CanFrame) -> Vec<u8> {
        let mut buf = vec![0u8; GS_TX_FRAME_SIZE]; // 76

        // echo_id: rotating 0..15
        let echo_id = self.tx_counter.fetch_add(1, Ordering::Relaxed) % 16;
        buf[0..4].copy_from_slice(&(echo_id as u32).to_le_bytes());

        // can_id (+flags)
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
        buf[4..8].copy_from_slice(&(can_id as u32).to_le_bytes());

        // dlc, channel, flags, reserved
        buf[8] = frame.dlc() as u8;
        buf[9] = self.channel_index;
        buf[10] = 0; // flags (no FD/BRS here)
        buf[11] = 0; // reserved

        // data[64], zero-padded
        let data = frame.data();
        let n = data.len().min(64);
        buf[12..12 + n].copy_from_slice(&data[..n]);
        // remaining bytes already zero

        buf
    }

    fn encode_frame_minimal(&self, frame: &CanFrame) -> Vec<u8> {
        let mut buf = vec![0u8; 20]; // 12 header + 8 data

        // echo_id (0..15 is fine)
        let echo_id = self.tx_counter.fetch_add(1, Ordering::Relaxed) % 16;
        buf[0..4].copy_from_slice(&(echo_id as u32).to_le_bytes());

        // can_id (+ flags)
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
        buf[4..8].copy_from_slice(&(can_id as u32).to_le_bytes());

        // dlc, channel, flags, reserved
        buf[8] = frame.dlc() as u8;
        buf[9] = self.channel_index;
        buf[10] = 0;
        buf[11] = 0;

        // data (pad to 8)
        let d = frame.data();
        let n = d.len().min(8);
        buf[12..12 + n].copy_from_slice(&d[..n]); // remaining bytes stay 0

        // DO NOT pad to 32 unless you set PAD_PKTS mode
        buf
    }

    async fn send_frame(&mut self, frame: &CanFrame) -> io::Result<()> {
        let buffer = self.encode_frame_auto(frame);

        loop {
            match self.cmd_bulk_write(self.out_ep, buffer.clone()).await {
                Ok(written) if written == buffer.len() => {
                    return Ok(());
                }
                Ok(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::Other,
                        "incomplete bulk write",
                    ));
                }
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                    sleep(USB_WRITE_RETRY_DELAY).await;
                }
                Err(err) => return Err(err),
            }
        }
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

    async fn open_channel_inner(&mut self) -> io::Result<()> {
        let mut flags = 0u32;
        if self.timestamp_enabled {
            flags |= GS_CAN_MODE_HW_TIMESTAMP;
        }
        if self.pad_pkts {
            flags |= GS_CAN_MODE_PAD_PKTS_TO_MAX_PKT_SIZE;
        }

        self.cmd_control_out(
            request_type_out(),
            GS_USB_BREQ_MODE,
            self.channel_index as u16, // value = channel
            self.interface as u16,     // index = interface
            encode_mode(GS_CAN_MODE_START, flags).to_vec(),
        )
        .await
        .map(|_| ())
    }

    async fn close_channel_inner(&mut self) -> io::Result<()> {
        let _reset = encode_mode(GS_CAN_MODE_RESET, 0);

        self.cmd_control_out(
            request_type_out(),
            GS_USB_BREQ_MODE,
            self.channel_index as u16, // value = channel
            self.interface as u16,     // index = interface
            encode_mode(GS_CAN_MODE_RESET, 0).to_vec(),
        )
        .await
        .map(|_| ()) // <-- map usize -> ()
    }

    pub fn device_label(&self) -> &str {
        &self.device_label
    }
}

#[async_trait]
impl CanDriver for GsUsbDriver {
    async fn enable_timestamp(&mut self) -> io::Result<()> {
        self.cmd_control_out(
            request_type_out(),
            GS_USB_BREQ_TIMESTAMP,
            self.channel_index as u16, // value = channel
            self.interface as u16,     // index = interface
            1u32.to_le_bytes().to_vec(),
        )
        .await?;
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

        self.cmd_control_out(
            // MODE = RESET first
            request_type_out(),
            GS_USB_BREQ_MODE,
            self.channel_index as u16, // value = channel
            self.interface as u16,     // index = interface
            encode_mode(GS_CAN_MODE_RESET, 0).to_vec(),
        )
        .await?;

        self.cmd_control_out(
            // BITTIMING
            request_type_out(),
            GS_USB_BREQ_BITTIMING,
            self.channel_index as u16, // value = channel
            self.interface as u16,     // index = interface
            timing.to_bytes().to_vec(),
        )
        .await?;

        self.configured_bitrate = Some(bitrate);
        Ok(())
    }

    async fn get_bitrate(&self) -> Option<u32> {
        self.configured_bitrate
    }

    async fn open_channel(&mut self) -> io::Result<()> {
        self.open_channel_inner().await
    }

    async fn send_frame(&mut self, frame: &CanFrame) -> io::Result<()> {
        self.send_frame(frame).await
    }

    async fn read_frames(&mut self) -> io::Result<Vec<CanFrame>> {
        self.read_frames().await
    }

    async fn close_channel(&mut self) -> io::Result<()> {
        self.close_channel_inner().await
    }
}
