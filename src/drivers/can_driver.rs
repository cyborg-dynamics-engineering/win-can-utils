use async_trait::async_trait;
use crosscan::can::CanFrame;
use std::io;

#[async_trait]
pub trait CanDriver: Send + Sync {
    async fn enable_timestamp(&mut self) -> std::io::Result<()>;

    async fn set_bitrate(&mut self, bitrate: u32) -> io::Result<()>;

    async fn get_bitrate(&self) -> Option<u32>;

    async fn open_channel(&mut self) -> io::Result<()>;

    async fn send_frame(&mut self, frame: &CanFrame) -> io::Result<()>;

    async fn read_frames(&mut self) -> io::Result<Vec<CanFrame>>;

    async fn close_channel(&mut self) -> io::Result<()>;
}
