#![allow(dead_code)]

use std::ffi::CStr;
use std::future::Future;
use std::io;
use std::mem::MaybeUninit;
use std::os::raw::{c_int, c_uint, c_void};
use std::pin::Pin;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::task::{Context, Poll};
use std::thread::JoinHandle;
use std::time::Duration;

use libusb1_sys as libusb;
use libusb1_sys::constants::{
    LIBUSB_CONTROL_SETUP_SIZE, LIBUSB_ERROR_ACCESS, LIBUSB_ERROR_BUSY, LIBUSB_ERROR_INTERRUPTED,
    LIBUSB_ERROR_NO_DEVICE, LIBUSB_ERROR_NOT_FOUND, LIBUSB_ERROR_NOT_SUPPORTED, LIBUSB_ERROR_PIPE,
    LIBUSB_ERROR_TIMEOUT, LIBUSB_TRANSFER_CANCELLED, LIBUSB_TRANSFER_COMPLETED,
    LIBUSB_TRANSFER_ERROR, LIBUSB_TRANSFER_NO_DEVICE, LIBUSB_TRANSFER_OVERFLOW,
    LIBUSB_TRANSFER_STALL, LIBUSB_TRANSFER_TIMED_OUT, LIBUSB_TRANSFER_TYPE_BULK,
    LIBUSB_TRANSFER_TYPE_CONTROL,
};
use log::{debug, error, warn};
use tokio::sync::oneshot;

use super::constants::duration_to_timeout;
use crate::instance_lock::InstanceLock;

#[derive(Copy, Clone)]
pub(crate) struct LibusbCtxPtr(pub(crate) *mut libusb::libusb_context);

unsafe impl Send for LibusbCtxPtr {}
unsafe impl Sync for LibusbCtxPtr {}

/// RAII wrapper owning a libusb context and a background event thread.
pub(crate) struct LibusbContext {
    pub(crate) ptr: LibusbCtxPtr,
    running: Arc<AtomicBool>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl LibusbContext {
    pub(crate) fn new() -> io::Result<Arc<Self>> {
        let mut ctx = ptr::null_mut();
        let rc = unsafe { libusb::libusb_init(&mut ctx) };
        if rc < 0 {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                format!("libusb init failed: {rc}"),
            ));
        }

        let ctx_ptr = Arc::new(LibusbCtxPtr(ctx));
        let running = Arc::new(AtomicBool::new(true));
        let running_thread = running.clone();
        let ctx_for_thread = ctx_ptr.clone();

        // libusb blocks on OS events and wakes immediately on USB completion.
        // A long maximum wait does not add receive latency.
        let handle = std::thread::Builder::new()
            .name("libusb-events".into())
            .spawn(move || {
                while running_thread.load(Ordering::SeqCst) {
                    let timeval = libc::timeval {
                        tv_sec: 1,
                        tv_usec: 0,
                    };
                    let rc = unsafe {
                        libusb::libusb_handle_events_timeout_completed(
                            ctx_for_thread.0,
                            &timeval,
                            ptr::null_mut(),
                        )
                    };
                    if rc == libusb::constants::LIBUSB_ERROR_INTERRUPTED {
                        continue;
                    }
                    if rc < 0 && running_thread.load(Ordering::SeqCst) {
                        error!("libusb event handling failed: {}", libusb_error_string(rc));
                        std::thread::sleep(Duration::from_millis(10));
                    }
                }
            })
            .map_err(|e| {
                unsafe { libusb::libusb_exit(ctx) };
                io::Error::new(
                    io::ErrorKind::Other,
                    format!("Failed to spawn libusb event thread: {e}"),
                )
            })?;

        Ok(Arc::new(LibusbContext {
            ptr: *ctx_ptr,
            running,
            thread: Mutex::new(Some(handle)),
        }))
    }
}

