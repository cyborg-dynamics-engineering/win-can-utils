use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::drivers::CanDriver;
use async_trait::async_trait;
use crosscan::can::CanFrame;
use futures::StreamExt;
use futures::stream::FuturesUnordered;
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

use super::bit_timing::{GsBtConst, calc_bit_timing, encode_mode, parse_bt_const};
use super::constants::*;
use super::context::{LibusbContext, LibusbDeviceHandle, map_libusb_error};
use super::device::select_device;
use super::frames::parse_host_frame_at;

use log::{debug, error, info, warn};
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
    out_wmax: usize,
    pad_pkts_enabled: bool,
}

impl UsbEventLoop {
    fn new(
        handle: LibusbDeviceHandle,
        iface: u8,
        in_ep: u8,
        out_ep: u8,
        cmd_rx: mpsc::Receiver<UsbCommand>,
        frame_tx: mpsc::Sender<CanFrame>,
        out_wmax: usize,
        pad_pkts_enabled: bool,
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
            out_wmax,
            pad_pkts_enabled,
        }
    }

    const NUM_RX_TRANSFERS: usize = 8;

    async fn run(mut self) -> io::Result<()> {
        let read_handle = self.handle.clone();

        type BoxedReadFuture = Pin<Box<dyn Future<Output = (usize, io::Result<Vec<u8>>)> + Send>>;

        let mut inflight: FuturesUnordered<BoxedReadFuture> = (0..Self::NUM_RX_TRANSFERS)
            .map(|i| {
                let h = read_handle.clone();
                let ep = self.in_ep;
                let len = self.out_wmax.max(GS_HEADER_LEN);

                Box::pin(async move { (i, h.bulk_read(ep, len, Duration::ZERO).await) })
                    as BoxedReadFuture
            })
            .collect();

        // Track completion times so the debug output includes spacing between packets.
        let mut last_rx = Instant::now();

        loop {
            tokio::select! {
                maybe_cmd = self.cmd_rx.recv() => {
                    let Some(command) = maybe_cmd else { return Ok(()); };
                    if !self.handle_command(command).await? { return Ok(()); }
                }

                Some((idx, result)) = inflight.next() => {
                    let now = Instant::now();
                    let delta = now.duration_since(last_rx);
                    last_rx = now;

                    match result {
                        Ok(buf) => {
                            log::debug!(
                                "RX transfer {} completed: {} bytes, Δt={:?}",
                                idx,
                                buf.len(),
                                delta
                            );
                            self.handle_rx_completion(Ok(buf)).await?;
                        }
                        Err(e) => {
                            log::error!("RX transfer {} failed: {:?}, Δt={:?}", idx, e, delta);
                        }
                    }

                    // Submit a new read for the slot we just handled.
                    let h = read_handle.clone();
                    let ep = self.in_ep;
                    let len = self.out_wmax.max(GS_HEADER_LEN);
                    inflight.push(Box::pin(async move {
                        (idx, h.bulk_read(ep, len, Duration::ZERO).await)
                    }) as BoxedReadFuture);
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
            UsbCommand::UpdateConfig {
                out_wmax,
                pad_pkts_enabled,
            } => {
                self.out_wmax = out_wmax;
                self.pad_pkts_enabled = pad_pkts_enabled;
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
                self.out_wmax,
                self.pad_pkts_enabled,
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
        const TX_TIMEOUT: Duration = Duration::from_millis(20);

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
    UpdateConfig {
        out_wmax: usize,
        pad_pkts_enabled: bool,
    },
}

/// High level driver used by the rest of the crate to talk to gs_usb adapters.
pub struct GsUsbDriver {
    /// Interface identifier claimed on the adapter.
    interface: u8,
    /// Bulk IN endpoint used for RX (held for completeness).
    _in_ep: u8,
    /// Bulk OUT endpoint used for TX submissions.
    out_ep: u8,
    /// Optional interrupt endpoint exposed by some adapters.
    _int_ep: Option<u8>,
    /// Selected CAN channel index.
    channel_index: u8,
    /// Human readable description of the attached device.
    device_label: String,

    /// Bitrate chosen via CLI or auto-detection.
    configured_bitrate: Option<u32>,
    /// Tracks whether timestamping is enabled in firmware.
    timestamp_enabled: bool,
    /// Incremented for each transmitted frame to generate echo identifiers.
    tx_counter: AtomicU32,

    /// Feature bitmask returned by BT_CONST/BT_CONST_EXT.
    features: u32,
    /// Maximum packet size supported by the OUT endpoint.
    out_wmax: u16,
    /// Flag toggled when PAD_PKTS handshake succeeds.
    pad_pkts: Arc<AtomicBool>,

    /// Receiver half of the frame channel bridging the USB thread.
    frame_rx: Arc<Mutex<mpsc::Receiver<CanFrame>>>,
    /// Command channel to the dedicated USB event loop thread.
    cmd_tx: mpsc::Sender<UsbCommand>,

    /// Cached BT_CONST descriptor for reference when changing bit timings.
    bt_const: Option<GsBtConst>,
}

impl GsUsbDriver {
    /// Open a gs_usb adapter matching `identifier` and spin up the USB event loop.
    pub async fn open(identifier: &str) -> io::Result<Self> {
        let context = LibusbContext::new()?;
        let (handle, info, label) = select_device(&context, identifier)?;

        info!("OUT endpoint wMaxPacketSize = {}", info.out_wmax);

        let _ = handle.set_auto_detach_kernel_driver(true);
        handle.claim_interface(info.interface as i32)?;

        let rc = unsafe {
            libusb1_sys::libusb_set_interface_alt_setting(
                handle.raw(),
                info.interface as i32,
                info.alt_setting as i32,
            )
        };

        if rc < 0 {
            return Err(map_libusb_error(rc));
        }

        log::info!(
            "Using interface {} alt_setting {}",
            info.interface,
            info.alt_setting
        );

        // Channel carrying driver requests into the USB event loop.
        let (cmd_tx, cmd_rx) = mpsc::channel::<UsbCommand>(128);

        // Channel streaming decoded frames back to async callers.
        let (frame_tx, frame_rx) = mpsc::channel::<CanFrame>(1024);

        // Assemble the high-level driver state shared with async callers.
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
            out_wmax: info.out_wmax,
            pad_pkts: Arc::new(AtomicBool::new(false)),

            frame_rx: Arc::new(Mutex::new(frame_rx)),
            cmd_tx: cmd_tx.clone(),

            bt_const: None,
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
                    info.out_wmax as usize,
                    false,
                );
                let _ = runtime.block_on(event_loop.run());
            }));
        });

        // Perform the handshake and discover optional firmware capabilities.
        driver.send_host_format().await?;

        let bt = driver.read_bt_const().await?;
        driver.features = bt.feature;
        driver.bt_const = Some(bt);
        driver.out_wmax = info.out_wmax;

        let _dev_conf = driver.read_device_config().await?;
        let features = driver.read_features().await.unwrap_or(0);
        driver.features = features;

        // Let the USB thread know about negotiated padding behaviour.
        let _ = driver
            .cmd_tx
            .send(UsbCommand::UpdateConfig {
                out_wmax: info.out_wmax as usize,
                pad_pkts_enabled: (features & GS_CAN_FEATURE_PAD_PKTS_TO_MAX_PKT_SIZE) != 0,
            })
            .await;

        driver.out_wmax = info.out_wmax;

        driver.pad_pkts = Arc::new(AtomicBool::new(
            (features & GS_CAN_FEATURE_PAD_PKTS_TO_MAX_PKT_SIZE) != 0,
        ));

        log::info!(
            "CAN init: features={:#010x} out_wmax={} pad_pkts_fw={} bitrate={:?}",
            driver.features,
            driver.out_wmax,
            driver.pad_pkts.load(Ordering::Relaxed),
            driver.configured_bitrate
        );

        Ok(driver)
    }

    pub async fn open_listen_only(&mut self) -> io::Result<()> {
        if self.configured_bitrate.is_none() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "set_bitrate() must be called before open_listen_only()",
            ));
        }
        self.open_channel_with_flags(Some(GS_CAN_MODE_LISTEN_ONLY))
            .await
    }

    async fn open_channel_with_flags(&mut self, extra_flags: Option<u32>) -> io::Result<()> {
        let mut flags = 0u32;
        if self.timestamp_enabled {
            flags |= GS_CAN_MODE_HW_TIMESTAMP;
        }

        let enable_pad_pkts = true; // allow padding when firmware supports it
        if enable_pad_pkts && self.pad_pkts.load(Ordering::Relaxed) {
            flags |= GS_CAN_MODE_PAD_PKTS_TO_MAX_PKT_SIZE;
        }

        if let Some(f) = extra_flags {
            flags |= f;
        }

        log::debug!(
            "Opening channel {}, iface={}, flags={:#010x}",
            self.channel_index,
            self.interface,
            flags
        );

        // Transition the adapter from RESET to START with the negotiated flags.
        self.cmd_control_out(
            request_type_out(),
            GS_USB_BREQ_MODE,
            self.channel_index as u16,
            self.interface as u16,
            encode_mode(GS_CAN_MODE_RESET, 0).to_vec(),
        )
        .await?;
        log::debug!(" → Sent MODE RESET");

        self.cmd_control_out(
            request_type_out(),
            GS_USB_BREQ_MODE,
            self.channel_index as u16,
            self.interface as u16,
            encode_mode(GS_CAN_MODE_START, flags).to_vec(),
        )
        .await?;
        log::debug!(" → Sent MODE START (flags={:#010x})", flags);

        Ok(())
    }

    fn maybe_pad_tx(&self, mut buf: Vec<u8>) -> Vec<u8> {
        if self.pad_pkts.load(Ordering::Relaxed) {
            let m = self.out_wmax as usize;
            if m > 0 {
                let rem = buf.len() % m;
                if rem != 0 {
                    buf.resize(buf.len() + (m - rem), 0);
                }
            }
        }
        buf
    }

    // Helpers that proxy control and bulk messages to the USB event loop.

    async fn cmd_control_out(
        &self,
        request_type: u8,
        request: u8,
        value: u16,
        index: u16,
        data: Vec<u8>,
    ) -> io::Result<usize> {
        debug!(
            "ControlOut req={:#04x}, val={:#06x}, idx={:#06x}, len={}, data={:02x?}",
            request,
            value,
            index,
            data.len(),
            &data
        );
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
        let res = resp_rx
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "USB event loop dropped"))?;
        debug!(" → ControlOut result: {:?}", res);
        res
    }

    async fn cmd_control_in(
        &self,
        request_type: u8,
        request: u8,
        value: u16,
        index: u16,
        len: usize,
    ) -> io::Result<Vec<u8>> {
        debug!(
            "ControlIn → req={:#04x}, val={:#06x}, idx={:#06x}, len={}",
            request, value, index, len
        );

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
            .map_err(|_| {
                warn!("ControlIn → USB event loop closed before send");
                io::Error::new(io::ErrorKind::Other, "USB event loop closed")
            })?;

        match resp_rx.await {
            Ok(Ok(buf)) => {
                debug!("ControlIn ← got {} bytes: {:02x?}", buf.len(), &buf);
                Ok(buf)
            }
            Ok(Err(e)) => {
                warn!("ControlIn ← error: {}", e);
                Err(e)
            }
            Err(_) => {
                warn!("ControlIn ← USB event loop dropped before response");
                Err(io::Error::new(
                    io::ErrorKind::Other,
                    "USB event loop dropped",
                ))
            }
        }
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
        info!("Device config raw: {:02x?}", arr);
        Ok(arr)
    }

    async fn read_bt_const(&self) -> io::Result<GsBtConst> {
        match self
            .cmd_control_in(
                request_type_in(),
                GS_USB_BREQ_BT_CONST_EXT,
                0,
                self.interface as u16,
                52,
            )
            .await
        {
            Ok(buf) if buf.len() >= 40 => return Ok(parse_bt_const(&buf[..40])),
            Ok(_) => warn!("BT_CONST_EXT short, falling back to BT_CONST"),
            Err(e) => warn!("BT_CONST_EXT not supported, falling back: {}", e),
        }

        let buf = self
            .cmd_control_in(
                request_type_in(),
                GS_USB_BREQ_BT_CONST,
                0,
                self.interface as u16,
                40,
            )
            .await?;
        if buf.len() < 40 {
            return Err(io::Error::new(io::ErrorKind::Other, "BT_CONST short read"));
        }
        Ok(parse_bt_const(&buf[..40]))
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
        let fd_supported = (self.features & GS_CAN_FEATURE_FD) != 0;

        // Encode primary attempt
        let mut buf = if fd_supported {
            self.encode_frame_tx_76(frame) // 76 bytes for CAN-FD
        } else {
            self.encode_frame_minimal(frame) // 20 bytes for classic CAN
        };

        buf = self.maybe_pad_tx(buf);
        debug!(
            "TX len={} (wMaxPacketSize={}, pad_pkts={})",
            buf.len(),
            self.out_wmax,
            self.pad_pkts.load(Ordering::Relaxed),
        );

        self.try_tx(frame, buf).await
    }

    async fn try_tx(&self, frame: &CanFrame, buf: Vec<u8>) -> io::Result<()> {
        match self.cmd_bulk_write(self.out_ep, buf.clone()).await {
            Ok(written) if written == buf.len() => Ok(()),

            // PAD_PKTS was enabled but device rejected it → disable and retry unpadded
            Err(e)
                if e.kind() == io::ErrorKind::BrokenPipe
                    && self.pad_pkts.load(Ordering::Relaxed) =>
            {
                log::warn!("TX stalled with PAD_PKTS enabled, disabling and retrying unpadded");
                self.pad_pkts.store(false, Ordering::Relaxed);

                // Re-encode frame without padding
                let fallback = if (self.features & GS_CAN_FEATURE_FD) != 0 {
                    self.encode_frame_tx_76(frame)
                } else {
                    self.encode_frame_minimal(frame)
                };

                // no padding this time
                debug!(
                    "TX fallback len={} (wMaxPacketSize={}, pad_pkts={})",
                    fallback.len(),
                    self.out_wmax,
                    self.pad_pkts.load(Ordering::Relaxed),
                );

                match self.cmd_bulk_write(self.out_ep, fallback.clone()).await {
                    Ok(written) if written == fallback.len() => Ok(()),
                    Ok(_) => Err(io::Error::new(
                        io::ErrorKind::Other,
                        "incomplete bulk write (fallback)",
                    )),
                    Err(e) => Err(e),
                }
            }

            Ok(_) => Err(io::Error::new(
                io::ErrorKind::Other,
                "incomplete bulk write",
            )),
            Err(e) => Err(e),
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
        if self.pad_pkts.load(Ordering::Relaxed) {
            flags |= GS_CAN_MODE_PAD_PKTS_TO_MAX_PKT_SIZE;
        }

        debug!(
            "Opening channel {}, iface={}, flags={:#010x}",
            self.channel_index, self.interface, flags
        );

        // Ensure we're cleanly in RESET before START (mirrors Linux gs_usb)
        self.cmd_control_out(
            request_type_out(),
            GS_USB_BREQ_MODE,
            self.channel_index as u16,
            self.interface as u16,
            encode_mode(GS_CAN_MODE_RESET, 0).to_vec(),
        )
        .await?;

        // START with final flags
        self.cmd_control_out(
            request_type_out(),
            GS_USB_BREQ_MODE,
            self.channel_index as u16,
            self.interface as u16,
            encode_mode(GS_CAN_MODE_START, flags).to_vec(),
        )
        .await?;

        Ok(())
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
        let res = self
            .cmd_control_out(
                request_type_out(),
                GS_USB_BREQ_TIMESTAMP,
                self.channel_index as u16,
                self.interface as u16,
                1u32.to_le_bytes().to_vec(),
            )
            .await;

        match res {
            Ok(_) => {
                info!("BREQ_TIMESTAMP accepted by device");
                self.timestamp_enabled = true;
                Ok(())
            }
            Err(e) if e.kind() == io::ErrorKind::TimedOut => {
                warn!("Device ignored BREQ_TIMESTAMP (timeout); falling back to MODE flag only");
                self.timestamp_enabled = true; // ensures MODE flag is set later
                Ok(())
            }
            Err(e) => {
                warn!("BREQ_TIMESTAMP not supported: {e}, falling back to MODE flag only");
                self.timestamp_enabled = true;
                Ok(())
            }
        }
    }

    async fn set_bitrate(&mut self, bitrate: u32) -> io::Result<()> {
        let bt = self
            .bt_const
            .as_ref()
            .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "BT_CONST not initialized"))?;

        let timing = calc_bit_timing(bitrate, bt).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("Unable to compute bit timing for bitrate {bitrate}"),
            )
        })?;

        debug!("Setting bitrate={} bps, timing={:?}", bitrate, timing);

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
        if self.configured_bitrate.is_none() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "set_bitrate() must be called before open_channel()",
            ));
        }
        self.open_channel_inner().await // false = not listen-only
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
