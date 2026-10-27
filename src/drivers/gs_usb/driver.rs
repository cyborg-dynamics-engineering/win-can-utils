use std::collections::VecDeque;
use std::future::Future;
use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::drivers::CanDriver;
use async_trait::async_trait;
use crosscan::can::CanFrame;
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore, mpsc, oneshot};
use tokio::time::Instant;

use super::bit_timing::{GsBtConst, calc_bit_timing, encode_mode, parse_bt_const};
use super::constants::*;
use super::context::{LibusbContext, LibusbDeviceHandle, PendingBulkWrite, map_libusb_error};
use super::device::{GsUsbAdapter, list_adapters, select_device};
use super::frames::parse_host_frame_at;

use log::{debug, info, warn};

const MAX_TX_IN_FLIGHT: usize = 10;

fn classic_tx_record_len(product: &str, features: u32) -> usize {
    // The tested DSD TECH SH-C31x firmware requires the full FD-sized transport
    // record even in classic mode. Other firmware follows the active CAN mode;
    // merely advertising FD support must not select a 76-byte classic record.
    if product.eq_ignore_ascii_case("SH-C31x") && features & GS_CAN_FEATURE_FD != 0 {
        GS_TX_FRAME_SIZE
    } else {
        GS_HEADER_LEN + 8
    }
}

struct TxSlot {
    echo_id: u32,
    usb_complete: bool,
    echoed: bool,
    _permit: OwnedSemaphorePermit,
}

struct TxState {
    pending: Vec<TxSlot>,
    next_echo_id: u32,
    failure: Option<(io::ErrorKind, String)>,
}

impl TxState {
    fn closed_error(&self) -> io::Error {
        match &self.failure {
            Some((kind, message)) => io::Error::new(*kind, message.clone()),
            None => io::Error::new(io::ErrorKind::NotConnected, "USB event loop closed"),
        }
    }
}

/// Retire a credit only after BOTH USB completion and the firmware echo.
struct TxSlots {
    capacity: Arc<Semaphore>,
    state: Mutex<TxState>,
}

/// Cancellation before USB submission returns the reserved credit automatically.
struct TxReservation {
    echo_id: u32,
    slots: Arc<TxSlots>,
    cancel_on_drop: bool,
}

impl TxReservation {
    fn submitted(mut self) -> u32 {
        self.cancel_on_drop = false;
        self.echo_id
    }
}

impl Drop for TxReservation {
    fn drop(&mut self) {
        if self.cancel_on_drop {
            self.slots.cancel(self.echo_id);
        }
    }
}

impl TxSlots {
    fn new() -> Self {
        Self {
            capacity: Arc::new(Semaphore::new(MAX_TX_IN_FLIGHT)),
            state: Mutex::new(TxState {
                pending: Vec::with_capacity(MAX_TX_IN_FLIGHT),
                next_echo_id: 0,
                failure: None,
            }),
        }
    }

    fn check(&self) -> io::Result<()> {
        let state = self.state.lock().unwrap();
        if self.capacity.is_closed() {
            Err(state.closed_error())
        } else {
            Ok(())
        }
    }

    async fn acquire(self: &Arc<Self>) -> io::Result<TxReservation> {
        self.check()?;
        let permit = tokio::time::timeout(
            Duration::from_secs(1),
            self.capacity.clone().acquire_owned(),
        )
        .await
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                "CAN transmit queue full: waiting for USB completion or firmware echo",
            )
        })?
        .map_err(|_| self.state.lock().unwrap().closed_error())?;
        let mut state = self.state.lock().unwrap();
        if self.capacity.is_closed() {
            return Err(state.closed_error());
        }
        // Echo IDs are opaque u32 values. Keep them distinct across RESET so a
        // late echo from the previous session cannot release a new frame.
        let echo_id = loop {
            let id = state.next_echo_id;
            state.next_echo_id = id.wrapping_add(1);
            if id != GS_CAN_ECHO_ID_UNUSED && !state.pending.iter().any(|s| s.echo_id == id) {
                break id;
            }
        };
        state.pending.push(TxSlot {
            echo_id,
            usb_complete: false,
            echoed: false,
            _permit: permit,
        });
        Ok(TxReservation {
            echo_id,
            slots: self.clone(),
            cancel_on_drop: true,
        })
    }

    fn complete_part(&self, echo_id: u32, usb: bool) {
        let mut state = self.state.lock().unwrap();
        if let Some(index) = state.pending.iter().position(|s| s.echo_id == echo_id) {
            let slot = &mut state.pending[index];
            if usb {
                slot.usb_complete = true;
            } else {
                slot.echoed = true;
            }
            if slot.usb_complete && slot.echoed {
                state.pending.swap_remove(index);
            }
        }
    }

    fn cancel(&self, echo_id: u32) {
        let mut state = self.state.lock().unwrap();
        if let Some(index) = state.pending.iter().position(|s| s.echo_id == echo_id) {
            state.pending.swap_remove(index);
        }
    }

    fn reset(&self) {
        self.state.lock().unwrap().pending.clear();
    }

    fn close(&self, error: Option<&io::Error>) {
        let mut state = self.state.lock().unwrap();
        if state.failure.is_none() {
            state.failure = error.map(|e| (e.kind(), e.to_string()));
        }
        self.capacity.close();
        state.pending.clear();
    }
}

