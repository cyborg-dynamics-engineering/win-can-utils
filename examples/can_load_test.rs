//! Sequence-checked end-to-end load test through two running canserver pipes.
use clap::Parser;
use crosscan::{CanInterface, can::CanFrame, win_can::WindowsCan};
use serde_json::json;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Parser)]
struct Args {
    #[arg(long)]
    source: String,
    #[arg(long)]
    destination: String,
    #[arg(long, default_value_t = 60)]
    seconds: u64,
    /// Frames per second to submit; zero sends as fast as the pipe accepts.
    #[arg(long, default_value_t = 0)]
    rate: u32,
    #[arg(long, default_value_t = 1_000_000)]
    bitrate: u32,
}

#[derive(Default)]
struct Received {
    count: u64,
    seen: Vec<bool>,
    duplicates: u64,
    out_of_order: u64,
    bad_payloads: u64,
    other_frames: u64,
    error_frames: u64,
    timestamp_regressions: u64,
    timestamped: u64,
    previous_timestamp: Option<u64>,
    previous_sequence: Option<u32>,
    first: Option<Instant>,
    last: Option<Instant>,
    read_error: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    anyhow::ensure!(args.seconds > 0, "--seconds must be positive");
    let mut writer = WindowsCan::open_write_only(&args.source)?;
    let mut reader = WindowsCan::open_read_only(&args.destination)?;
    let received = Arc::new(Mutex::new(Received::default()));
    let receiver_stats = received.clone();
    let receiver = tokio::spawn(async move {
        loop {
            let frame = match reader.read_frame().await {
                Ok(frame) => frame,
                Err(error) => {
                    receiver_stats.lock().unwrap().read_error = Some(error.to_string());
                    return;
                }
            };
            let mut stats = receiver_stats.lock().unwrap();
            if frame.is_error() {
                stats.error_frames += 1;
                continue;
            }
            if frame.id() != 0x5a0 || frame.is_extended() || frame.is_rtr() || frame.dlc() != 8 {
                stats.other_frames += 1;
                continue;
            }
            let seq = u32::from_le_bytes(frame.data()[..4].try_into().unwrap());
            let check = u32::from_le_bytes(frame.data()[4..].try_into().unwrap());
            if check != !seq || seq > 10_000_000 {
                stats.bad_payloads += 1;
                continue;
            }
            let now = Instant::now();
            stats.first.get_or_insert(now);
            stats.last = Some(now);
            stats.count += 1;
            if stats.seen.len() <= seq as usize {
                stats.seen.resize(seq as usize + 1, false);
            }
            if std::mem::replace(&mut stats.seen[seq as usize], true) {
                stats.duplicates += 1;
            }
            if stats
                .previous_sequence
                .is_some_and(|previous| seq < previous)
            {
                stats.out_of_order += 1;
            }
            stats.previous_sequence = Some(seq);
            if let Some(timestamp) = frame.timestamp() {
                stats.timestamped += 1;
                if stats
                    .previous_timestamp
                    .is_some_and(|previous| timestamp < previous)
                {
                    stats.timestamp_regressions += 1;
                }
                stats.previous_timestamp = Some(timestamp);
            }
        }
    });

    let mut pacing = (args.rate > 0)
        .then(|| tokio::time::interval(Duration::from_secs_f64(1.0 / f64::from(args.rate))));
    if let Some(pacing) = &mut pacing {
        pacing.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    }
    let begin = Instant::now();
    let mut submitted = 0u32;
    let mut write_error = None;
    while begin.elapsed() < Duration::from_secs(args.seconds) {
        if let Some(pacing) = &mut pacing {
            pacing.tick().await;
        }
        let mut payload = [0; 8];
        payload[..4].copy_from_slice(&submitted.to_le_bytes());
        payload[4..].copy_from_slice(&(!submitted).to_le_bytes());
        let frame = CanFrame::new(0x5a0, &payload).unwrap();
        match tokio::time::timeout(Duration::from_secs(5), writer.write_frame(frame)).await {
            Ok(Ok(())) => submitted += 1,
            error => {
                write_error = Some(format!("{error:?}"));
                break;
            }
        }
    }
    let send_seconds = begin.elapsed().as_secs_f64();
    drop(writer);
    // Allow queued pipe/USB/CAN traffic to drain without cancelling partial reads.
    let drain = Instant::now();
    while drain.elapsed() < Duration::from_secs(15) {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let stats = received.lock().unwrap();
        let quiet = stats
            .last
            .is_some_and(|last| last.elapsed() >= Duration::from_millis(500));
        if quiet || stats.read_error.is_some() {
            break;
        }
    }
    receiver.abort();
    let _ = receiver.await;
    let stats = received.lock().unwrap();
    let missing: Vec<_> = (0..submitted)
        .filter(|seq| !stats.seen.get(*seq as usize).copied().unwrap_or(false))
        .collect();
    let receive_seconds = match (stats.first, stats.last) {
        (Some(first), Some(last)) => (last - first).as_secs_f64(),
        _ => 0.0,
    };
    let fps = if receive_seconds > 0.0 {
        (stats.count.saturating_sub(1)) as f64 / receive_seconds
    } else {
        0.0
    };
    let success = missing.is_empty()
        && stats.duplicates == 0
        && stats.out_of_order == 0
        && stats.bad_payloads == 0
        && stats.error_frames == 0
        && stats.other_frames == 0
        && stats.timestamp_regressions == 0
        && stats.read_error.is_none()
        && write_error.is_none();
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "source": args.source, "destination": args.destination, "bitrate": args.bitrate,
            "requested_fps": args.rate, "submitted": submitted, "received": stats.count,
            "missing": missing.len(), "missing_sample": missing.iter().take(20).collect::<Vec<_>>(),
            "duplicates": stats.duplicates, "out_of_order": stats.out_of_order,
            "bad_payloads": stats.bad_payloads, "error_frames": stats.error_frames,
            "other_frames": stats.other_frames, "timestamp_regressions": stats.timestamp_regressions,
            "timestamped": stats.timestamped,
            "send_seconds": send_seconds, "receive_seconds": receive_seconds,
            "received_fps": fps, "bus_load_min_percent": fps * 111.0 / f64::from(args.bitrate) * 100.0,
            "bus_load_max_percent": (fps * 135.0 / f64::from(args.bitrate) * 100.0).min(100.0),
            "read_error": stats.read_error, "write_error": write_error, "success": success
        }))?
    );
    anyhow::ensure!(
        success,
        "load test found missing, duplicate, corrupt or reordered frames"
    );
    Ok(())
}