impl Drop for LibusbContext {
    fn drop(&mut self) {
        self.running.store(false, Ordering::SeqCst);

        unsafe {
            libusb::libusb_interrupt_event_handler(self.ptr.0);
        }

        if let Ok(mut guard) = self.thread.lock() {
            if let Some(handle) = guard.take() {
                let _ = handle.join();
            }
        }

        unsafe { libusb::libusb_exit(self.ptr.0) };
    }
}

/// Wrapper around a libusb device handle, adding async helpers and automatic close semantics.
#[derive(Clone)]
pub(crate) struct LibusbDeviceHandle {
    // Fields drop in declaration order: close the handle before its context.
    handle: Arc<LibusbHandleWrapper>,
    pub(crate) _context: Arc<LibusbContext>,
}

struct LibusbHandleWrapper {
    ptr: *mut libusb::libusb_device_handle,
    claimed: Mutex<Vec<i32>>,
    _ownership: InstanceLock,
}

unsafe impl Send for LibusbHandleWrapper {}
unsafe impl Sync for LibusbHandleWrapper {}

impl Drop for LibusbHandleWrapper {
    fn drop(&mut self) {
        unsafe {
            for interface in self.claimed.get_mut().unwrap().drain(..) {
                libusb::libusb_release_interface(self.ptr, interface);
            }
            libusb::libusb_close(self.ptr);
        }
    }
}

impl LibusbDeviceHandle {
    pub(crate) fn open(
        context: Arc<LibusbContext>,
        device: *mut libusb::libusb_device,
        ownership: InstanceLock,
    ) -> io::Result<Self> {
        let mut handle = ptr::null_mut();
        let rc = unsafe { libusb::libusb_open(device, &mut handle) };
        if rc < 0 {
            return Err(map_libusb_error(rc));
        }
        Ok(Self {
            _context: context,
            handle: Arc::new(LibusbHandleWrapper {
                ptr: handle,
                claimed: Mutex::new(Vec::new()),
                _ownership: ownership,
            }),
        })
    }

    pub(crate) fn raw(&self) -> *mut libusb::libusb_device_handle {
        self.handle.ptr
    }

    pub(crate) fn set_auto_detach_kernel_driver(&self, enable: bool) -> io::Result<()> {
        let flag = if enable { 1 } else { 0 };
        let rc = unsafe { libusb::libusb_set_auto_detach_kernel_driver(self.handle.ptr, flag) };
        if rc < 0 && rc != LIBUSB_ERROR_NOT_SUPPORTED {
            return Err(map_libusb_error(rc));
        }
        Ok(())
    }

    pub(crate) fn claim_interface(&self, interface: i32) -> io::Result<()> {
        let mut claimed = self.handle.claimed.lock().unwrap();
        if claimed.contains(&interface) {
            return Ok(());
        }
        let rc = unsafe { libusb::libusb_claim_interface(self.handle.ptr, interface) };
        if rc < 0 {
            return Err(map_libusb_error(rc));
        }
        claimed.push(interface);
        Ok(())
    }

    pub(crate) fn clear_halt(&self, endpoint: u8) -> io::Result<()> {
        let rc = unsafe { libusb::libusb_clear_halt(self.handle.ptr, endpoint) };
        if rc < 0 {
            return Err(map_libusb_error(rc));
        }
        Ok(())
    }

