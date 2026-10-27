use clap::{ArgAction, Parser};
use crosscan::{
    can::CanFrame,
    protocol::{Command, Direction, Filter as ProtocolFilter, Reply, Subscription},
    service::ServiceClient,
};
use futures::future::join_all;
use tokio::task;
use tokio::time::Duration;

use std::num::ParseIntError;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// Display traffic from one or more Windows CAN service interfaces.
#[derive(Debug, Parser)]
#[command(name = "candump")]
#[command(about = "Display CAN traffic with per-client queues and filters")]
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

    /// Terminate after this many frames per interface
    #[arg(short = 'n', value_name = "count")]
    pub count: Option<u64>,

    /// set socket receive buffer size (Not implemented yet)
    #[arg(short = 'r', value_name = "size", hide = true)]
    pub rcv_buf: Option<usize>,

    /// Don't exit if a "detected" CAN device goes down (Not implemented yet)
    #[arg(short = 'D', action = ArgAction::SetTrue, hide = true)]
    pub dont_exit_on_down: bool,

    /// Report changes to server and client dropped-frame counters
    #[arg(short = 'd', action = ArgAction::SetTrue)]
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

    #[arg(long, default_value_t = 1024)]
    pub queue_capacity: usize,

    #[arg(long)]
    pub receive_own_messages: bool,

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

pub struct CandumpInterface {
    pub ifname: String,
    pub pipe: ServiceClient,
}

impl CandumpInterface {
    pub async fn parse_and_connect(
        spec: &str,
        queue_capacity: usize,
        receive_own_messages: bool,
    ) -> Result<Self, String> {
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

        let subscription = Subscription {
            queue_capacity,
            receive_own_messages,
            filters: filters
                .iter()
                .filter_map(|filter| match filter {
                    Filter::Match { id, mask } => Some(ProtocolFilter {
                        id: *id,
                        mask: *mask,
                        invert: false,
                    }),
                    Filter::NotMatch { id, mask } => Some(ProtocolFilter {
                        id: *id,
                        mask: *mask,
                        invert: true,
                    }),
                    _ => None,
                })
                .collect(),
            join_filters: filters.iter().any(|filter| matches!(filter, Filter::Join)),
            error_mask: filters.iter().fold(0, |mask, filter| {
                if let Filter::ErrorMask(value) = filter {
                    mask | value
                } else {
                    mask
                }
            }),
            ..Default::default()
        };
        let pipe = ServiceClient::connect(&ifname, subscription)
            .await
            .map_err(|e| e.to_string())?;

        Ok(CandumpInterface { ifname, pipe })
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

    // Parse interfaces
    let mut interfaces = Vec::new();
    for spec in &args.interfaces {
        match CandumpInterface::parse_and_connect(
            spec,
            args.queue_capacity,
            args.receive_own_messages,
        )
        .await
        {
            Ok(i) => {
                interfaces.push(i);
            }
            Err(e) => {
                eprintln!("error parsing interface spec '{}': {}", spec, e);
                std::process::exit(1);
            }
        }
    }

    run_interfaces(
        interfaces,
        ts_mode,
        args.hardware_ts,
        args.monitor_drops,
        args.count,
    )
    .await
    .map_err(std::io::Error::other)?;

    Ok(())
}

async fn run_interfaces(
    interfaces: Vec<CandumpInterface>,
    ts_mode: TimestampMode,
    hardware_ts: bool,
    monitor_drops: bool,
    count: Option<u64>,
) -> anyhow::Result<()> {
    let mut handles = Vec::new();

    for mut interface in interfaces.into_iter() {
        let mut ts_ctx = TimestampCtx::new(ts_mode, hardware_ts);

        let handle = task::spawn(async move {
            let mut timer = tokio::time::interval(Duration::from_secs(1));
            let mut last_drops = (0, 0);
            let mut received = 0;
            if count == Some(0) {
                return Ok::<(), anyhow::Error>(());
            }
            loop {
                let event = tokio::select! {
                    event = interface.pipe.next_event() => event,
                    _ = timer.tick(), if monitor_drops => {
                        let server_drops = match interface.pipe.request(Command::Clients).await? {
                            Reply::Clients { clients } => clients.iter().find(|c| c.client_id == interface.pipe.client_id).map_or(0, |c| c.dropped),
                            _ => 0,
                        };
                        let drops = (server_drops, interface.pipe.local_drops());
                        if drops != last_drops {
                            eprintln!("{}: dropped {} at server, {} in client", interface.ifname, drops.0, drops.1);
                            last_drops = drops;
                        }
                        continue;
                    }
                };
                match event {
                    Ok(event) => {
                        let mut frame = event.frame.to_can()?;
                        frame.set_timestamp(event.hardware_timestamp_us);

                        // timestamp string
                        let ts_str = ts_ctx.get_timestamp(&frame).map_or(String::new(), |t| {
                            format!("({:03}.{:06}) ", t / 1_000_000, t % 1_000_000)
                        });

                        // CAN ID string
                        let id = if frame.is_extended() {
                            format!("{:08X}", frame.id())
                        } else {
                            format!("{:03X}", frame.id())
                        };

                        println!(
                            "{}{} {:>08}   [{}]  {}{}",
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
                            if event.direction == Direction::LocalTx {
                                "  TX"
                            } else {
                                ""
                            }
                        );
                        received += 1;
                        if count.is_some_and(|count| received >= count) {
                            break;
                        }
                    }
                    Err(e) => {
                        return Err(e.into());
                    }
                }
            }
            Ok::<(), anyhow::Error>(())
        });

        handles.push(handle);
    }

    for result in join_all(handles).await {
        result??;
    }

    Ok(())
}
