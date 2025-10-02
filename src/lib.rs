/// Collection of supported CAN drivers.
pub mod drivers;
pub use drivers::{CanDriver, GsUsbDriver, PcanDriver, SlcanDriver};
/// We'll create this instead of thread_manager.rs
pub mod thread_manager_async;