    /// Submit immediately; the returned future owns cancellation and buffer lifetime.
    pub(crate) fn submit_bulk_write(
        &self,
        endpoint: u8,
        data: Vec<u8>,
        timeout: Duration,
    ) -> io::Result<PendingBulkWrite> {
        let (sender, receiver) = oneshot::channel();
        let completion = Arc::new(TransferCompletion {
            transfer: Mutex::new(ptr::null_mut()),
            finished: Condvar::new(),
        });
        let state = Box::new(BulkWriteState {
            sender: Some(sender),
            buffer: Some(data),
            completion: completion.clone(),
        });
        let state_ptr = Box::into_raw(state);
        let transfer = unsafe { libusb::libusb_alloc_transfer(0) };
        if transfer.is_null() {
            unsafe {
                let _ = Box::from_raw(state_ptr);
            }
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "Failed to allocate libusb transfer",
            ));
        }
        unsafe {
            (*transfer).dev_handle = self.handle.ptr;
            (*transfer).endpoint = endpoint;
            (*transfer).transfer_type = LIBUSB_TRANSFER_TYPE_BULK;
            (*transfer).timeout = duration_to_timeout(timeout);
            (*transfer).callback = bulk_write_callback;
            (*transfer).user_data = state_ptr as *mut c_void;
            if let Some(buffer) = (&mut *state_ptr).buffer.as_mut() {
                (*transfer).buffer = buffer.as_mut_ptr();
                (*transfer).length = buffer.len() as c_int;
            }
        }
        *completion.transfer.lock().unwrap() = transfer;
        let submit = unsafe { libusb::libusb_submit_transfer(transfer) };
        if submit < 0 {
            unsafe {
                let _ = Box::from_raw(state_ptr);
                libusb::libusb_free_transfer(transfer);
            }
            return Err(map_libusb_error(submit));
        }
        Ok(PendingTransfer {
            receiver,
            completion,
            _handle: self.clone(),
        })
    }

    #[allow(dead_code)]
    pub(crate) async fn control_in(
        &self,
        request_type: u8,
        request: u8,
        value: u16,
        index: u16,
        buf: &mut [u8],
        timeout: Duration,
    ) -> io::Result<usize> {
        let millis = duration_to_timeout(timeout) as u32;
        // Safe: libusb takes mutable pointer + len
        let res = unsafe {
            libusb1_sys::libusb_control_transfer(
                self.handle.ptr,
                request_type,
                request,
                value,
                index,
                buf.as_mut_ptr(),
                buf.len() as u16,
                millis,
            )
        };
        if res < 0 {
            Err(io::Error::new(
                io::ErrorKind::Other,
                format!("libusb control_in error {}", res),
            ))
        } else {
            Ok(res as usize)
        }
    }

    /// Submit immediately, keeping the device alive until completion or cancellation.
    pub(crate) fn submit_bulk_read(
        &self,
        endpoint: u8,
        length: usize,
        timeout: Duration,
    ) -> io::Result<PendingBulkRead> {
        let (sender, receiver) = oneshot::channel();
        let completion = Arc::new(TransferCompletion {
            transfer: Mutex::new(ptr::null_mut()),
            finished: Condvar::new(),
        });
        let state = Box::new(BulkReadState {
            sender: Some(sender),
            buffer: Some(vec![0u8; length]),
            completion: completion.clone(),
        });
        let state_ptr = Box::into_raw(state);
        let transfer = unsafe { libusb::libusb_alloc_transfer(0) };
        if transfer.is_null() {
            unsafe {
                let _ = Box::from_raw(state_ptr);
            }
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "Failed to allocate libusb transfer",
            ));
        }
        unsafe {
            (*transfer).dev_handle = self.handle.ptr;
            (*transfer).endpoint = endpoint;
            (*transfer).transfer_type = LIBUSB_TRANSFER_TYPE_BULK;
            (*transfer).timeout = duration_to_timeout(timeout);
            (*transfer).callback = bulk_read_callback;
            (*transfer).user_data = state_ptr as *mut c_void;
            if let Some(buffer) = (&mut *state_ptr).buffer.as_mut() {
                (*transfer).buffer = buffer.as_mut_ptr();
                (*transfer).length = buffer.len() as c_int;
            }
        }
        *completion.transfer.lock().unwrap() = transfer;
        let submit = unsafe { libusb::libusb_submit_transfer(transfer) };
        if submit < 0 {
            unsafe {
                let _ = Box::from_raw(state_ptr);
                libusb::libusb_free_transfer(transfer);
            }
            return Err(map_libusb_error(submit));
        }
        Ok(PendingTransfer {
            receiver,
            completion,
            _handle: self.clone(),
        })
    }

    #[allow(dead_code)]
    pub(crate) async fn control_out(
        &self,
        request_type: u8,
        request: u8,
        value: u16,
        index: u16,
        data: Vec<u8>,
        timeout: Duration,
    ) -> io::Result<usize> {
        let (sender, receiver) = oneshot::channel();
        let mut buffer = vec![0u8; LIBUSB_CONTROL_SETUP_SIZE + data.len()];
        unsafe {
            libusb::libusb_fill_control_setup(
                buffer.as_mut_ptr(),
                request_type,
                request,
                value,
                index,
                data.len() as u16,
            );
        }
        buffer[LIBUSB_CONTROL_SETUP_SIZE..].copy_from_slice(&data);
        let state = Box::new(ControlTransferState {
            sender: Some(sender),
            buffer: Some(buffer),
        });
        let state_ptr = Box::into_raw(state);
        let transfer = unsafe { libusb::libusb_alloc_transfer(0) };
        if transfer.is_null() {
            unsafe {
                let _ = Box::from_raw(state_ptr);
            }
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "Failed to allocate libusb transfer",
            ));
        }
        unsafe {
            (*transfer).dev_handle = self.handle.ptr;
            (*transfer).endpoint = 0;
            (*transfer).transfer_type = LIBUSB_TRANSFER_TYPE_CONTROL;
            (*transfer).timeout = duration_to_timeout(timeout);
            (*transfer).callback = control_callback;
            (*transfer).user_data = state_ptr as *mut c_void;
            if let Some(buffer) = (&mut *state_ptr).buffer.as_mut() {
                (*transfer).buffer = buffer.as_mut_ptr();
                (*transfer).length = buffer.len() as c_int;
            }
        }
        let submit = unsafe { libusb::libusb_submit_transfer(transfer) };
        if submit < 0 {
            unsafe {
                let _ = Box::from_raw(state_ptr);
                libusb::libusb_free_transfer(transfer);
            }
            return Err(map_libusb_error(submit));
        }
        match receiver.await {
            Ok(result) => result,
            Err(_) => Err(io::Error::new(
                io::ErrorKind::Other,
                "Control transfer channel closed",
            )),
        }
    }

    #[allow(dead_code)]
    pub(crate) fn bulk_read_blocking(
        &self,
        endpoint: u8,
        length: usize,
        timeout: Duration,
    ) -> io::Result<Vec<u8>> {
        let mut buffer = vec![0u8; length];
        let mut transferred: c_int = 0;
        let rc = unsafe {
            libusb::libusb_bulk_transfer(
                self.handle.ptr,
                endpoint,
                buffer.as_mut_ptr(),
                length as c_int,
                &mut transferred,
                duration_to_timeout(timeout) as c_uint,
            )
        };
        if rc < 0 {
            eprintln!("[usb] bulk_read_blocking error: rc={rc}");
            return Err(map_libusb_error(rc));
        }
        buffer.truncate(transferred as usize);
        Ok(buffer)
    }

    pub(crate) fn bulk_write_blocking(
        &self,
        endpoint: u8,
        data: Vec<u8>,
        timeout: Duration,
    ) -> io::Result<usize> {
        let mut transferred: c_int = 0;
        let rc = unsafe {
            libusb::libusb_bulk_transfer(
                self.handle.ptr,
                endpoint,
                data.as_ptr() as *mut u8,
                data.len() as c_int,
                &mut transferred,
                duration_to_timeout(timeout) as c_uint,
            )
        };
        if rc < 0 {
            return Err(map_libusb_error(rc));
        }
        Ok(transferred as usize)
    }

    pub(crate) fn control_out_blocking(
        &self,
        request_type: u8,
        request: u8,
        value: u16,
        index: u16,
        data: &[u8],
        timeout: Duration,
    ) -> io::Result<usize> {
        let millis = duration_to_timeout(timeout) as u32;
        let rc = unsafe {
            libusb::libusb_control_transfer(
                self.handle.ptr,
                request_type,
                request,
                value,
                index,
                data.as_ptr() as *mut u8,
                data.len() as u16,
                millis,
            )
        };
        if rc < 0 {
            return Err(map_libusb_error(rc));
        }
        Ok(rc as usize)
    }

    pub(crate) fn control_in_blocking(
        &self,
        request_type: u8,
        request: u8,
        value: u16,
        index: u16,
        buf: &mut [u8],
        timeout: Duration,
    ) -> io::Result<usize> {
        let millis = duration_to_timeout(timeout) as u32;
        let rc = unsafe {
            libusb::libusb_control_transfer(
                self.handle.ptr,
                request_type,
                request,
                value,
                index,
                buf.as_mut_ptr(),
                buf.len() as u16,
                millis,
            )
        };
        if rc < 0 {
            return Err(map_libusb_error(rc));
        }
        Ok(rc as usize)
    }
}

