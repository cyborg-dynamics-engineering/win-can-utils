/// Provides the SlcanDriver that exposes a serial port as a CAN interface.
use async_trait::async_trait;
use crosscan::can::CanFrame;
use memchr::memchr;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, split};
use tokio::sync::Mutex;
use tokio::time::timeout;
use tokio_serial::SerialStream;

use crate::drivers::CanDriver;

pub struct SlcanDriver {
    reader: Mutex<tokio::io::ReadHalf<SerialStream>>,
    writer: Mutex<tokio::io::WriteHalf<SerialStream>>,
    leftover: Vec<u8>, // Buffer to store partial incoming data between reads
    timestamp_high: u32,
    configured_bitrate: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::SlcanDriver;

    #[test]
    fn truncated_records_never_index_past_the_header() {
        for line in [b"t12320102000000FF\r".as_slice(), b"T0000012320102\r"] {
            for length in 0..5.min(line.len()) {
                assert!(SlcanDriver::parse_slcan_line_bytes(&mut 0, &line[..length]).is_none());
            }
        }
        assert!(SlcanDriver::parse_slcan_line_bytes(&mut 0, b"t1239\r").is_none());
    }

    #[test]
    fn timestamp_and_remote_frames_preserve_metadata() {
        let mut high = 0;
        SlcanDriver::parse_slcan_line_bytes(&mut high, b"J\r");
        let frame = SlcanDriver::parse_slcan_line_bytes(&mut high, b"t12320102000000FF\r").unwrap();
        assert_eq!(frame.data(), &[1, 2]);
        assert_eq!(frame.timestamp(), Some(0x1_0000_00ff));
        let remote = SlcanDriver::parse_slcan_line_bytes(&mut high, b"r1238\r").unwrap();
        assert!(remote.is_rtr());
        assert_eq!(remote.dlc(), 8);
    }
}

impl SlcanDriver {
    /// Open serial port and initialize driver, optionally enabling SLCAN timestamp
    pub async fn open(port_name: &str) -> std::io::Result<Self> {
        let builder = tokio_serial::new(port_name, 2_500_000);
        let port = SerialStream::open(&builder)?;

        let (reader, writer) = split(port);

        Ok(SlcanDriver {
            reader: Mutex::new(reader),
            writer: Mutex::new(writer),
            leftover: Vec::with_capacity(8192),
            timestamp_high: 0,
            configured_bitrate: None,
        })
    }

    /// Parse SLCAN frame line from bytes, optionally with timestamp
    fn parse_slcan_line_bytes(timestamp_high: &mut u32, line: &[u8]) -> Option<CanFrame> {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let kind = *line.first()?;
        if kind == b'J' {
            *timestamp_high = timestamp_high.wrapping_add(1);
            return None;
        }
        let (extended, remote, header) = match kind {
            b't' => (false, false, 5),
            b'T' => (true, false, 10),
            b'r' => (false, true, 5),
            b'R' => (true, true, 10),
            _ => return None,
        };
        if line.len() < header {
            return None;
        }
        let id = u32::from_str_radix(std::str::from_utf8(&line[1..header - 1]).ok()?, 16).ok()?;
        let dlc = (line[header - 1] as char).to_digit(10)? as usize;
        if dlc > 8 {
            return None;
        }
        let data_end = header + if remote { 0 } else { dlc * 2 };
        if line.len() < data_end {
            return None;
        }
        let timestamp = match line.len() - data_end {
            0 => None,
            8 => {
                let low =
                    u32::from_str_radix(std::str::from_utf8(&line[data_end..]).ok()?, 16).ok()?;
                Some((u64::from(*timestamp_high) << 32) | u64::from(low))
            }
            _ => return None,
        };
        let mut data = Vec::with_capacity(dlc);
        for pair in line[header..data_end].chunks_exact(2) {
            data.push(u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?);
        }
        let mut frame = if remote {
            CanFrame::new_remote(id, dlc, extended).ok()?
        } else if extended {
            CanFrame::new_eff(id, &data).ok()?
        } else {
            CanFrame::new(id, &data).ok()?
        };
        frame.set_timestamp(timestamp);
        Some(frame)
    }

    pub async fn get_measured_bitrate(&mut self) -> std::io::Result<u32> {
        const SUPPORTED_BITRATES: [u32; 9] = [
            10_000, 20_000, 50_000, 100_000, 125_000, 250_000, 500_000, 800_000, 1_000_000,
        ];

        self.leftover.clear();
        // Request bitrate
        {
            let mut writer = self.writer.lock().await;
            writer.write_all(b"B\r").await?;
            writer.flush().await?;
        }

        let mut buf = [0u8; 4];
        let mut received = 0;
        let start = tokio::time::Instant::now();
        let deadline = Duration::from_millis(500);

        while received < 4 {
            if start.elapsed() > deadline {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "Timed out waiting for bitrate bytes",
                ));
            }