struct QueuedWrite<F> {
    echo_id: u32,
    expected_len: usize,
    completion: F,
}

/// Submission is eager and ordered. Callback wake order cannot reorder frames.
struct TxQueue<F> {
    writes: VecDeque<QueuedWrite<F>>,
    slots: Arc<TxSlots>,
}

impl<F: Future<Output = io::Result<usize>> + Unpin> TxQueue<F> {
    fn new(slots: Arc<TxSlots>) -> Self {
        Self {
            writes: VecDeque::with_capacity(MAX_TX_IN_FLIGHT),
            slots,
        }
    }

    fn push(&mut self, echo_id: u32, expected_len: usize, completion: F) {
        assert!(self.writes.len() < MAX_TX_IN_FLIGHT);
        self.writes.push_back(QueuedWrite {
            echo_id,
            expected_len,
            completion,
        });
    }

    async fn complete_next(&mut self) -> io::Result<()> {
        let Some(front) = self.writes.front_mut() else {
            return std::future::pending().await;
        };
        let result = (&mut front.completion).await;
        let write = self.writes.pop_front().unwrap();
        let result = result.and_then(|written| {
            if written == write.expected_len {
                Ok(())
            } else {
                Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    format!(
                        "Incomplete USB CAN write: {written}/{} bytes",
                        write.expected_len
                    ),
                ))
            }
        });
        match result {
            Ok(()) => {
                self.slots.complete_part(write.echo_id, true);
                Ok(())
            }
            Err(error) => {
                // Do not retry a possibly partial frame or hide a queued error.
                self.slots.close(Some(&error));
                Err(error)
            }
        }
    }

    async fn drain(&mut self) -> io::Result<()> {
        while !self.writes.is_empty() {
            self.complete_next().await?;
        }
        Ok(())
    }
}