struct BulkWriteState {
    sender: Option<oneshot::Sender<io::Result<usize>>>,
    buffer: Option<Vec<u8>>,
    completion: Arc<TransferCompletion>,
}

struct BulkReadState {
    sender: Option<oneshot::Sender<io::Result<Vec<u8>>>>,
    buffer: Option<Vec<u8>>,
    completion: Arc<TransferCompletion>,
}

struct TransferCompletion {
    transfer: Mutex<*mut libusb::libusb_transfer>,
    finished: Condvar,
}

// The pointer is only inspected, cancelled, or cleared while holding the mutex.
unsafe impl Send for TransferCompletion {}
unsafe impl Sync for TransferCompletion {}

pub(crate) type PendingBulkRead = PendingTransfer<Vec<u8>>;
pub(crate) type PendingBulkWrite = PendingTransfer<usize>;

pub(crate) struct PendingTransfer<T> {
    receiver: oneshot::Receiver<io::Result<T>>,
    completion: Arc<TransferCompletion>,
    _handle: LibusbDeviceHandle,
}

impl<T> Future for PendingTransfer<T> {
    type Output = io::Result<T>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.receiver).poll(cx).map(|result| {
            result.unwrap_or_else(|_| Err(io::Error::other("USB transfer channel closed")))
        })
    }
}

