/// Exposes a serial port as a CAN interface.
pub mod slcan;
pub use slcan::SlcanDriver;

pub mod can_driver;
pub mod gs_usb;
pub mod pcan;
pub use can_driver::CanDriver;
pub use gs_usb::GsUsbDriver;
pub use pcan::PcanDriver;
/// We'll create this instead of thread_manager.rs
pub mod thread_manager_async;
