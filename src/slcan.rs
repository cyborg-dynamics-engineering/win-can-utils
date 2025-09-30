/// Provides the SlcanDriver that exposes a serial port as a CAN interface.
use async_trait::async_trait;
use crosscan::can::CanFrame;
use memchr::memchr;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, split};
use tokio::sync::Mutex;
use tokio::time::timeout;
use tokio_serial::SerialStream;

use crate::can_driver::CanDriver;

pub struct SlcanDriver {
    reader: Mutex<tokio::io::ReadHalf<SerialStream>>,
    writer: Mutex<tokio::io::WriteHalf<SerialStream>>,
    leftover: Vec<u8>, // Buffer to store partial incoming data between reads
    timestamp_high: u32,
    configured_bitrate: Option<u32>,
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
        if line.is_empty() {
            return None;
        }

        fn parse_hex_u32(slice: &[u8]) -> Option<u32> {
            std::str::from_utf8(slice)
                .ok()
                .and_then(|s| u32::from_str_radix(s, 16).ok())
        }

        let has_timestamp = if line.len() > 1 {
            match line[0] as char {
                't' => {
                    let len = (line[4] as char).to_digit(10)? as usize;
                    line.len() == 5 + len * 2 + 8 + 1
                }
                'T' => {
                    let len = (line[9] as char).to_digit(10)? as usize;
                    line.len() == 10 + len * 2 + 8 + 1
                }
                _ => false,
            }
        } else {
            false
        };

        let (id, extended, dlc, data_start, ts_start) = match line[0] as char {
            't' => {
                if line.len() < 5 {
                    return None;
                }
                let id = u32::from_str_radix(std::str::from_utf8(&line[1..4]).ok()?, 16).ok()?;
                let dlc = (line[4] as char).to_digit(10)? as usize;
                (id, false, dlc, 5, 5 + dlc * 2)
            }
            'T' => {
                if line.len() < 10 {
                    return None;
                }
                let id = u32::from_str_radix(std::str::from_utf8(&line[1..9]).ok()?, 16).ok()?;
                let dlc = (line[9] as char).to_digit(10)? as usize;
                (id, true, dlc, 10, 10 + dlc * 2)
            }
            'J' => {
                *timestamp_high = timestamp_high.wrapping_add(1);
                return None;
            }
            _ => return None,
        };

        if line.len() < data_start + dlc * 2 {
            return None;
        }

        let mut data = Vec::with_capacity(dlc);
        for i in 0..dlc {
            let start = data_start + i * 2;
            let byte =
                u8::from_str_radix(std::str::from_utf8(&line[start..start + 2]).ok()?, 16).ok()?;
            data.push(byte);
        }

        let timestamp = if has_timestamp && line.len() >= ts_start + 8 {
            parse_hex_u32(&line[ts_start..ts_start + 8])
                .map(|low| ((u64::from(*timestamp_high) << 32) | u64::from(low)))
        } else {
            None
        };

        let mut frame = if extended {
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
        let timeout = Duration::from_millis(500);

        while received < 4 {
            if start.elapsed() > timeout {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "Timed out waiting for bitrate bytes",
                ));
            }

            // Await instead of busy-looping
            let num_bytes = {
                let mut reader = self.reader.lock().await;
                reader.read(&mut buf[received..]).await?
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
            cmd.push('T');
            cmd.push_str(&format!("{:08X}", frame.id()));
        } else {
            cmd.push('t');
            cmd.push_str(&format!("{:03X}", frame.id()));
        }

        cmd.push_str(&format!("{}", frame.dlc()));

        for byte in frame.data() {
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
            reader.read(&mut buf).await?
        };

        if num_bytes > 0 {
            self.leftover.extend_from_slice(&buf[..num_bytes]);
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
