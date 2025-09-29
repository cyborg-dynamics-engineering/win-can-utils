use async_trait::async_trait;
use crosscan::can::CanFrame;
use libc::timeval;
use libusb1_sys as libusb;
use libusb1_sys::constants::{
    LIBUSB_CONTROL_SETUP_SIZE, LIBUSB_ENDPOINT_IN, LIBUSB_ENDPOINT_OUT, LIBUSB_ERROR_INTERRUPTED,
    LIBUSB_ERROR_NO_DEVICE, LIBUSB_ERROR_NOT_FOUND, LIBUSB_ERROR_NOT_SUPPORTED, LIBUSB_ERROR_PIPE,
    LIBUSB_ERROR_TIMEOUT, LIBUSB_RECIPIENT_INTERFACE, LIBUSB_REQUEST_TYPE_VENDOR,
    LIBUSB_TRANSFER_CANCELLED, LIBUSB_TRANSFER_COMPLETED, LIBUSB_TRANSFER_ERROR,
    LIBUSB_TRANSFER_NO_DEVICE, LIBUSB_TRANSFER_OVERFLOW, LIBUSB_TRANSFER_STALL,
    LIBUSB_TRANSFER_TIMED_OUT, LIBUSB_TRANSFER_TYPE_BULK, LIBUSB_TRANSFER_TYPE_CONTROL,
    LIBUSB_TRANSFER_TYPE_INTERRUPT,
};
use std::cmp::min;
use std::ffi::CStr;
use std::mem::MaybeUninit;
use std::os::raw::{c_int, c_uint, c_void};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;
use std::{io, thread};
use tokio::sync::oneshot;

use crate::can_driver::CanDriver;

const USB_TIMEOUT: Duration = Duration::from_millis(25);
const READ_CHUNK_FRAMES: usize = 1024;
const DRAIN_READ_TIMEOUT: Duration = Duration::from_millis(2);

const HOST_FRAME_SIZE: usize = 4 + 4 + 1 + 1 + 1 + 1 + 4 + 64;

const GS_USB_BREQ_HOST_FORMAT: u8 = 0;
const GS_USB_BREQ_BITTIMING: u8 = 1;
const GS_USB_BREQ_MODE: u8 = 2;
const GS_USB_BREQ_TIMESTAMP: u8 = 3;
const GS_CAN_MODE_RESET: u32 = 0x0000_0000;
const GS_CAN_MODE_START: u32 = 0x0000_0001;

const GS_CAN_ECHO_ID_UNUSED: u32 = 0xFFFF_FFFF;

const GS_HEADER_LEN: usize = 12; // echo_id(4) + can_id(4) + dlc(1)+chan(1)+flags(1)+res(1)
const GS_TS_LEN: usize = 4; // optional u32 timestamp
const GS_MAX_DATA: usize = 64; // max CAN(-FD) payload
const GS_MAX_FRAME_LEN: usize = GS_HEADER_LEN + GS_TS_LEN + GS_MAX_DATA; // 80

// choose a sane USB bulk read size (multiple frames)
const USB_READ_BYTES: usize = 8 * 1024;

const CAN_EFF_FLAG: u32 = 0x8000_0000;
const CAN_RTR_FLAG: u32 = 0x4000_0000;
const CAN_ERR_FLAG: u32 = 0x2000_0000;
const CAN_EFF_MASK: u32 = 0x1FFF_FFFF;
const CAN_SFF_MASK: u32 = 0x0000_07FF;
const CAN_ERR_MASK: u32 = 0x1FFF_FFFF;

const TARGET_SAMPLE_POINT: f64 = 0.875;
const GS_USB_MAX_ECHO_SLOTS: u32 = 64;

#[derive(Clone, Copy, Debug)]
struct InterfaceInfo {
    interface: u8,
    in_ep: u8,
    out_ep: u8,
    int_ep: Option<u8>,
}

fn libusb_error_string(code: i32) -> String {
    unsafe {
        let ptr = libusb::libusb_error_name(code);
        if ptr.is_null() {
            format!("libusb error {code}")
        } else {
            CStr::from_ptr(ptr).to_string_lossy().into_owned()
        }
    }
}

