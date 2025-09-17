/// Provides the SlcanDriver that exposes a serial port as a CAN interface.
use crosscan::can::CanFrame;
use serialport::SerialPort;
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::time::Duration;

pub struct SlcanDriver {
    port: Box<dyn SerialPort>,
    leftover: Vec<u8>, // Buffer to store partial incoming data between reads
    timestamp_high: u32,
    configured_bitrate: Option<u32>,
}

impl SlcanDriver {
    pub fn try_clone(&self) -> std::io::Result<Self> {
        let cloned = self.port.try_clone()?;
        Ok(SlcanDriver {
            port: cloned,
            leftover: Vec::new(),
            timestamp_high: self.timestamp_high,
            configured_bitrate: self.configured_bitrate,
        })
    }

    /// Open serial port and initialize driver, optionally enabling SLCAN timestamp
    pub fn open(port_name: &str) -> std::io::Result<Self> {
        let port = serialport::new(port_name, 230_400)
            .timeout(Duration::from_millis(1))
            .open()?;

        Ok(SlcanDriver {
            port,
            leftover: Vec::with_capacity(4096),
            timestamp_high: 0,
            configured_bitrate: None,
        })
    }

    /// Enable timestamp support on the SLCAN device
    pub fn enable_timestamp(&mut self) -> std::io::Result<()> {
        self.port.write_all(b"Z1\r")?;
        Ok(())
    }

    pub fn set_can_bitrate(&mut self, bitrate: u32) -> std::io::Result<()> {
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
        self.port.write_all(cmd)
    }

    pub fn open_channel(&mut self) -> std::io::Result<()> {
        self.port.write_all(b"O\r") // Open CAN channel
    }

    pub fn send_frame(&mut self, frame: &CanFrame) -> std::io::Result<()> {
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
        self.port.write_all(cmd.as_bytes())
    }

    /// Read all available CAN frames from serial input
    pub fn read_frames(&mut self) -> std::io::Result<Vec<CanFrame>> {
        let mut buf = [0u8; 1024];
        let mut frames = Vec::new();

        loop {
            match self.port.read(&mut buf) {
                Ok(n) if n > 0 => {
                    self.leftover.extend_from_slice(&buf[..n]);

                    while let Some(pos) = self.leftover.iter().position(|&b| b == b'\r') {
                        let line = self.leftover.drain(..=pos).collect::<Vec<_>>();
                        if let Some(frame) = self.parse_slcan_line_bytes(&line) {
                            frames.push(frame);
                        }
                    }
                }
                Err(ref e)
                    if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut =>
                {
                    break;
                }
                Err(e) => return Err(e),
                _ => break,
            }
        }

        Ok(frames)
    }

    /// Parse SLCAN frame line from bytes, optionally with timestamp
    fn parse_slcan_line_bytes(&mut self, line: &[u8]) -> Option<CanFrame> {
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
                self.timestamp_high = self.timestamp_high.wrapping_add(1);
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
                .map(|low| ((self.timestamp_high as u64) << 32) | (low as u64))
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

    /// Close the CAN channel cleanly
    pub fn close_channel(&mut self) -> std::io::Result<()> {
        self.port.write_all(b"C\r")?;
        self.port.flush()?;
        Ok(())
    }

    pub fn get_configured_bitrate(&self) -> Option<u32> {
        self.configured_bitrate
    }

    pub fn get_measured_bitrate(&mut self) -> std::io::Result<u32> {
        const SUPPORTED_BITRATES: [u32; 9] = [
            10_000, 20_000, 50_000, 100_000, 125_000, 250_000, 500_000, 800_000, 1_000_000,
        ];

        self.leftover.clear();

        self.port.write_all(b"B\r")?;
        self.port.flush()?; // ensure it's sent

        let mut buf = [0u8; 4];
        let mut received = 0;
        let start = std::time::Instant::now();
        let timeout = Duration::from_millis(500);

        while received < 4 {
            if start.elapsed() > timeout {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "Timed out waiting for bitrate bytes",
                ));
            }

            match self.port.read(&mut buf[received..]) {
                Ok(n) if n > 0 => received += n,
                Ok(_) => continue,
                Err(ref e)
                    if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut =>
                {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(e) => return Err(e),
            }
        }

        let actual = u32::from_le_bytes(buf);

        if actual < 5000 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "Error detecting bitrate. Has the device been modified correctly?",
            ));
        }

        // Find the closest supported bitrate
        let closest = *SUPPORTED_BITRATES
            .iter()
            .min_by_key(|&&rate| (rate as i64 - actual as i64).abs())
            .unwrap(); // Safe unwrap — list is non-empty

        Ok(closest)
    }

    pub fn get_version(&mut self) -> std::io::Result<String> {
        self.leftover.clear();

        self.port.write_all(b"V\r")?;
        self.port.flush()?;

        let mut reader = BufReader::new(&mut self.port);
        let mut buf = Vec::new();

        loop {
            reader.read_until(b'\r', &mut buf)?;

            if buf[0] != b'T' {
                break;
            }

            buf = Vec::new();
        }

        Ok(String::from_utf8_lossy(&buf).trim().to_string())
    }
}