/// State owned by the dedicated USB thread.
///
/// Serializes device commands and parsing. A separate libusb event thread
/// completes the pending transfers and wakes this thread when data arrives.
struct UsbEventLoop {
    handle: LibusbDeviceHandle,
    in_ep: u8,
    _out_ep: u8,
    cmd_rx: mpsc::Receiver<UsbCommand>,
    frame_tx: mpsc::Sender<CanFrame>,
    rx_ready: Arc<Notify>,
    tx_slots: Arc<TxSlots>,
    tx_queue: TxQueue<PendingBulkWrite>,
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
        in_ep: u8,
        out_ep: u8,
        cmd_rx: mpsc::Receiver<UsbCommand>,
        frame_tx: mpsc::Sender<CanFrame>,
        rx_ready: Arc<Notify>,
        tx_slots: Arc<TxSlots>,
        out_wmax: usize,
        pad_pkts_enabled: bool,
    ) -> Self {
        Self {
            handle,
            in_ep,
            _out_ep: out_ep,
            cmd_rx,
            frame_tx,
            rx_ready,
            tx_queue: TxQueue::new(tx_slots.clone()),
            tx_slots,
            rx_buffer: Vec::with_capacity(GS_MAX_FRAME_LEN * 4),
            last_timestamp64: None,
            channel_index: 0,
            timestamp_enabled: false,
            out_wmax,
            pad_pkts_enabled,
        }
    }

    const NUM_RX_TRANSFERS: usize = 64;

    async fn run(mut self) -> io::Result<()> {
        let mut inflight = VecDeque::with_capacity(Self::NUM_RX_TRANSFERS);
        for _ in 0..Self::NUM_RX_TRANSFERS {
            inflight.push_back(self.handle.submit_bulk_read(
                self.in_ep,
                self.out_wmax.max(GS_HEADER_LEN),
                Duration::ZERO,
            )?);
        }

        // Track completion times so the debug output includes spacing between packets.
        let mut last_rx = Instant::now();

        loop {
            tokio::select! {
                maybe_cmd = self.cmd_rx.recv() => {
                    let Some(command) = maybe_cmd else { return Ok(()); };
                    if !self.handle_command(command).await? { return Ok(()); }
                }

                result = self.tx_queue.complete_next() => { result?; }

                // Transfers are submitted eagerly and consumed in submission
                // order. Completion wake order need not match USB byte order.
                result = inflight.front_mut().expect("RX pipeline is nonempty") => {
                    inflight.pop_front();
                    let now = Instant::now();
                    let delta = now.duration_since(last_rx);
                    last_rx = now;

                    if let Err(error) = &result {
                        // A disconnected device must end reception, rather than
                        // repeatedly submitting reads that fail immediately.
                        if error.kind() != io::ErrorKind::BrokenPipe {
                            return Err(io::Error::new(error.kind(), error.to_string()));
                        }
                        self.handle.clear_halt(self.in_ep)?;
                        self.rx_buffer.clear();
                    }

                    // Rearm before parsing or waiting for downstream capacity.
                    inflight.push_back(self.handle.submit_bulk_read(
                        self.in_ep,
                        self.out_wmax.max(GS_HEADER_LEN),
                        Duration::ZERO,
                    )?);
                    if let Ok(buf) = result {
                        debug!("RX transfer completed: {} bytes, spacing={:?}", buf.len(), delta);
                        log::trace!("RX bytes: {:02x?}", buf);
                        self.process_rx_chunk(&buf).await?;
                    }
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
                self.tx_queue.drain().await?;
                let result = self.handle.control_out_blocking(
                    request_type,
                    request,
                    value,
                    index,
                    &data,
                    USB_TIMEOUT,
                );

                if result.is_ok() && request == GS_USB_BREQ_MODE && data.len() >= 8 {
                    self.channel_index = value as u8;
                    let flags = u32::from_le_bytes(data[4..8].try_into().unwrap());
                    self.timestamp_enabled = flags & GS_CAN_MODE_HW_TIMESTAMP != 0;
                    self.pad_pkts_enabled = flags & GS_CAN_MODE_PAD_PKTS_TO_MAX_PKT_SIZE != 0;
                    if u32::from_le_bytes(data[..4].try_into().unwrap()) == GS_CAN_MODE_RESET {
                        self.rx_buffer.clear();
                        self.last_timestamp64 = None;
                        self.tx_slots.reset();
                    }
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
                self.tx_queue.drain().await?;
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
                reservation,
                resp,
            } => {
                let expected_len = data.len();
                match self.handle.submit_bulk_write(endpoint, data, USB_TIMEOUT) {
                    Ok(completion) => {
                        self.tx_queue
                            .push(reservation.submitted(), expected_len, completion);
                        // Acknowledge submission, allowing the next frame to be
                        // submitted while this transfer is still in flight.
                        let _ = resp.send(Ok(()));
                    }
                    Err(error) => {
                        let _ = resp.send(Err(io::Error::new(error.kind(), error.to_string())));
                        return Err(error);
                    }
                }
                Ok(true)
            }
            UsbCommand::Shutdown {} => Ok(false),
        }
    }

    async fn process_rx_chunk(&mut self, chunk: &[u8]) -> io::Result<()> {
        if chunk.is_empty() {
            return Ok(());
        }

        self.rx_buffer.extend_from_slice(chunk);

        while self.rx_buffer.len() >= GS_HEADER_LEN {
            let slice = &self.rx_buffer[..];
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
                    if consumed > 1 {
                        if slice[10] & GS_CAN_FLAG_OVERFLOW != 0 {
                            warn!("Adapter reports CAN receive overflow");
                        }
                        let echo_id = u32::from_le_bytes(slice[..4].try_into().unwrap());
                        if echo_id != GS_CAN_ECHO_ID_UNUSED {
                            self.tx_slots.complete_part(echo_id, false);
                        }
                    }
                    // Consume before awaiting capacity: a RESET command handled
                    // during backpressure can safely clear the remaining buffer.
                    self.rx_buffer.drain(..consumed);
                    if let Some(frame) = maybe_frame {
                        self.forward_frame(frame).await?;
                    }
                }
            }
        }

        Ok(())
    }

    async fn forward_frame(&mut self, frame: CanFrame) -> io::Result<()> {
        let frame_tx = self.frame_tx.clone();
        let send = frame_tx.send(frame);
        tokio::pin!(send);
        loop {
            tokio::select! {
                result = &mut send => {
                    result.map_err(|_| io::Error::new(
                        io::ErrorKind::BrokenPipe, "CAN frame receiver closed",
                    ))?;
                    self.rx_ready.notify_one();
                    return Ok(());
                }
                command = self.cmd_rx.recv() => {
                    // A slow or absent pipe reader must not prevent transmit,
                    // control commands, or shutdown from being serviced.
                    let keep_running = match command {
                        Some(command) => self.handle_command(command).await?,
                        None => false,
                    };
                    if !keep_running {
                        return Err(io::Error::new(io::ErrorKind::Interrupted, "USB event loop stopped"));
                    }
                }
                result = self.tx_queue.complete_next() => { result?; }
            }
        }
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
        reservation: TxReservation,
        resp: oneshot::Sender<io::Result<()>>,
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
    /// Echo identifiers remain reserved until firmware confirms transmission.
    tx_slots: Arc<TxSlots>,

    /// Feature bitmask returned by BT_CONST/BT_CONST_EXT.
    features: u32,
    /// Maximum packet size supported by the OUT endpoint.
    out_wmax: u16,
    /// Flag toggled when PAD_PKTS handshake succeeds.
    pad_pkts: Arc<AtomicBool>,

    /// Receiver half of the frame channel bridging the USB thread.
    frame_rx: Arc<Mutex<mpsc::Receiver<CanFrame>>>,
    rx_ready: Arc<Notify>,
    /// Command channel to the dedicated USB event loop thread.
    cmd_tx: mpsc::Sender<UsbCommand>,

    /// Cached BT_CONST descriptor for reference when changing bit timings.
    bt_const: Option<GsBtConst>,
}