impl<T> Drop for PendingTransfer<T> {
    fn drop(&mut self) {
        let mut transfer = self.completion.transfer.lock().unwrap();
        if !transfer.is_null() {
            // Cancellation is asynchronous. Keep the buffer, handle and context
            // alive until the event thread has run the cancellation callback.
            unsafe { libusb::libusb_cancel_transfer(*transfer) };
            while !transfer.is_null() {
                transfer = self.completion.finished.wait(transfer).unwrap();
            }
        }
    }
}

struct ControlTransferState {
    sender: Option<oneshot::Sender<io::Result<usize>>>,
    buffer: Option<Vec<u8>>,
}

struct LoopReadState {
    buffer: Vec<u8>,
    frame_tx: tokio::sync::mpsc::Sender<Vec<u8>>,
}

const NUM_TRANSFERS: usize = 8;

pub async fn spawn_bulk_read_loop(
    handle: Arc<LibusbDeviceHandle>,
    endpoint: u8,
    length: usize,
    frame_tx: tokio::sync::mpsc::Sender<Vec<u8>>,
) -> io::Result<()> {
    for _ in 0..NUM_TRANSFERS {
        submit_bulk_transfer(handle.clone(), endpoint, length, frame_tx.clone())?;
    }
    Ok(())
}

fn submit_bulk_transfer(
    handle: Arc<LibusbDeviceHandle>,
    endpoint: u8,
    length: usize,
    frame_tx: tokio::sync::mpsc::Sender<Vec<u8>>,
) -> io::Result<()> {
    let state = Box::new(LoopReadState {
        buffer: vec![0u8; length],
        frame_tx,
    });

    let transfer = unsafe { libusb::libusb_alloc_transfer(0) };
    if transfer.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            "Failed to alloc transfer",
        ));
    }

    unsafe {
        (*transfer).dev_handle = handle.raw();
        (*transfer).endpoint = endpoint;
        (*transfer).transfer_type = LIBUSB_TRANSFER_TYPE_BULK;
        (*transfer).timeout = 0; // no timeout, let device push
        (*transfer).callback = loop_read_callback;
        (*transfer).user_data = Box::into_raw(state) as *mut c_void;

        let buf_ref = &mut (*((*transfer).user_data as *mut LoopReadState)).buffer;
        (*transfer).buffer = buf_ref.as_mut_ptr();
        (*transfer).length = buf_ref.len() as c_int;
    }

    let submit = unsafe { libusb::libusb_submit_transfer(transfer) };
    if submit < 0 {
        unsafe { libusb::libusb_free_transfer(transfer) };
        return Err(map_libusb_error(submit));
    }
    Ok(())
}

