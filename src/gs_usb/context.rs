use std::ffi::CStr;
use std::io;
use std::mem::MaybeUninit;
use std::os::raw::{c_int, c_uint, c_void};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use libusb1_sys as libusb;
use libusb1_sys::constants::{
    LIBUSB_CONTROL_SETUP_SIZE, LIBUSB_ERROR_INTERRUPTED, LIBUSB_ERROR_NO_DEVICE,
    LIBUSB_ERROR_NOT_FOUND, LIBUSB_ERROR_NOT_SUPPORTED, LIBUSB_ERROR_PIPE, LIBUSB_ERROR_TIMEOUT,
    LIBUSB_TRANSFER_CANCELLED, LIBUSB_TRANSFER_COMPLETED, LIBUSB_TRANSFER_ERROR,
    LIBUSB_TRANSFER_NO_DEVICE, LIBUSB_TRANSFER_OVERFLOW, LIBUSB_TRANSFER_STALL,
    LIBUSB_TRANSFER_TIMED_OUT, LIBUSB_TRANSFER_TYPE_BULK, LIBUSB_TRANSFER_TYPE_CONTROL,
};
use tokio::sync::oneshot;

use super::constants::duration_to_timeout;

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

        // Spawn a small helper thread polling libusb for asynchronous transfer completion.
        let handle = std::thread::Builder::new()
            .name("libusb-events".into())
            .spawn(move || {
                // 10ms poll keeps latency low and prevents stalling async callbacks.
                let mut timeval = libc::timeval {
                    tv_sec: 0,
                    tv_usec: 10_000,
                };
                while running_thread.load(Ordering::SeqCst) {
                    let rc = unsafe {
                        libusb::libusb_handle_events_timeout_completed(
                            ctx_for_thread.0,
                            &mut timeval,
                            ptr::null_mut(),
                        )
                    };
                    if rc == libusb::constants::LIBUSB_ERROR_INTERRUPTED {
                        continue;
                    }
                    if rc < 0 && running_thread.load(Ordering::SeqCst) {
                        std::thread::yield_now();
                    }
                }
            })
            .map_err(|e| {
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
            let mut zero = libc::timeval {
                tv_sec: 0,
                tv_usec: 0,
            };
            let _ = libusb::libusb_handle_events_timeout_completed(
                self.ptr.0,
                &mut zero,
                ptr::null_mut(),
            );
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
    pub(crate) _context: Arc<LibusbContext>,
    handle: Arc<LibusbHandleWrapper>,
}

struct LibusbHandleWrapper(*mut libusb::libusb_device_handle);

unsafe impl Send for LibusbHandleWrapper {}
unsafe impl Sync for LibusbHandleWrapper {}

impl Drop for LibusbHandleWrapper {
    fn drop(&mut self) {
        unsafe { libusb::libusb_close(self.0) };
    }
}

impl LibusbDeviceHandle {
    pub(crate) fn open(
        context: Arc<LibusbContext>,
        device: *mut libusb::libusb_device,
    ) -> io::Result<Self> {
        let mut handle = ptr::null_mut();
        let rc = unsafe { libusb::libusb_open(device, &mut handle) };
        if rc < 0 {
            return Err(map_libusb_error(rc));
        }
        Ok(Self {
            _context: context,
            handle: Arc::new(LibusbHandleWrapper(handle)),
        })
    }

    pub(crate) fn raw(&self) -> *mut libusb::libusb_device_handle {
        self.handle.0
    }

    pub(crate) fn set_auto_detach_kernel_driver(&self, enable: bool) -> io::Result<()> {
        let flag = if enable { 1 } else { 0 };
        let rc = unsafe { libusb::libusb_set_auto_detach_kernel_driver(self.handle.0, flag) };
        if rc < 0 && rc != LIBUSB_ERROR_NOT_SUPPORTED {
            return Err(map_libusb_error(rc));
        }
        Ok(())
    }

    pub(crate) fn claim_interface(&self, interface: i32) -> io::Result<()> {
        let rc = unsafe { libusb::libusb_claim_interface(self.handle.0, interface) };
        if rc < 0 {
            return Err(map_libusb_error(rc));
        }
        Ok(())
    }

    pub(crate) fn clear_halt(&self, endpoint: u8) -> io::Result<()> {
        let rc = unsafe { libusb::libusb_clear_halt(self.handle.0, endpoint) };
        if rc < 0 {
            return Err(map_libusb_error(rc));
        }
        Ok(())
    }

    #[allow(dead_code)]
    pub(crate) async fn bulk_write(
        &self,
        endpoint: u8,
        data: Vec<u8>,
        timeout: Duration,
    ) -> io::Result<usize> {
        let (sender, receiver) = oneshot::channel();
        let state = Box::new(BulkWriteState {
            sender: Some(sender),
            buffer: Some(data),
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
            (*transfer).dev_handle = self.handle.0;
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
                "Bulk write transfer channel closed",
            )),
        }
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
                self.handle.0,
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

    pub(crate) async fn bulk_read(
        &self,
        endpoint: u8,
        length: usize,
        timeout: Duration,
    ) -> io::Result<Vec<u8>> {
        let (sender, receiver) = oneshot::channel();
        let state = Box::new(BulkReadState {
            sender: Some(sender),
            buffer: Some(vec![0u8; length]),
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
            (*transfer).dev_handle = self.handle.0;
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
                "Bulk read transfer channel closed",
            )),
        }
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
            (*transfer).dev_handle = self.handle.0;
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
                self.handle.0,
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
                self.handle.0,
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
                self.handle.0,
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
                self.handle.0,
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
}

struct BulkReadState {
    sender: Option<oneshot::Sender<io::Result<Vec<u8>>>>,
    buffer: Option<Vec<u8>>,
}

struct ControlTransferState {
    sender: Option<oneshot::Sender<io::Result<usize>>>,
    buffer: Option<Vec<u8>>,
}

extern "system" fn bulk_write_callback(transfer: *mut libusb::libusb_transfer) {
    unsafe {
        let state_ptr = (*transfer).user_data as *mut BulkWriteState;
        let mut state = Box::from_raw(state_ptr);
        let result = if (*transfer).status == LIBUSB_TRANSFER_COMPLETED {
            Ok((*transfer).actual_length as usize)
        } else {
            Err(map_transfer_status((*transfer).status))
        };
        state.buffer.take();
        if let Some(sender) = state.sender.take() {
            let _ = sender.send(result);
        }
        libusb::libusb_free_transfer(transfer);
    }
}

extern "system" fn bulk_read_callback(transfer: *mut libusb::libusb_transfer) {
    unsafe {
        let state_ptr = (*transfer).user_data as *mut BulkReadState;
        let mut state = Box::from_raw(state_ptr);
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
            eprintln!("[usb] bulk_read_callback error: {:?}", status);
            Err(map_transfer_status(status))
        };
        if let Some(sender) = state.sender.take() {
            let _ = sender.send(result);
        }
        libusb::libusb_free_transfer(transfer);
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