impl GsUsbDriver {
    /// Probe connected adapter interfaces without changing their CAN configuration.
    /// Busy, inaccessible and unsupported interfaces remain in the returned list.
    pub fn list_adapters() -> io::Result<Vec<GsUsbAdapter>> {
        let context = LibusbContext::new()?;
        list_adapters(&context)
    }

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
        let rx_ready = Arc::new(Notify::new());
        let tx_slots = Arc::new(TxSlots::new());

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
            tx_slots: tx_slots.clone(),

            features: 0,
            out_wmax: info.out_wmax,
            pad_pkts: Arc::new(AtomicBool::new(false)),

            frame_rx: Arc::new(Mutex::new(frame_rx)),
            rx_ready: rx_ready.clone(),
            cmd_tx: cmd_tx.clone(),

            bt_const: None,
        };

        // Own device commands on one thread; libusb completes asynchronous reads
        // on its separate event thread.
        std::thread::spawn(move || {
            let notify_on_exit = rx_ready.clone();
            let close_on_exit = tx_slots.clone();
            let result = catch_unwind(AssertUnwindSafe(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("tokio runtime");

                let event_loop = UsbEventLoop::new(
                    handle,
                    info.in_ep,
                    info.out_ep,
                    cmd_rx,
                    frame_tx,
                    rx_ready,
                    tx_slots,
                    info.out_wmax as usize,
                    false,
                );
                runtime.block_on(event_loop.run())
            }));
            match result {
                Ok(Err(error)) => {
                    close_on_exit.close(Some(&error));
                    log::error!("USB event loop stopped: {error}");
                }
                Err(_) => {
                    close_on_exit.close(Some(&io::Error::other("USB event loop panicked")));
                    log::error!("USB event loop panicked");
                }
                Ok(Ok(())) => close_on_exit.close(None),
            }
            // The frame sender is now dropped; wake an idle reader so it can
            // observe disconnection instead of waiting forever.
            notify_on_exit.notify_one();
        });

        // Perform the handshake and discover optional firmware capabilities.
        driver.send_host_format().await?;

        let bt = driver.read_bt_const().await?;
        driver.features = bt.feature;
        driver.bt_const = Some(bt);
        driver.out_wmax = info.out_wmax;

        let _dev_conf = driver.read_device_config().await?;
        // BT_CONST already supplied the feature mask. Re-reading only its first
        // four bytes is rejected by firmware that requires full descriptor sizes.
        let features = driver.features;

        // Let the USB thread know about negotiated padding behaviour.
        let _ = driver
            .cmd_tx
            .send(UsbCommand::UpdateConfig {
                // FD-capable firmware pads the full 80-byte receive union,
                // including classic frames, to 128 bytes on a 64-byte endpoint.
                out_wmax: if features & GS_CAN_FEATURE_FD != 0 {
                    GS_MAX_FRAME_LEN.div_ceil(info.out_wmax as usize) * info.out_wmax as usize
                } else {
                    info.out_wmax as usize
                },
                pad_pkts_enabled: (features & GS_CAN_FEATURE_PAD_PKTS_TO_MAX_PKT_SIZE) != 0
                    && features & GS_CAN_FEATURE_FD == 0,
            })
            .await;

        driver.out_wmax = info.out_wmax;

        driver.pad_pkts = Arc::new(AtomicBool::new(
            // On the SH-C31A this mode expands every classic frame to 128
            // bytes. Compact records avoid its high-rate receive bottleneck.
            (features & GS_CAN_FEATURE_PAD_PKTS_TO_MAX_PKT_SIZE) != 0
                && features & GS_CAN_FEATURE_FD == 0,
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

    async fn cmd_bulk_write(
        &self,
        endpoint: u8,
        data: Vec<u8>,
        reservation: TxReservation,
    ) -> io::Result<()> {
        let (resp_tx, resp_rx) = oneshot::channel();
        self.cmd_tx
            .send(UsbCommand::BulkWrite {
                endpoint,
                data,
                reservation,
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

    async fn read_device_config(&self) -> io::Result<[u8; 12]> {
        // DEVICE_CONFIG
        let buf = self
            .cmd_control_in(
                request_type_in(),
                GS_USB_BREQ_DEVICE_CONFIG,
                1,                     // value = 1
                self.interface as u16, // index = interface
                12,                    // reserved[3], channel count, software and hardware versions
            )
            .await?;

        if buf.len() < 12 {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                format!("device_config too short: {} bytes", buf.len()),
            ));
        }

        let mut arr = [0u8; 12];
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
                72, // ten nominal timing fields and eight data timing fields
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

    fn encode_frame_minimal(&self, frame: &CanFrame, echo_id: u32) -> Vec<u8> {
        let mut buf = vec![0u8; 20]; // 12 header + 8 data

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

        buf
    }

    async fn send_frame(&mut self, frame: &CanFrame) -> io::Result<()> {
        let reservation = self.tx_slots.acquire().await?;
        let mut buf = self.encode_frame_minimal(frame, reservation.echo_id);
        buf.resize(classic_tx_record_len(&self.device_label, self.features), 0);
        // PAD_PKTS applies to incoming records, not bulk OUT writes.
        debug!(
            "TX len={} (wMaxPacketSize={}, pad_pkts={})",
            buf.len(),
            self.out_wmax,
            self.pad_pkts.load(Ordering::Relaxed),
        );

        self.cmd_bulk_write(self.out_ep, buf, reservation).await
    }

    async fn read_frames(&mut self) -> io::Result<Vec<CanFrame>> {
        let mut frames = Vec::new();
        let mut rx = self
            .frame_rx
            .lock()
            .map_err(|_| io::Error::other("CAN frame receiver lock poisoned"))?;
        // Bound each batch even if a producer keeps replenishing the queue.
        for _ in 0..1024 {
            match rx.try_recv() {
                Ok(frame) => frames.push(frame),
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    if frames.is_empty() {
                        self.tx_slots.check()?;
                        return Err(io::Error::new(
                            io::ErrorKind::NotConnected,
                            "USB event loop closed",
                        ));
                    }
                    break;
                }
            }
        }
        if frames.is_empty() {
            self.tx_slots.check()?;
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
    fn capabilities(&self) -> crosscan::protocol::Capabilities {
        crosscan::protocol::Capabilities {
            hardware_can_fd: Some(self.features & GS_CAN_FEATURE_FD != 0),
            can_fd: false,
            hardware_timestamps: Some(self.features & GS_CAN_FEATURE_HW_TIMESTAMP != 0),
            bus_state: self.features & GS_CAN_FEATURE_GET_STATE != 0,
            software_tx_echo: true,
        }
    }

    async fn health(&mut self) -> io::Result<crate::drivers::can_driver::DriverHealth> {
        use crate::drivers::can_driver::DriverHealth;
        use crosscan::protocol::BusState;
        self.tx_slots.check()?;
        if self.features & GS_CAN_FEATURE_GET_STATE == 0 {
            return Ok(DriverHealth::default());
        }
        let bytes = self
            .cmd_control_in(
                request_type_in(),
                _GS_USB_BREQ_GET_STATE,
                self.channel_index as u16,
                self.interface as u16,
                12,
            )
            .await?;
        if bytes.len() != 12 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Short gs_usb GET_STATE response",
            ));
        }
        Ok(DriverHealth {
            bus_state: match u32::from_le_bytes(bytes[..4].try_into().unwrap()) {
                0 => BusState::ErrorActive,
                1 => BusState::ErrorWarning,
                2 => BusState::ErrorPassive,
                3 => BusState::BusOff,
                4 => BusState::Stopped,
                _ => BusState::Unknown,
            },
            rx_error_counter: Some(u32::from_le_bytes(bytes[4..8].try_into().unwrap())),
            tx_error_counter: Some(u32::from_le_bytes(bytes[8..12].try_into().unwrap())),
        })
    }

    fn receive_notifier(&self) -> Option<Arc<Notify>> {
        Some(self.rx_ready.clone())
    }

    async fn enable_timestamp(&mut self) -> io::Result<()> {
        // Timestamping is enabled by MODE START. BREQ_TIMESTAMP is an IN
        // request for the current clock, not an OUT enable command.
        self.timestamp_enabled = self.features & GS_CAN_FEATURE_HW_TIMESTAMP != 0;
        Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fd_capability_does_not_select_fd_transport_in_classic_mode() {
        assert_eq!(classic_tx_record_len("Cyder CANable gs_usb", 0x2503), 20);
        assert_eq!(classic_tx_record_len("canable gs_usb", 0xf3), 20);
        assert_eq!(
            classic_tx_record_len("other FD adapter", GS_CAN_FEATURE_FD),
            20
        );
    }

    #[test]
    fn factory_sh_c31x_keeps_tested_transport_quirk() {
        assert_eq!(classic_tx_record_len("SH-C31x", 0x25fb), 76);
        assert_eq!(classic_tx_record_len("SH-C31x", 0), 20);
    }

    #[tokio::test]
    async fn credits_wait_for_both_usb_completion_and_echo_in_either_order() {
        for usb_first in [false, true] {
            let slots = Arc::new(TxSlots::new());
            for expected in 0..MAX_TX_IN_FLIGHT as u32 {
                assert_eq!(slots.acquire().await.unwrap().submitted(), expected);
            }
            let waiting_slots = slots.clone();
            let waiting =
                tokio::spawn(
                    async move { waiting_slots.acquire().await.map(TxReservation::submitted) },
                );
            tokio::task::yield_now().await;
            assert!(!waiting.is_finished());
            slots.complete_part(3, usb_first);
            tokio::task::yield_now().await;
            assert!(
                !waiting.is_finished(),
                "A partial completion returned a credit"
            );
            slots.complete_part(3, !usb_first);
            assert_eq!(waiting.await.unwrap().unwrap(), MAX_TX_IN_FLIGHT as u32);
            slots.complete_part(3, true);
            slots.complete_part(3, false);
            slots.complete_part(999, false);
            assert_eq!(slots.capacity.available_permits(), 0);
        }
    }

    #[tokio::test]
    async fn usb_shutdown_wakes_a_sender_waiting_for_echo() {
        let slots = Arc::new(TxSlots::new());
        for _ in 0..10 {
            slots.acquire().await.unwrap().submitted();
        }
        let waiting_slots = slots.clone();
        let waiting =
            tokio::spawn(
                async move { waiting_slots.acquire().await.map(TxReservation::submitted) },
            );
        tokio::task::yield_now().await;
        slots.close(None);
        assert_eq!(
            waiting.await.unwrap().unwrap_err().kind(),
            io::ErrorKind::NotConnected
        );
    }

    #[tokio::test]
    async fn cancelled_reservations_and_unprocessed_commands_return_credits() {
        let slots = Arc::new(TxSlots::new());
        let reservation = slots.acquire().await.unwrap();
        assert_eq!(slots.capacity.available_permits(), MAX_TX_IN_FLIGHT - 1);
        drop(reservation);
        assert_eq!(slots.capacity.available_permits(), MAX_TX_IN_FLIGHT);
        let (sender, receiver) = mpsc::channel(1);
        let (response, _result) = oneshot::channel();
        sender
            .send(UsbCommand::BulkWrite {
                endpoint: 2,
                data: vec![0; 20],
                reservation: slots.acquire().await.unwrap(),
                resp: response,
            })
            .await
            .ok()
            .unwrap();
        drop(receiver);
        assert_eq!(slots.capacity.available_permits(), MAX_TX_IN_FLIGHT);
    }

    #[tokio::test]
    async fn stale_echo_after_reset_and_reserved_echo_id_cannot_release_new_frame() {
        let slots = Arc::new(TxSlots::new());
        let old = slots.acquire().await.unwrap().submitted();
        slots.complete_part(old, true);
        slots.reset();
        let new = slots.acquire().await.unwrap().submitted();
        assert_ne!(old, new);
        slots.complete_part(new, true);
        slots.complete_part(old, false);
        assert_eq!(slots.capacity.available_permits(), MAX_TX_IN_FLIGHT - 1);
        slots.complete_part(new, false);
        let zero = slots.acquire().await.unwrap().submitted();
        slots.state.lock().unwrap().next_echo_id = GS_CAN_ECHO_ID_UNUSED;
        let wrapped = slots.acquire().await.unwrap().submitted();
        assert_ne!(wrapped, GS_CAN_ECHO_ID_UNUSED);
        assert_ne!(wrapped, zero);
    }

    type TestWrite = std::pin::Pin<Box<dyn Future<Output = io::Result<usize>> + Send>>;

    #[tokio::test]
    async fn writes_overlap_but_retire_in_submission_order_and_drain_before_control() {
        let slots = Arc::new(TxSlots::new());
        let mut queue = TxQueue::<TestWrite>::new(slots.clone());
        let completed = Arc::new(Mutex::new(Vec::new()));
        let mut senders = Vec::new();
        for id in 0..3 {
            let echo_id = slots.acquire().await.unwrap().submitted();
            slots.complete_part(echo_id, false);
            let (sender, receiver) = oneshot::channel::<io::Result<usize>>();
            senders.push(sender);
            let completed = completed.clone();
            queue.push(
                echo_id,
                20,
                Box::pin(async move {
                    let result = receiver.await.unwrap();
                    completed.lock().unwrap().push(id);
                    result
                }),
            );
        }
        // Three USB requests have been submitted before any has completed.
        assert_eq!(queue.writes.len(), 3);
        senders.pop().unwrap().send(Ok(20)).unwrap();
        senders.pop().unwrap().send(Ok(20)).unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(10), queue.drain())
                .await
                .is_err()
        );
        assert!(completed.lock().unwrap().is_empty());
        assert_eq!(queue.writes.len(), 3);
        senders.pop().unwrap().send(Ok(20)).unwrap();
        queue.drain().await.unwrap(); // The control operation can now proceed.
        assert_eq!(*completed.lock().unwrap(), vec![0, 1, 2]);
        assert_eq!(slots.capacity.available_permits(), MAX_TX_IN_FLIGHT);
    }

    #[tokio::test]
    async fn short_or_failed_async_write_closes_queue_and_wakes_blocked_sender() {
        for outcome in [
            Ok(19),
            Err(io::Error::new(io::ErrorKind::NotConnected, "unplugged")),
        ] {
            let slots = Arc::new(TxSlots::new());
            let mut queue = TxQueue::<TestWrite>::new(slots.clone());
            let first = slots.acquire().await.unwrap().submitted();
            for _ in 1..MAX_TX_IN_FLIGHT {
                slots.acquire().await.unwrap().submitted();
            }
            queue.push(first, 20, Box::pin(async move { outcome }));
            let waiting_slots = slots.clone();
            let waiting =
                tokio::spawn(
                    async move { waiting_slots.acquire().await.map(TxReservation::submitted) },
                );
            tokio::task::yield_now().await;
            assert!(!waiting.is_finished());
            let error = queue.complete_next().await.unwrap_err();
            assert_eq!(slots.check().unwrap_err().kind(), error.kind());
            assert_eq!(waiting.await.unwrap().unwrap_err().kind(), error.kind());
        }
    }
}
