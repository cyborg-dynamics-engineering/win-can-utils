use libusb1_sys::constants::{
    LIBUSB_ENDPOINT_IN, LIBUSB_ENDPOINT_OUT, LIBUSB_RECIPIENT_INTERFACE, LIBUSB_REQUEST_TYPE_VENDOR,
};
use std::time::Duration;

/// Default timeout for most USB operations against gs_usb devices.
pub const USB_TIMEOUT: Duration = Duration::from_millis(5);
/// Delay between retries when a bulk write times out.
pub const USB_WRITE_RETRY_DELAY: Duration = Duration::from_millis(1);

/// Header and data sizes
pub const GS_HEADER_LEN: usize = 12; // echo_id(4) + can_id(4) + dlc(1) + chan(1) + flags(1) + res(1)
pub const GS_TS_LEN: usize = 4; // optional u32 timestamp (RX only)
pub const GS_MAX_DATA: usize = 64; // max CAN(-FD) payload
/// Max frame length (RX: header + ts + payload)
pub const GS_MAX_FRAME_LEN: usize = GS_HEADER_LEN + GS_TS_LEN + GS_MAX_DATA; // 80
/// Classic CAN TX frame length (header + 8 data bytes)
pub const GS_CAN_FRAME_LEN: usize = GS_HEADER_LEN + 8; // 20
pub const GS_TX_FRAME_SIZE: usize = 12 + 64; // 76 bytes, no timestamp on TX

/// Chosen USB bulk read size (in bytes).
pub const USB_READ_BYTES: usize = GS_MAX_FRAME_LEN;

/// Host echo-id unused value
pub const GS_CAN_ECHO_ID_UNUSED: u32 = 0xFFFF_FFFF;

//
// USB control requests (bRequest values)
//
pub const GS_USB_BREQ_HOST_FORMAT: u8 = 0x00;
pub const GS_USB_BREQ_BITTIMING: u8 = 0x01;
pub const GS_USB_BREQ_MODE: u8 = 0x02;
pub const GS_USB_BREQ_BERR: u8 = 0x03;
pub const GS_USB_BREQ_BT_CONST: u8 = 0x04;
pub const GS_USB_BREQ_DEVICE_CONFIG: u8 = 0x05;
pub const GS_USB_BREQ_TIMESTAMP: u8 = 0x06;
pub const GS_USB_BREQ_IDENTIFY: u8 = 0x07;
// 0x08 GET_USER_ID (rarely used)
// 0x09 SET_USER_ID
pub const GS_USB_BREQ_DATA_BITTIMING: u8 = 0x0A;
pub const GS_USB_BREQ_BT_CONST_EXT: u8 = 0x0B;
pub const GS_USB_BREQ_SET_TERMINATION: u8 = 0x0C;
pub const GS_USB_BREQ_GET_TERMINATION: u8 = 0x0D;
pub const GS_USB_BREQ_GET_STATE: u8 = 0x0E;

//
// gs_can_mode flags
//
pub const GS_CAN_MODE_RESET: u32 = 0x0000_0000;
pub const GS_CAN_MODE_START: u32 = 0x0000_0001;
pub const GS_CAN_MODE_LOOP_BACK: u32 = 0x0000_0002;
pub const GS_CAN_MODE_LISTEN_ONLY: u32 = 0x0000_0004;
pub const GS_CAN_MODE_TRIPLE_SAMPLE: u32 = 0x0000_0008;
pub const GS_CAN_MODE_ONE_SHOT: u32 = 0x0000_0010;
pub const GS_CAN_MODE_HW_TIMESTAMP: u32 = 0x0000_0020;
pub const GS_CAN_MODE_PAD_PKTS_TO_MAX_PKT_SIZE: u32 = 0x0000_0040;

//
// gs_can_feature flags (reported by BT_CONST / BT_CONST_EXT)
//
pub const GS_CAN_FEATURE_LISTEN_ONLY: u32 = 0x0000_0001;
pub const GS_CAN_FEATURE_LOOP_BACK: u32 = 0x0000_0002;
pub const GS_CAN_FEATURE_TRIPLE_SAMPLE: u32 = 0x0000_0004;
pub const GS_CAN_FEATURE_ONE_SHOT: u32 = 0x0000_0008;
pub const GS_CAN_FEATURE_HW_TIMESTAMP: u32 = 0x0000_0010;
pub const GS_CAN_FEATURE_IDENTIFY: u32 = 0x0000_0020;
pub const GS_CAN_FEATURE_PAD_PKTS_TO_MAX_PKT_SIZE: u32 = 0x0000_0040;
pub const GS_CAN_FEATURE_FD: u32 = 0x0000_0100;
pub const GS_CAN_FEATURE_BRS: u32 = 0x0000_0200;
pub const GS_CAN_FEATURE_EXT_LOOP_BACK: u32 = 0x0000_0400;

//
// CAN ID flags/masks (as in Linux <linux/can.h>)
//
pub const CAN_EFF_FLAG: u32 = 0x8000_0000; // extended frame format
pub const CAN_RTR_FLAG: u32 = 0x4000_0000; // remote transmission request
pub const CAN_ERR_FLAG: u32 = 0x2000_0000; // error frame

pub const CAN_SFF_MASK: u32 = 0x0000_07FF; // standard frame format mask
pub const CAN_EFF_MASK: u32 = 0x1FFF_FFFF; // extended frame format mask
pub const CAN_ERR_MASK: u32 = 0x1FFF_FFFF; // error mask

//
// Bit-timing defaults
//
pub const TARGET_SAMPLE_POINT: f64 = 0.875;

//
// Helpers
//

/// Vendor OUT request type
pub fn request_type_out() -> u8 {
    (LIBUSB_REQUEST_TYPE_VENDOR | LIBUSB_RECIPIENT_INTERFACE | LIBUSB_ENDPOINT_OUT) as u8
}

/// Vendor IN request type
pub fn request_type_in() -> u8 {
    (LIBUSB_REQUEST_TYPE_VENDOR | LIBUSB_RECIPIENT_INTERFACE | LIBUSB_ENDPOINT_IN) as u8
}

/// Convert Duration into libusb timeout (ms)
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