fn map_libusb_error(code: i32) -> io::Error {
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

fn map_transfer_status(status: i32) -> io::Error {
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

fn request_type_out() -> u8 {
    (LIBUSB_ENDPOINT_OUT | LIBUSB_REQUEST_TYPE_VENDOR | LIBUSB_RECIPIENT_INTERFACE) as u8
}

fn duration_to_timeout(duration: Duration) -> c_uint {
    if duration.is_zero() {
        return 0;
    }
    let millis = duration.as_millis();
    if millis == 0 {
        1
    } else if millis > c_uint::MAX as u128 {
        c_uint::MAX
    } else {
        millis as c_uint
    }
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

#[derive(Copy, Clone)]
struct LibusbCtxPtr(*mut libusb::libusb_context);

unsafe impl Send for LibusbCtxPtr {}
unsafe impl Sync for LibusbCtxPtr {}

pub struct LibusbContext {
    ptr: LibusbCtxPtr,
    running: Arc<AtomicBool>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl LibusbContext {
    pub fn new() -> io::Result<Arc<Self>> {
        // Initialise libusb context
        let mut ctx = std::ptr::null_mut();
        let rc = unsafe { libusb::libusb_init(&mut ctx) };
        if rc < 0 {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                format!("libusb init failed: {}", rc),
            ));
        }

        let ctx_ptr = Arc::new(LibusbCtxPtr(ctx));

        // Control flag
        let running = Arc::new(AtomicBool::new(true));

        let running_thread = running.clone();
        let ctx_for_thread = ctx_ptr.clone();

        // Spawn background thread
        let handle = std::thread::Builder::new()
            .name("libusb-events".into())
            .spawn(move || {
                // 10ms poll keeps latency low and prevents stalling async callbacks
                let mut timeval = libc::timeval {
                    tv_sec: 0,
                    tv_usec: 10_000,
                };
                while running_thread.load(Ordering::SeqCst) {
                    let rc = unsafe {
                        libusb::libusb_handle_events_timeout_completed(
                            ctx_for_thread.0,
                            &mut timeval,
                            std::ptr::null_mut(),
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
            ptr: *ctx_ptr, // store as raw wrapper
            running,
            thread: Mutex::new(Some(handle)),
        }))
    }
}

impl Drop for LibusbContext {
    fn drop(&mut self) {
        // signal shutdown
        self.running.store(false, Ordering::SeqCst);

        // poke libusb so it unblocks
        unsafe {
            let mut zero = libc::timeval {
                tv_sec: 0,
                tv_usec: 0,
            };
            let _ = libusb::libusb_handle_events_timeout_completed(
                self.ptr.0,
                &mut zero,
                std::ptr::null_mut(),
            );
        }

        // join the thread safely
        if let Ok(mut guard) = self.thread.lock() {
            if let Some(handle) = guard.take() {
                let _ = handle.join();
            }
        }

        // free the libusb context
        unsafe { libusb::libusb_exit(self.ptr.0) };
    }
}

struct LibusbDeviceHandle {
    context: Arc<LibusbContext>,
    handle: *mut libusb::libusb_device_handle,
}

unsafe impl Send for LibusbDeviceHandle {}
unsafe impl Sync for LibusbDeviceHandle {}

impl LibusbDeviceHandle {
    fn open(context: Arc<LibusbContext>, device: *mut libusb::libusb_device) -> io::Result<Self> {
        let mut handle = ptr::null_mut();
        let rc = unsafe { libusb::libusb_open(device, &mut handle) };
        if rc < 0 {
            return Err(map_libusb_error(rc));
        }
        Ok(Self { context, handle })
    }

    fn raw(&self) -> *mut libusb::libusb_device_handle {
        self.handle
    }

    fn set_auto_detach_kernel_driver(&self, enable: bool) -> io::Result<()> {
        let flag = if enable { 1 } else { 0 };
        let rc = unsafe { libusb::libusb_set_auto_detach_kernel_driver(self.handle, flag) };
        if rc < 0 && rc != LIBUSB_ERROR_NOT_SUPPORTED {
            return Err(map_libusb_error(rc));
        }
        Ok(())
    }

    fn claim_interface(&self, interface: i32) -> io::Result<()> {
        let rc = unsafe { libusb::libusb_claim_interface(self.handle, interface) };
        if rc < 0 {
            return Err(map_libusb_error(rc));
        }
        Ok(())
    }

    fn clear_halt(&self, endpoint: u8) -> io::Result<()> {
        let rc = unsafe { libusb::libusb_clear_halt(self.handle, endpoint) };
        if rc < 0 {
            return Err(map_libusb_error(rc));
        }
        Ok(())
    }

    async fn bulk_write(
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
            (*transfer).dev_handle = self.handle;
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

    async fn bulk_read(
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
            (*transfer).dev_handle = self.handle;
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

    async fn control_out(
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
            (*transfer).dev_handle = self.handle;
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
}

impl Drop for LibusbDeviceHandle {
    fn drop(&mut self) {
        unsafe {
            libusb::libusb_close(self.handle);
        }
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

struct ConfigDescriptor(*const libusb::libusb_config_descriptor);

impl ConfigDescriptor {
    unsafe fn active(device: *mut libusb::libusb_device) -> io::Result<Self> {
        let mut ptr = ptr::null();
        let rc = unsafe { libusb::libusb_get_active_config_descriptor(device, &mut ptr) };
        if rc < 0 {
            return Err(map_libusb_error(rc));
        }
        Ok(Self(ptr))
    }

    unsafe fn by_index(device: *mut libusb::libusb_device, index: u8) -> io::Result<Self> {
        let mut ptr = ptr::null();
        let rc = unsafe { libusb::libusb_get_config_descriptor(device, index as u8, &mut ptr) };
        if rc < 0 {
            return Err(map_libusb_error(rc));
        }
        Ok(Self(ptr))
    }
}

impl Drop for ConfigDescriptor {
    fn drop(&mut self) {
        unsafe {
            libusb::libusb_free_config_descriptor(self.0);
        }
    }
}

unsafe fn find_gs_usb_interface(
    device: *mut libusb::libusb_device,
) -> io::Result<Option<InterfaceInfo>> {
    let config = match unsafe { ConfigDescriptor::active(device) } {
        Ok(cfg) => cfg,
        Err(err) if err.kind() == io::ErrorKind::NotFound => unsafe {
            ConfigDescriptor::by_index(device, 0)?
        },
        Err(err) => return Err(err),
    };

    let config_ptr = config.0;
    let interface_count = unsafe { (*config_ptr).bNumInterfaces };
    for interface_index in 0..interface_count {
        let interface = unsafe { &*(*config_ptr).interface.add(interface_index as usize) };
        for alt_index in 0..interface.num_altsetting as usize {
            let descriptor = unsafe { &*interface.altsetting.add(alt_index) };
            if descriptor.bInterfaceClass != 0xff {
                continue;
            }
            let mut info = InterfaceInfo {
                interface: descriptor.bInterfaceNumber,
                in_ep: 0,
                out_ep: 0,
                int_ep: None,
            };
            for ep_index in 0..descriptor.bNumEndpoints as usize {
                let endpoint = unsafe { &*descriptor.endpoint.add(ep_index) };
                match endpoint.bmAttributes & 0x3 {
                    x if x == LIBUSB_TRANSFER_TYPE_BULK => {
                        if endpoint.bEndpointAddress & LIBUSB_ENDPOINT_IN != 0 {
                            info.in_ep = endpoint.bEndpointAddress;
                        } else {
                            info.out_ep = endpoint.bEndpointAddress;
                        }
                    }
                    x if x == LIBUSB_TRANSFER_TYPE_INTERRUPT => {
                        if endpoint.bEndpointAddress & LIBUSB_ENDPOINT_IN != 0 {
                            info.int_ep = Some(endpoint.bEndpointAddress);
                        }
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

fn get_device_descriptor(
    device: *mut libusb::libusb_device,
) -> io::Result<libusb::libusb_device_descriptor> {
    let mut desc = MaybeUninit::<libusb::libusb_device_descriptor>::uninit();
    let rc = unsafe { libusb::libusb_get_device_descriptor(device, desc.as_mut_ptr()) };
    if rc < 0 {
        return Err(map_libusb_error(rc));
    }
    Ok(unsafe { desc.assume_init() })
}

fn read_string_descriptor(handle: &LibusbDeviceHandle, index: u8) -> Option<String> {
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

fn device_matches_identifier(
    identifier: &str,
    index: usize,
    device: *mut libusb::libusb_device,
    desc: &libusb::libusb_device_descriptor,
    handle: &LibusbDeviceHandle,
) -> bool {
    let ident = identifier.trim();
    if ident.eq_ignore_ascii_case("auto") {
        return true;
    }

    if let Ok(idx) = ident.parse::<usize>() {
        if idx == index {
            return true;
        }
    }

    if let Some(serial) = read_string_descriptor(handle, desc.iSerialNumber) {
        if serial.eq_ignore_ascii_case(ident) {
            return true;
        }
    }

    if let Some(product) = read_string_descriptor(handle, desc.iProduct) {
        if product.eq_ignore_ascii_case(ident) {
            return true;
        }
    }

    let bus = unsafe { libusb::libusb_get_bus_number(device) };
    let address = unsafe { libusb::libusb_get_device_address(device) };
    let bus_addr = format!("{:03}:{:03}", bus, address);
    bus_addr.eq_ignore_ascii_case(ident)
}

fn read_product_label(
    handle: &LibusbDeviceHandle,
    desc: &libusb::libusb_device_descriptor,
) -> Option<String> {
    read_string_descriptor(handle, desc.iProduct)
        .or_else(|| read_string_descriptor(handle, desc.iSerialNumber))
        .or_else(|| Some(format!("{:04x}:{:04x}", desc.idVendor, desc.idProduct)))
}

fn select_device(
    context: &Arc<LibusbContext>,
    identifier: &str,
) -> io::Result<(LibusbDeviceHandle, InterfaceInfo, String)> {
    let mut list = ptr::null();
    let count = unsafe { libusb::libusb_get_device_list(context.ptr.0, &mut list) };
    if count < 0 {
        return Err(map_libusb_error(count as i32));
    }

    let mut result: Option<(LibusbDeviceHandle, InterfaceInfo, String)> = None;
    let mut index = 0usize;
    let mut error: Option<io::Error> = None;

    for i in 0..count {
        let device = unsafe { *list.add(i as usize) };
        let desc = match get_device_descriptor(device) {
            Ok(d) => d,
            Err(e) => {
                error = Some(e);
                break;
            }
        };

        let info = match unsafe { find_gs_usb_interface(device) } {
            Ok(Some(info)) => info,
            Ok(None) => continue,
            Err(e) => {
                error = Some(e);
                break;
            }
        };

        let handle = match LibusbDeviceHandle::open(context.clone(), device) {
            Ok(h) => h,
            Err(e) => {
                error = Some(e);
                break;
            }
        };

        let matches = device_matches_identifier(identifier, index, device, &desc, &handle);

        if matches {
            let label = read_product_label(&handle, &desc)
                .unwrap_or_else(|| format!("{:04x}:{:04x}", desc.idVendor, desc.idProduct));
            result = Some((handle, info, label));
            break;
        }

        index += 1;
    }

    unsafe {
        libusb::libusb_free_device_list(list, 1);
    }

    if let Some((handle, info, label)) = result {
        return Ok((handle, info, label));
    }

    if let Some(err) = error {
        return Err(err);
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

#[inline]
fn plausible_header(hdr: &[u8], expected_chan: u8) -> bool {
    if hdr.len() < GS_HEADER_LEN {
        return false;
    }
    // header fields we can sanity-check quickly
    let _echo = u32::from_le_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]);
    let _id = u32::from_le_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]);
    let dlc = hdr[8];
    let chan = hdr[9];
    // flags := hdr[10], reserved := hdr[11]

    // DLC must map to <=64 bytes
    let len = dlc_to_len(dlc);
    if len > GS_MAX_DATA {
        return false;
    }

    // Channel should usually match (or at least be small). We prioritize exact match.
    if chan != expected_chan {
        return false;
    }

    true
}

/// Parses one frame starting at bytes[0], returns (maybe_frame, consumed_len).
fn parse_host_frame_at(
    bytes: &[u8],
    channel_index: u8,
    _timestamp_enabled: bool, // advisory only; we adapt per-frame
    last_ts64: &mut Option<u64>,
) -> Option<(Option<CanFrame>, usize)> {
    if bytes.len() < GS_HEADER_LEN {
        return None;
    }

    // Read minimal header
    let echo_id = u32::from_le_bytes(bytes[0..4].try_into().ok()?);
    let raw_id = u32::from_le_bytes(bytes[4..8].try_into().ok()?);
    let dlc = bytes[8];
    let chan = bytes[9];

    let data_len = dlc_to_len(dlc);
    if data_len > GS_MAX_DATA {
        // malformed dlc -> skip one byte to resync
        return Some((None, 1));
    }

    // Consider two candidate layouts: without and with timestamp
    let len_no_ts = GS_HEADER_LEN + data_len;
    let len_with_ts = GS_HEADER_LEN + GS_TS_LEN + data_len;

    // Decide which layout is more plausible by peeking at the next header (if available).
    // Prefer a layout that leaves the next header aligned and plausible.
    let mut use_ts = false;
    let have_no_ts = bytes.len() >= len_no_ts;
    let have_with_ts = bytes.len() >= len_with_ts;

    if have_with_ts && bytes.len() >= len_with_ts + GS_HEADER_LEN {
        let next_hdr = &bytes[len_with_ts..len_with_ts + GS_HEADER_LEN];
        if plausible_header(next_hdr, channel_index) {
            use_ts = true;
        }
    }

    if !use_ts && have_no_ts && bytes.len() >= len_no_ts + GS_HEADER_LEN {
        let next_hdr = &bytes[len_no_ts..len_no_ts + GS_HEADER_LEN];
        if plausible_header(next_hdr, channel_index) {
            use_ts = false;
        }
    }

    // If only one variant fits in the available bytes, take that.
    let total_len = if use_ts {
        if !have_with_ts {
            // not enough yet
            return None;
        }
        len_with_ts
    } else {
        if !have_no_ts {
            // try timestamped if that one fits
            if have_with_ts {
                use_ts = true;
                len_with_ts
            } else {
                return None;
            }
        } else {
            len_no_ts
        }
    };

    if total_len == 0 || total_len > GS_MAX_FRAME_LEN {
        return Some((None, 1)); // safety
    }

    // Consume echoes / other channels but keep stream moving
    if echo_id != GS_CAN_ECHO_ID_UNUSED || chan != channel_index {
        return Some((None, total_len));
    }

    // Data offset after optional timestamp
    let ts_off = GS_HEADER_LEN;
    let data_off = if use_ts {
        GS_HEADER_LEN + GS_TS_LEN
    } else {
        GS_HEADER_LEN
    };

    // Bounds are guaranteed by total_len checks
    let data = &bytes[data_off..data_off + data_len];

    // Build frame
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

    // Timestamp (if we selected the ts variant)
    if use_ts {
        let ts32 = u32::from_le_bytes(bytes[ts_off..ts_off + 4].try_into().unwrap()) as u64;
        let ts64 = match *last_ts64 {
            None => ts32,
            Some(last) => {
                let base = last & !0xFFFF_FFFFu64;
                let mut candidate = base | ts32;
                if candidate < last {
                    candidate = candidate.wrapping_add(1u64 << 32);
                }
                candidate
            }
        };
        *last_ts64 = Some(ts64);
        frame.set_timestamp(Some(ts64));
    }

    Some((Some(frame), total_len))
}

pub struct GsUsbDriver {
    handle: LibusbDeviceHandle,
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
        let context = LibusbContext::new()?;
        let (handle, info, label) = select_device(&context, identifier)?;

        let _ = handle.set_auto_detach_kernel_driver(true);
        handle.claim_interface(info.interface as i32)?;

        let driver = GsUsbDriver {
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

        let host_cfg = host_config_bytes();
        driver
            .send_control(GS_USB_BREQ_HOST_FORMAT, &host_cfg)
            .await?;
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
        let mut buffer = vec![0u8; HOST_FRAME_SIZE];

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

        buffer[8] = frame.dlc() as u8;
        buffer[9] = self.channel_index;
        buffer[10] = 0;
        buffer[11] = 0;
        buffer[12..16].fill(0);

        let frame_data = frame.data();
        let data_len = frame_data.len().min(64);
        buffer[16..16 + data_len].copy_from_slice(&frame_data[..data_len]);

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
        let mut first = true;
        loop {
            let timeout = if first {
                USB_TIMEOUT
            } else {
                DRAIN_READ_TIMEOUT
            };
            let chunk = match self
                .handle
                .bulk_read(self.in_ep, USB_READ_BYTES, timeout)
                .await
            {
                Ok(chunk) => chunk,
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                    if first {
                        return Ok(Vec::new());
                    }
                    break;
                }
                Err(err) if err.kind() == io::ErrorKind::BrokenPipe => {
                    let _ = self.handle.clear_halt(self.in_ep);
                    break;
                }
                Err(err) => return Err(err),
            };

            if chunk.is_empty() {
                if first {
                    return Ok(Vec::new());
                }
                break;
            }
            first = false;
            self.rx_leftover.extend_from_slice(&chunk);
        }

        let mut frames = Vec::new();
        let mut offset = 0usize;

        while self.rx_leftover.len() >= offset + GS_HEADER_LEN {
            let slice = &self.rx_leftover[offset..];
            match parse_host_frame_at(
                slice,
                self.channel_index,
                self.timestamp_enabled, // advisory only now
                &mut self.last_timestamp64,
            ) {
                None => break, // need more bytes
                Some((maybe_frame, consumed)) => {
                    let c = if consumed == 0 || consumed > GS_MAX_FRAME_LEN {
                        1
                    } else {
                        consumed
                    };
                    if let Some(frame) = maybe_frame {
                        frames.push(frame);
                    }
                    offset += c;
                }
            }
        }

        if offset > 0 {
            self.rx_leftover.drain(..offset);
        }
        Ok(frames)
    }

    async fn close_channel(&mut self) -> io::Result<()> {
        let reset = encode_mode(GS_CAN_MODE_RESET, 0);
        self.send_control(GS_USB_BREQ_MODE, &reset).await
    }
}