extern "system" fn loop_read_callback(transfer: *mut libusb::libusb_transfer) {
    unsafe {
        let state_ptr = (*transfer).user_data as *mut LoopReadState;
        let state = &mut *state_ptr;

        if (*transfer).status == libusb1_sys::constants::LIBUSB_TRANSFER_COMPLETED {
            let len = (*transfer).actual_length as usize;
            if len > 0 {
                let mut frame = vec![0u8; len];
                frame.copy_from_slice(&state.buffer[..len]);

                debug!(
                    "bulk_rx: got {} bytes (endpoint=0x{:02x})",
                    len,
                    (*transfer).endpoint
                );

                let _ = state.frame_tx.try_send(frame);
            }
        } else {
            warn!(
                "bulk_rx error: status={} actual_length={}",
                (*transfer).status,
                (*transfer).actual_length
            );
        }

        // Resubmit so pipeline stays full
        let res = libusb::libusb_submit_transfer(transfer);
        if res < 0 {
            error!("libusb resubmit failed: {:?}", res);
            libusb::libusb_free_transfer(transfer);
        }
    }
}

extern "system" fn bulk_write_callback(transfer: *mut libusb::libusb_transfer) {
    unsafe {
        let state_ptr = (*transfer).user_data as *mut BulkWriteState;
        let mut state = Box::from_raw(state_ptr);
        let completion = state.completion.clone();
        let mut active = completion.transfer.lock().unwrap();
        let result = if (*transfer).status == LIBUSB_TRANSFER_COMPLETED {
            Ok((*transfer).actual_length as usize)
        } else {
            Err(map_transfer_status((*transfer).status))
        };
        libusb::libusb_free_transfer(transfer);
        *active = ptr::null_mut();
        if let Some(sender) = state.sender.take() {
            let _ = sender.send(result);
        }
        completion.finished.notify_all();
    }
}

extern "system" fn bulk_read_callback(transfer: *mut libusb::libusb_transfer) {
    unsafe {
        let state_ptr = (*transfer).user_data as *mut BulkReadState;
        let mut state = Box::from_raw(state_ptr);
        let completion = state.completion.clone();
        let mut active = completion.transfer.lock().unwrap();
        let status = (*transfer).status;
        let result = if status == LIBUSB_TRANSFER_COMPLETED {
            if let Some(mut buffer) = state.buffer.take() {
                let actual = (*transfer).actual_length as usize;
                buffer.truncate(actual);
                Ok(buffer)
            } else {
                Err(io::Error::new(
                    io::ErrorKind::Other,
                    "Bulk read buffer missing",
                ))
            }
        } else {
            Err(map_transfer_status(status))
        };
        libusb::libusb_free_transfer(transfer);
        *active = ptr::null_mut();
        if let Some(sender) = state.sender.take() {
            let _ = sender.send(result);
        }
        completion.finished.notify_all();
    }
}

extern "system" fn control_callback(transfer: *mut libusb::libusb_transfer) {
    unsafe {
        let state_ptr = (*transfer).user_data as *mut ControlTransferState;
        let mut state = Box::from_raw(state_ptr);
        let status = (*transfer).status;
        let result = if status == LIBUSB_TRANSFER_COMPLETED {
            Ok((*transfer).actual_length as usize)
        } else {
            Err(map_transfer_status(status))
        };
        state.buffer.take();
        if let Some(sender) = state.sender.take() {
            let _ = sender.send(result);
        }
        libusb::libusb_free_transfer(transfer);
    }
}

