use async_trait::async_trait;
use crosscan::can::CanFrame;
use std::io;
use std::sync::Arc;
use tokio::sync::Notify;

#[async_trait]
pub trait CanDriver: Send + Sync {
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