            // Await instead of busy-looping
            let num_bytes = {
                let mut reader = self.reader.lock().await;
                timeout(
                    Duration::from_millis(500),
                    reader.read(&mut buf[received..]),
                )
                .await
                .map_err(|_| {
                    std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "Timed out detecting SLCAN bitrate",
                    )
                })??
            };

            if num_bytes > 0 {
                received += num_bytes;
            } else {
                // yield briefly
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }

        let actual = u32::from_le_bytes(buf);
        if actual < 5000 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "Error detecting bitrate. Has the device been modified correctly?",
            ));
        }

        // Find closest supported bitrate
        let closest = *SUPPORTED_BITRATES
            .iter()
            .min_by_key(|&&rate| (rate as i64 - actual as i64).abs())
            .unwrap();

        Ok(closest)
    }

    pub async fn get_version(&mut self) -> std::io::Result<String> {
        self.leftover.clear();

        {
            let mut writer = self.writer.lock().await;
            writer.write_all(b"V\r").await?;
            writer.flush().await?;
        }

        let mut reader = self.reader.lock().await;
        let mut reader = BufReader::new(&mut *reader);
        let mut buf = Vec::new();

        // Wait for response with a timeout
        let res = timeout(Duration::from_millis(20), async {
            loop {
                buf.clear();
                let n = reader.read_until(b'\r', &mut buf).await?;

                if n == 0 {
                    // EOF
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "Device closed connection",
                    ));
                }

                // Skip "T" lines (CAN frames)
                if buf.get(0) == Some(&b'T') {
                    continue;
                }

                return Ok(String::from_utf8_lossy(&buf).trim().to_string());
            }
        })
        .await;

        match res {
            Ok(inner) => inner,
            Err(_) => Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "Timeout waiting for SLCAN version response",
            )),
        }
    }
}

#[async_trait]
impl CanDriver for SlcanDriver {
    fn capabilities(&self) -> crosscan::protocol::Capabilities {
        crosscan::protocol::Capabilities {
            hardware_can_fd: None,
            can_fd: false,
            hardware_timestamps: None,
            bus_state: false,
            software_tx_echo: true,
        }
    }
    /// Enable timestamp support on the SLCAN device
    async fn enable_timestamp(&mut self) -> std::io::Result<()> {
        let mut writer = self.writer.lock().await;
        writer.write_all(b"Z1\r").await?;
        Ok(())
    }

    async fn set_bitrate(&mut self, bitrate: u32) -> std::io::Result<()> {
        let cmd = match bitrate {
            10_000 => b"S0\r",
            20_000 => b"S1\r",
            50_000 => b"S2\r",
            100_000 => b"S3\r",
            125_000 => b"S4\r",
            250_000 => b"S5\r",
            500_000 => b"S6\r",
            800_000 => b"S7\r",
            1_000_000 => b"S8\r",
            _ => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("Unsupported CAN bitrate: {}", bitrate),
                ));
            }
        };

        self.configured_bitrate = Some(bitrate);
        let mut writer = self.writer.lock().await;
        writer.write_all(cmd).await
    }

    async fn open_channel(&mut self) -> std::io::Result<()> {
        let mut writer = self.writer.lock().await;
        writer.write_all(b"O\r").await // Open CAN channel
    }

    async fn send_frame(&mut self, frame: &CanFrame) -> std::io::Result<()> {
        let mut cmd = String::with_capacity(20 + frame.data().len() * 2);

        if frame.is_extended() {
            cmd.push(if frame.is_rtr() { 'R' } else { 'T' });
            cmd.push_str(&format!("{:08X}", frame.id()));
        } else {
            cmd.push(if frame.is_rtr() { 'r' } else { 't' });
            cmd.push_str(&format!("{:03X}", frame.id()));
        }

        cmd.push_str(&format!("{}", frame.dlc()));

        for byte in if frame.is_rtr() {
            &[][..]
        } else {
            frame.data()
        } {
            cmd.push_str(&format!("{:02X}", byte));
        }

        cmd.push('\r');
        let mut writer = self.writer.lock().await;
        writer.write_all(cmd.as_bytes()).await
    }

    async fn read_frames(&mut self) -> std::io::Result<Vec<CanFrame>> {
        let mut buf = [0u8; 4096];
        let mut frames = Vec::new();

        let num_bytes = {
            let mut reader = self.reader.lock().await;
            // Bound idle serial waits so interface commands and TX remain available.
            match timeout(Duration::from_millis(5), reader.read(&mut buf)).await {
                Ok(result) => {
                    let count = result?;
                    if count == 0 {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::UnexpectedEof,
                            "SLCAN serial port closed",
                        ));
                    }
                    count
                }
                Err(_) => return Ok(frames),
            }
        };

        if num_bytes > 0 {
            self.leftover.extend_from_slice(&buf[..num_bytes]);
            if self.leftover.len() > 65_536 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "SLCAN input exceeds the line buffer limit",
                ));
            }
        }

        let mut processed = 0;
        while let Some(relative_pos) = memchr(b'\r', &self.leftover[processed..]) {
            let end = processed + relative_pos + 1;
            let line = &self.leftover[processed..end];
            if let Some(frame) = Self::parse_slcan_line_bytes(&mut self.timestamp_high, line) {
                frames.push(frame);
            }
            processed = end;
        }

        if processed > 0 {
            self.leftover.drain(..processed);
        }

        Ok(frames)
    }

    /// Close the CAN channel cleanly
    async fn close_channel(&mut self) -> std::io::Result<()> {
        let mut writer = self.writer.lock().await;
        writer.write_all(b"C\r").await?;
        writer.flush().await?;
        Ok(())
    }

    async fn get_bitrate(&self) -> Option<u32> {
        self.configured_bitrate
    }
}
