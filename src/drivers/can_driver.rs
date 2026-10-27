use async_trait::async_trait;
use crosscan::can::CanFrame;
use crosscan::protocol::{BusState, Capabilities};
use std::io;
use std::sync::Arc;
use tokio::sync::Notify;

#[derive(Clone, Debug, Default)]
pub struct DriverHealth {
    pub bus_state: BusState,
    pub rx_error_counter: Option<u32>,
    pub tx_error_counter: Option<u32>,
}

#[async_trait]
pub trait CanDriver: Send + Sync {
    fn capabilities(&self) -> Capabilities {
        Capabilities::default()
    }

    /// Unknown fields remain unknown; querying must not reconfigure the device.
    async fn health(&mut self) -> io::Result<DriverHealth> {
        Ok(DriverHealth::default())
    }

    async fn enable_timestamp(&mut self) -> std::io::Result<()>;

    async fn set_bitrate(&mut self, bitrate: u32) -> io::Result<()>;

    async fn get_bitrate(&self) -> Option<u32>;

    async fn open_channel(&mut self) -> io::Result<()>;

    /// Submit a frame. Success can mean queue admission, not a CAN acknowledgement.
    /// Background transport failures are reported by subsequent driver operations.
    async fn send_frame(&mut self, frame: &CanFrame) -> io::Result<()>;

    async fn read_frames(&mut self) -> io::Result<Vec<CanFrame>>;

    /// Optional wake signal for a nonblocking `read_frames` implementation.
    ///
    /// Notify one waiter after queueing frames and when reception stops. The
    /// single reader must drain the queue before waiting, outside any driver
    /// lock. `Notify` retains a permit if data arrives between reading and waiting.
    fn receive_notifier(&self) -> Option<Arc<Notify>> {
        None
    }

    async fn close_channel(&mut self) -> io::Result<()>;
}
