use clap::{ArgAction, Parser};
use crosscan::can::CanFrame;
use crosscan::win_can::CanSocket;
use tokio::time::{Duration, sleep};

use std::num::ParseIntError;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// Minimal clap-based parser for `candump` (argument parsing only).
///
/// This mirrors the common candump command-line switches (parsing only)
/// and provides a structured configuration you can hook into real
/// candump-like behaviour later.
///
/// See: candump manpage for option semantics.
#[derive(Debug, Parser)]
#[command(name = "candump-rs")]
#[command(about = "candump-compatible CLI parser (args only)")]
pub struct Args {
    /// timestamp type: a (absolute), d (delta), z (zero), A (absolute w date)
    #[arg(short = 't', value_name = "type")]
    pub timestamp: Option<char>,

    /// read hardware timestamps instead of system timestamps
    #[arg(short = 'H', action = ArgAction::SetTrue)]
    pub hardware_ts: bool,

    /// increase color mode level (can be specified multiple times) (Not implemented yet)
    #[arg(short = 'c', action = ArgAction::Count, hide = true)]
    pub color_level: u8,

    /// binary output (may exceed 80 chars/line) (Not implemented yet)
    #[arg(short = 'i', action = ArgAction::SetTrue, hide = true)]
    pub binary_output: bool,

    /// enable additional ASCII output (Not implemented yet)
    #[arg(short = 'a', action = ArgAction::SetTrue, hide = true)]
    pub ascii: bool,

    /// swap byte order in printed CAN data (Not implemented yet)
    #[arg(short = 'S', action = ArgAction::SetTrue, hide = true)]
    pub swap_bytes: bool,

    /// silent mode level: 0 (off), 1 (animation), 2 (silent) (Not implemented yet)
    #[arg(short = 's', value_name = "level", hide = true)]
    pub silent_level: Option<u8>,

    /// log CAN frames into file (same as -f but toggles logging flag) (Not implemented yet)
    #[arg(short = 'l', action = ArgAction::SetTrue, hide = true)]
    pub log: bool,

    /// log filename (sets silent mode 2 by default in original candump) (Not implemented yet)
    #[arg(short = 'f', value_name = "fname", hide = true)]
    pub logfile: Option<String>,

    /// use log file format on stdout (Not implemented yet)
    #[arg(short = 'L', action = ArgAction::SetTrue, hide = true)]
    pub stdout_logformat: bool,

    /// terminate after reception of <count> CAN frames (Not implemented yet)
    #[arg(short = 'n', value_name = "count", hide = true)]
    pub count: Option<u64>,

    /// set socket receive buffer size (Not implemented yet)
    #[arg(short = 'r', value_name = "size", hide = true)]
    pub rcv_buf: Option<usize>,

    /// Don't exit if a "detected" CAN device goes down (Not implemented yet)
    #[arg(short = 'D', action = ArgAction::SetTrue, hide = true)]
    pub dont_exit_on_down: bool,

    /// monitor dropped CAN frames (Not implemented yet)
    #[arg(short = 'd', action = ArgAction::SetTrue, hide = true)]
    pub monitor_drops: bool,

    /// dump CAN error frames in human-readable format (Not implemented yet)
    #[arg(short = 'e', action = ArgAction::SetTrue, hide = true)]
    pub show_error_frames: bool,

    /// display raw DLC values in {} for Classical CAN (Not implemented yet)
    #[arg(short = '8', action = ArgAction::SetTrue, hide = true)]
    pub raw_dlc: bool,

    /// print extra message infos (rx/tx brs esi) (Not implemented yet)
    #[arg(short = 'x', action = ArgAction::SetTrue, hide = true)]
    pub extra_infos: bool,

    /// terminate after <msecs> if no frames were received (Not implemented yet)
    #[arg(short = 'T', value_name = "msecs", hide = true)]
    pub timeout_ms: Option<u64>,

    /// CAN interfaces with optional filter sets: <ifname>[,filter]*
    /// multiple interfaces allowed
    #[arg(required = true, value_name = "IF[,FILTER]*", num_args = 1..)]
    pub interfaces: Vec<String>,
}

/// Controls timestamp mode (from `-t` flag).
#[derive(Debug, Clone, Copy)]
pub enum TimestampMode {
    None,         // -t not specified
    Absolute,     // -t a
    AbsoluteDate, // -t A
    Delta,        // -t d
    Zero,         // -t z
}

impl TimestampMode {
    pub fn from_char(c: char) -> Option<Self> {
        match c {
            'a' => Some(Self::Absolute),
            'A' => Some(Self::AbsoluteDate),
            'd' => Some(Self::Delta),
            'z' => Some(Self::Zero),
            _ => None,
        }
    }
}

/// Holds state needed for timestamp calculations.
pub struct TimestampCtx {
    pub mode: TimestampMode,
    pub hardware: bool,
    start_instant: Instant,
    last_instant: Option<Instant>,
}

impl TimestampCtx {
    pub fn new(mode: TimestampMode, hardware: bool) -> Self {
        Self {
            mode,
            hardware,
            start_instant: Instant::now(),
            last_instant: None,
        }
    }

    /// Return timestamp in microseconds, depending on mode and hardware flag.
    pub fn get_timestamp(&mut self, frame: &CanFrame) -> Option<u64> {
        if self.hardware {
            // Hardware timestamp (already in µs)
            return Some(frame.timestamp().unwrap_or(0));
        }

        match self.mode {
            TimestampMode::None => None,
            TimestampMode::Absolute => {
                // system epoch time
                Some(
                    SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map(|d| d.as_micros() as u64)
                        .unwrap_or(0),
                )
            }
            TimestampMode::AbsoluteDate => {
                // system epoch time. TODO: This should display w/date.
                Some(
                    SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map(|d| d.as_micros() as u64)
                        .unwrap_or(0),
                )
            }
            TimestampMode::Delta => {
                let now = Instant::now();
                let ts = if let Some(last) = self.last_instant {
                    now.duration_since(last).as_micros() as u64
                } else {
                    0
                };
                self.last_instant = Some(now);
                Some(ts)
            }
            TimestampMode::Zero => {
                // relative to program start
                let now = Instant::now();
                Some(now.duration_since(self.start_instant).as_micros() as u64)
            }
        }
    }
}

