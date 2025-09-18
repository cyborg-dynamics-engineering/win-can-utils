/// Exposes a serial port as a CAN interface.
pub mod slcan;
pub use slcan::SlcanDriver;

pub mod can_driver;
pub use can_driver::CanDriver;
/// We'll create this instead of thread_manager.rs
pub mod thread_manager_async;
