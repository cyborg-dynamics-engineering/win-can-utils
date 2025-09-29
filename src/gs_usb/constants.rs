use libusb1_sys::constants::{
    LIBUSB_ENDPOINT_OUT, LIBUSB_RECIPIENT_INTERFACE, LIBUSB_REQUEST_TYPE_VENDOR,
};
use std::time::Duration;

/// Default timeout for most USB operations against gs_usb devices.
pub const USB_TIMEOUT: Duration = Duration::from_millis(25);
/// Size of the host frame buffer used when exchanging frames with the device.
pub const HOST_FRAME_SIZE: usize = 4 + 4 + 1 + 1 + 1 + 1 + 4 + 64;

pub const GS_USB_BREQ_BITTIMING: u8 = 1;
pub const GS_USB_BREQ_MODE: u8 = 2;
pub const GS_USB_BREQ_TIMESTAMP: u8 = 3;

pub const GS_CAN_MODE_RESET: u32 = 0x0000_0000;
pub const GS_CAN_MODE_START: u32 = 0x0000_0001;

pub const GS_CAN_ECHO_ID_UNUSED: u32 = 0xFFFF_FFFF;

/// Layout of a gs_usb frame on the wire.
pub const GS_HEADER_LEN: usize = 12; // echo_id(4) + can_id(4) + dlc(1)+chan(1)+flags(1)+res(1)
pub const GS_TS_LEN: usize = 4; // optional u32 timestamp
pub const GS_MAX_DATA: usize = 64; // max CAN(-FD) payload
pub const GS_MAX_FRAME_LEN: usize = GS_HEADER_LEN + GS_TS_LEN + GS_MAX_DATA; // 80

/// Chosen USB bulk read size (in bytes). Must be a multiple of the frame length.
pub const USB_READ_BYTES: usize = 80;

pub const CAN_EFF_FLAG: u32 = 0x8000_0000;
pub const CAN_RTR_FLAG: u32 = 0x4000_0000;
pub const CAN_ERR_FLAG: u32 = 0x2000_0000;
pub const CAN_EFF_MASK: u32 = 0x1FFF_FFFF;
pub const CAN_SFF_MASK: u32 = 0x0000_07FF;
pub const CAN_ERR_MASK: u32 = 0x1FFF_FFFF;

pub const TARGET_SAMPLE_POINT: f64 = 0.875;

/// Helper returning the request type value used for vendor specific control transfers.
pub fn request_type_out() -> u8 {
    (LIBUSB_ENDPOINT_OUT | LIBUSB_REQUEST_TYPE_VENDOR | LIBUSB_RECIPIENT_INTERFACE) as u8
}

/// Helper to convert a desired timeout duration into a libusb timeout value in milliseconds.
pub fn duration_to_timeout(duration: Duration) -> u32 {
    use std::os::raw::c_uint;

    if duration.is_zero() {
        return 0;
    }
    let millis = duration.as_millis();
    if millis == 0 {
        1
    } else if millis > c_uint::MAX as u128 {
        c_uint::MAX as u32
    } else {
        millis as u32
    }
}