/// A parsed filter from the candump filter grammar.
#[derive(Debug, Clone)]
pub enum Filter {
    /// <can_id>:<can_mask>
    Match { id: u32, mask: u32 },
    /// <can_id>~<can_mask>
    NotMatch { id: u32, mask: u32 },
    /// #<error_mask>
    ErrorMask(u32),
    /// Join flag: 'j' or 'J' means join filters (logical AND)
    Join,
}

pub struct InterfaceSpec {
    pub ifname: String,
    pub filters: Vec<Filter>,
    pub pipe: CanSocket,
}

impl InterfaceSpec {
    pub async fn parse_and_connect(spec: &str) -> Result<Self, String> {
        // format: ifname[,filter]*
        let mut parts = spec.split(',');
        let ifname = parts
            .next()
            .ok_or_else(|| "empty interface specification".to_string())?
            .to_string();
        let mut filters = Vec::new();
        for token in parts {
            let token = token.trim();
            if token.is_empty() {
                continue;
            }
            if token.eq_ignore_ascii_case("j") {
                filters.push(Filter::Join);
                continue;
            }
            if let Some(rest) = token.strip_prefix('#') {
                let val =
                    parse_hex(rest).map_err(|e| format!("invalid error mask '{}': {}", rest, e))?;
                filters.push(Filter::ErrorMask(val));
                continue;
            }
            if let Some((a, b)) = token.split_once(':') {
                let id = parse_hex(a).map_err(|e| format!("invalid can_id '{}': {}", a, e))?;
                let mask = parse_hex(b).map_err(|e| format!("invalid can_mask '{}': {}", b, e))?;
                filters.push(Filter::Match { id, mask });
                continue;
            }
            if let Some((a, b)) = token.split_once('~') {
                let id = parse_hex(a).map_err(|e| format!("invalid can_id '{}': {}", a, e))?;
                let mask = parse_hex(b).map_err(|e| format!("invalid can_mask '{}': {}", b, e))?;
                filters.push(Filter::NotMatch { id, mask });
                continue;
            }
            // If token looks like plain hex (e.g. 12345678), treat as id with default mask 0xFFFFFFFF
            if is_hex(token) {
                let id =
                    parse_hex(token).map_err(|e| format!("invalid hex id '{}': {}", token, e))?;
                filters.push(Filter::Match {
                    id,
                    mask: 0xFFFFFFFF,
                });
                continue;
            }

            return Err(format!("unrecognized filter token: '{}'", token));
        }

        let pipe = connect_pipe_retry(ifname.as_str()).await;

        Ok(InterfaceSpec {
            ifname,
            filters,
            pipe,
        })
    }
}

fn is_hex(s: &str) -> bool {
    s.chars().all(|c| c.is_ascii_hexdigit())
}

fn parse_hex(s: &str) -> Result<u32, ParseIntError> {
    // Accept leading 0x/0X or plain hex
    if let Some(rest) = s.strip_prefix("0x") {
        u32::from_str_radix(rest, 16)
    } else if let Some(rest) = s.strip_prefix("0X") {
        u32::from_str_radix(rest, 16)
    } else {
        u32::from_str_radix(s, 16)
    }
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let args = Args::parse();

    // Quick sanity: if -l or -f was given, mimic candump behaviour: sets silent mode 2 by default
    let _silent_level = if args.log || args.logfile.is_some() {
        Some(2)
    } else {
        args.silent_level
    };

    let ts_mode = args
        .timestamp
        .and_then(TimestampMode::from_char)
        .unwrap_or(TimestampMode::None);

    let mut ts_ctx = TimestampCtx::new(ts_mode, args.hardware_ts);

    // Parse interfaces
    let mut interfaces = Vec::new();
    for spec in &args.interfaces {
        match InterfaceSpec::parse_and_connect(spec).await {
            Ok(i) => {
                interfaces.push(i);
            }
            Err(e) => {
                eprintln!("error parsing interface spec '{}': {}", spec, e);
                std::process::exit(1);
            }
        }
    }

    loop {
        for interface in &mut interfaces {
            let frame = interface.pipe.read_frame().await?;

            // Get timestamp string. If no -t option, it will return an empty string ("").
            let ts_str = ts_ctx.get_timestamp(&frame).map_or(String::new(), |t| {
                format!("({}.{:06}) ", t / 1_000_000, t % 1_000_000)
            });

            let id = match frame.is_extended() {
                true => format!("{:08X}", frame.id()),
                false => format!("{:03X}", frame.id()),
            };

            println!(
                "{}{} {:>08}   [{}]  {}",
                ts_str,
                interface.ifname,
                id,
                frame.dlc(),
                frame
                    .data()
                    .iter()
                    .map(|b| format!("{:02X}", b))
                    .collect::<Vec<_>>()
                    .join(" "),
            );
        }
    }
}

async fn connect_pipe_retry(channel: &str) -> CanSocket {
    println!("Attempting to connect to {} server", channel);

    loop {
        match CanSocket::open_read_only(channel) {
            Ok(pipe) => {
                println!("Connected to {} server", channel);
                return pipe;
            }
            Err(_) => {
                println!("Unable to connect. Is a server running for {}?", channel);
                sleep(Duration::from_millis(500)).await;
            }
        };
    }
}