pub(crate) fn libusb_error_string(code: i32) -> String {
    unsafe {
        let ptr = libusb::libusb_error_name(code);
        if ptr.is_null() {
            format!("libusb error {code}")
        } else {
            CStr::from_ptr(ptr).to_string_lossy().into_owned()
        }
    }
}

pub(crate) fn map_libusb_error(code: i32) -> io::Error {
    let kind = match code {
        LIBUSB_ERROR_ACCESS => io::ErrorKind::PermissionDenied,
        LIBUSB_ERROR_BUSY => io::ErrorKind::AddrInUse,
        LIBUSB_ERROR_TIMEOUT => io::ErrorKind::WouldBlock,
        LIBUSB_ERROR_PIPE => io::ErrorKind::BrokenPipe,
        LIBUSB_ERROR_NO_DEVICE => io::ErrorKind::NotConnected,
        LIBUSB_ERROR_NOT_FOUND => io::ErrorKind::NotFound,
        LIBUSB_ERROR_INTERRUPTED => io::ErrorKind::Interrupted,
        _ => io::ErrorKind::Other,
    };
    io::Error::new(kind, libusb_error_string(code))
}

pub(crate) fn map_transfer_status(status: i32) -> io::Error {
    let (kind, description) = match status {
        s if s == LIBUSB_TRANSFER_TIMED_OUT => {
            (io::ErrorKind::WouldBlock, "libusb transfer timed out")
        }
        s if s == LIBUSB_TRANSFER_STALL => (io::ErrorKind::BrokenPipe, "libusb transfer stalled"),
        s if s == LIBUSB_TRANSFER_NO_DEVICE => {
            (io::ErrorKind::NotConnected, "libusb device disconnected")
        }
        s if s == LIBUSB_TRANSFER_CANCELLED => {
            (io::ErrorKind::Interrupted, "libusb transfer cancelled")
        }
        s if s == LIBUSB_TRANSFER_ERROR => (io::ErrorKind::Other, "libusb transfer error"),
        s if s == LIBUSB_TRANSFER_OVERFLOW => (io::ErrorKind::Other, "libusb transfer overflow"),
        _ => (io::ErrorKind::Other, "libusb transfer failed"),
    };
    io::Error::new(kind, description)
}

pub(crate) fn get_device_descriptor(
    device: *mut libusb::libusb_device,
) -> io::Result<libusb::libusb_device_descriptor> {
    let mut desc = MaybeUninit::<libusb::libusb_device_descriptor>::uninit();
    let rc = unsafe { libusb::libusb_get_device_descriptor(device, desc.as_mut_ptr()) };
    if rc < 0 {
        return Err(map_libusb_error(rc));
    }
    Ok(unsafe { desc.assume_init() })
}

