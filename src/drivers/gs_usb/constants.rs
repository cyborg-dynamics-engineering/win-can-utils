#![allow(dead_code)]

use libusb1_sys::constants::{
    LIBUSB_ENDPOINT_IN, LIBUSB_ENDPOINT_OUT, LIBUSB_RECIPIENT_INTERFACE, LIBUSB_REQUEST_TYPE_VENDOR,
};
use std::time::Duration;

//
// USB endpoints (bulk)
//
pub const GSUSB_ENDPOINT_IN: u8 = 0x81;
pub const GSUSB_ENDPOINT_OUT: u8 = 0x02;

//
// Default timeouts / delays
//
pub const USB_TIMEOUT: Duration = Duration::from_millis(100);
pub const _USB_WRITE_RETRY_DELAY: Duration = Duration::from_millis(1);

//
// Frame sizing
//
pub const GS_HEADER_LEN: usize = 12; // echo_id(4) + can_id(4) + dlc(1) + chan(1) + flags(1) + res(1)
pub const GS_TS_LEN: usize = 4; // optional u32 timestamp (RX only)
pub const GS_MAX_DATA: usize = 64; // max CAN(-FD) payload
pub const GS_MAX_FRAME_LEN: usize = GS_HEADER_LEN + GS_TS_LEN + GS_MAX_DATA; // 80
pub const GS_TX_FRAME_SIZE: usize = GS_HEADER_LEN + GS_MAX_DATA; // 76 (no timestamp on TX)

pub const USB_READ_BYTES: usize = GS_MAX_FRAME_LEN;

pub const GS_CAN_ECHO_ID_UNUSED: u32 = 0xFFFF_FFFF;

//
// USB control requests (bRequest values)
// (match enum gs_usb_breq in the header)
//
pub const GS_USB_BREQ_HOST_FORMAT: u8 = 0x00;
pub const GS_USB_BREQ_BITTIMING: u8 = 0x01;
pub const GS_USB_BREQ_MODE: u8 = 0x02;
pub const _GS_USB_BREQ_BERR: u8 = 0x03;
pub const GS_USB_BREQ_BT_CONST: u8 = 0x04;
pub const GS_USB_BREQ_DEVICE_CONFIG: u8 = 0x05;
pub const GS_USB_BREQ_TIMESTAMP: u8 = 0x06;
pub const _GS_USB_BREQ_IDENTIFY: u8 = 0x07;
pub const _GS_USB_BREQ_GET_USER_ID: u8 = 0x08; // not implemented
pub const _GS_USB_BREQ_SET_USER_ID: u8 = 0x09; // not implemented
pub const _GS_USB_BREQ_DATA_BITTIMING: u8 = 0x0A;
pub const GS_USB_BREQ_BT_CONST_EXT: u8 = 0x0B;
pub const _GS_USB_BREQ_SET_TERMINATION: u8 = 0x0C;
pub const _GS_USB_BREQ_GET_TERMINATION: u8 = 0x0D;
pub const _GS_USB_BREQ_GET_STATE: u8 = 0x0E;

//
// gs_can_mode (command) — enum values
// (reset vs start)
//
pub const GS_CAN_MODE_RESET: u32 = 0;
pub const GS_CAN_MODE_START: u32 = 1;

//
// gs_device_mode.flags — bit flags
// (match the GS_CAN_MODE_* flag defines in the header)
//
pub const GS_CAN_MODE_LISTEN_ONLY: u32 = 1 << 0; // 0x0001
pub const GS_CAN_MODE_LOOP_BACK: u32 = 1 << 1; // 0x0002
pub const GS_CAN_MODE_TRIPLE_SAMPLE: u32 = 1 << 2; // 0x0004
pub const GS_CAN_MODE_ONE_SHOT: u32 = 1 << 3; // 0x0008
pub const GS_CAN_MODE_HW_TIMESTAMP: u32 = 1 << 4; // 0x0010
pub const GS_CAN_MODE_PAD_PKTS_TO_MAX_PKT_SIZE: u32 = 1 << 7; // 0x0080
pub const GS_CAN_MODE_FD: u32 = 1 << 8; // 0x0100
pub const GS_CAN_MODE_BERR_REPORTING: u32 = 1 << 12; // 0x1000

//
// gs_can_feature flags (reported by BT_CONST/BT_CONST_EXT)
// (mirror the GS_CAN_FEATURE_* defines in the header)
//
pub const GS_CAN_FEATURE_LISTEN_ONLY: u32 = 1 << 0;
pub const GS_CAN_FEATURE_LOOP_BACK: u32 = 1 << 1;
pub const GS_CAN_FEATURE_TRIPLE_SAMPLE: u32 = 1 << 2;
pub const GS_CAN_FEATURE_ONE_SHOT: u32 = 1 << 3;
pub const GS_CAN_FEATURE_HW_TIMESTAMP: u32 = 1 << 4;
pub const GS_CAN_FEATURE_IDENTIFY: u32 = 1 << 5;
pub const GS_CAN_FEATURE_USER_ID: u32 = 1 << 6;
pub const GS_CAN_FEATURE_PAD_PKTS_TO_MAX_PKT_SIZE: u32 = 1 << 7;
pub const GS_CAN_FEATURE_FD: u32 = 1 << 8;
pub const GS_CAN_FEATURE_REQ_USB_QUIRK_LPC546XX: u32 = 1 << 9;
pub const GS_CAN_FEATURE_BT_CONST_EXT: u32 = 1 << 10;
pub const GS_CAN_FEATURE_TERMINATION: u32 = 1 << 11;
pub const GS_CAN_FEATURE_BERR_REPORTING: u32 = 1 << 12;
pub const GS_CAN_FEATURE_GET_STATE: u32 = 1 << 13;

//
// Per-frame flags (gs_host_frame.flags)
//
pub const GS_CAN_FLAG_OVERFLOW: u8 = 1 << 0;
pub const GS_CAN_FLAG_FD: u8 = 1 << 1; // is a CAN-FD frame
pub const GS_CAN_FLAG_BRS: u8 = 1 << 2; // bit rate switch (CAN-FD)
pub const GS_CAN_FLAG_ESI: u8 = 1 << 3; // error state indicator (CAN-FD)

//
// CAN ID flags/masks (match <linux/can.h>)
//
pub const CAN_EFF_FLAG: u32 = 0x8000_0000; // extended frame format
pub const CAN_RTR_FLAG: u32 = 0x4000_0000; // remote transmission request
pub const CAN_ERR_FLAG: u32 = 0x2000_0000; // error frame

pub const CAN_SFF_MASK: u32 = 0x0000_07FF; // standard id mask (11-bit)
pub const CAN_EFF_MASK: u32 = 0x1FFF_FFFF; // extended id mask (29-bit)
pub const CAN_ERR_MASK: u32 = 0x1FFF_FFFF; // error mask

//
// Bit-timing defaults
//
pub const TARGET_SAMPLE_POINT: f64 = 0.875;

//
// Helpers: control request types and timeout conversion
//
#[inline]
pub fn request_type_out() -> u8 {
    (LIBUSB_REQUEST_TYPE_VENDOR | LIBUSB_RECIPIENT_INTERFACE | LIBUSB_ENDPOINT_OUT) as u8
}

#[inline]
pub fn request_type_in() -> u8 {
    (LIBUSB_REQUEST_TYPE_VENDOR | LIBUSB_RECIPIENT_INTERFACE | LIBUSB_ENDPOINT_IN) as u8
}

#[inline]
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