pub(crate) fn read_string_descriptor(handle: &LibusbDeviceHandle, index: u8) -> Option<String> {
    if index == 0 {
        return None;
    }
    let mut buf = vec![0u8; 255];
    let len = unsafe {
        libusb::libusb_get_string_descriptor_ascii(
            handle.raw(),
            index,
            buf.as_mut_ptr(),
            buf.len() as c_int,
        )
    };
    if len < 0 {
        return None;
    }
    buf.truncate(len as usize);
    String::from_utf8(buf).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestRead = (
        *mut libusb::libusb_transfer,
        Arc<TransferCompletion>,
        oneshot::Receiver<io::Result<Vec<u8>>>,
    );

    // Exercise the real FFI callback without submitting to a USB device.
    fn read_completion(status: i32, buffer: Vec<u8>, actual_length: usize) -> TestRead {
        let transfer = unsafe { libusb::libusb_alloc_transfer(0) };
        assert!(!transfer.is_null());
        let completion = Arc::new(TransferCompletion {
            transfer: Mutex::new(transfer),
            finished: Condvar::new(),
        });
        let (sender, receiver) = oneshot::channel();
        let state = Box::new(BulkReadState {
            sender: Some(sender),
            buffer: Some(buffer),
            completion: completion.clone(),
        });
        unsafe {
            (*transfer).status = status;
            (*transfer).actual_length = actual_length as c_int;
            (*transfer).user_data = Box::into_raw(state).cast();
        }
        (transfer, completion, receiver)
    }

    #[test]
    fn read_callback_delivers_received_bytes_and_retires_transfer() {
        let (transfer, completion, receiver) =
            read_completion(LIBUSB_TRANSFER_COMPLETED, vec![1, 2, 3, 0, 0, 0], 3);
        bulk_read_callback(transfer);
        assert_eq!(receiver.blocking_recv().unwrap().unwrap(), vec![1, 2, 3]);
        assert!(completion.transfer.lock().unwrap().is_null());
    }

    #[test]
    fn write_callback_reports_short_writes_errors_and_retires_buffer() {
        for (status, actual) in [
            (LIBUSB_TRANSFER_COMPLETED, 20),
            (LIBUSB_TRANSFER_COMPLETED, 19),
            (LIBUSB_TRANSFER_CANCELLED, 0),
            (LIBUSB_TRANSFER_NO_DEVICE, 0),
            (LIBUSB_TRANSFER_TIMED_OUT, 0),
        ] {
            let transfer = unsafe { libusb::libusb_alloc_transfer(0) };
            assert!(!transfer.is_null());
            let completion = Arc::new(TransferCompletion {
                transfer: Mutex::new(transfer),
                finished: Condvar::new(),
            });
            let (sender, receiver) = oneshot::channel();
            let state = Box::new(BulkWriteState {
                sender: Some(sender),
                buffer: Some(vec![0; 20]),
                completion: completion.clone(),
            });
            unsafe {
                (*transfer).status = status;
                (*transfer).actual_length = actual;
                (*transfer).user_data = Box::into_raw(state).cast();
            }
            bulk_write_callback(transfer);
            let result = receiver.blocking_recv().unwrap();
            if status == LIBUSB_TRANSFER_COMPLETED {
                assert_eq!(result.unwrap(), actual as usize);
            } else {
                assert_eq!(
                    result.unwrap_err().kind(),
                    map_transfer_status(status).kind()
                );
            }
            assert!(completion.transfer.lock().unwrap().is_null());
        }
    }

    #[test]
    fn error_callbacks_wake_cancellation_waiters() {
        for (status, kind) in [
            (LIBUSB_TRANSFER_CANCELLED, io::ErrorKind::Interrupted),
            (LIBUSB_TRANSFER_NO_DEVICE, io::ErrorKind::NotConnected),
        ] {
            let (transfer, completion, receiver) = read_completion(status, vec![0; 64], 0);
            let waiting = completion.clone();
            let (started_tx, started_rx) = std::sync::mpsc::channel();
            let (done_tx, done_rx) = std::sync::mpsc::channel();
            let waiter = std::thread::spawn(move || {
                let mut active = waiting.transfer.lock().unwrap();
                started_tx.send(()).unwrap();
                while !active.is_null() {
                    active = waiting.finished.wait(active).unwrap();
                }
                done_tx.send(()).unwrap();
            });
            started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            bulk_read_callback(transfer);
            done_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("cancellation waiter was not woken");
            waiter.join().unwrap();
            assert_eq!(receiver.blocking_recv().unwrap().unwrap_err().kind(), kind);
            assert!(completion.transfer.lock().unwrap().is_null());
        }
    }

    #[test]
    fn idle_event_thread_can_be_stopped() {
        let context = LibusbContext::new().unwrap();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            drop(context);
            done_tx.send(()).unwrap();
        });
        done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("event thread failed to stop");
        worker.join().unwrap();
    }
}
