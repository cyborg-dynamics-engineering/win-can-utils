use clap::Parser;
use crosscan::{CanInterface, can::CanFrame, win_can::WindowsCan};
use std::io;
use std::process;
use tokio::time::{Duration, sleep};

#[derive(Parser)]
struct Args {
    /// CAN channel name, e.g. COM1 or COM4
    channel: String,

    /// CAN frame to send, format: ID#DATA, e.g. 123#11223344 or 1ABCDEFC#11AA22
    frame: String,
}

#[tokio::main]
async fn main() -> io::Result<()> {
    let args = Args::parse();

    // Attempt to open the named pipe (with retries)
    let max_attempts = 5;
    let mut pipe = match connect_pipe_retry(&args.channel, max_attempts).await {
        Some(p) => p,
        None => {
            eprintln!("Failed to parse CAN frame");
            process::exit(1);
        }
    };

    // Attempt to parse the CAN frame from the arg provided
    let frame = match parse_cansend_frame(&args.frame) {
        Some(f) => f,
        None => {
            eprintln!("Failed to parse CAN frame");
            process::exit(1);
        }
    };

    pipe.write_frame(frame).await
}

async fn connect_pipe_retry(channel: &str, max_attempts: i32) -> Option<WindowsCan> {
    println!("Attempting to connect to {} server", channel);

    let mut attempts = 0;
    loop {
        match WindowsCan::open_write_only(channel) {
            Ok(pipe) => {
                println!("Connected to {} server", channel);
                return Some(pipe);
            }
            Err(e) => {
                attempts += 1;
                if attempts >= max_attempts {
                    eprintln!("Failed to open pipe: {:?}", e);
                    return None;
                } else {
                    eprintln!("Pipe not ready (attempt {}), retrying...", attempts);
                    sleep(Duration::from_millis(500)).await;
                }
            }
        };
    }
}

fn parse_cansend_frame(input: &str) -> Option<CanFrame> {
    let parts: Vec<&str> = input.split('#').collect();
    if parts.len() != 2 {
        eprintln!("Invalid frame format, expected ID#DATA");
        return None;
    }

    let id = u32::from_str_radix(parts[0], 16).ok()?;
    let extended = id > 0x7FF;

    let data_str = parts[1];
    if data_str.len() % 2 != 0 {
        eprintln!("Data length must be even hex digits");
        return None;
    }

    let data: Option<Vec<u8>> = (0..data_str.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&data_str[i..i + 2], 16).ok())
        .collect();

    if extended {
        match CanFrame::new_eff(id, data?.as_slice()) {
            Ok(frame) => Some(frame),
            Err(e) => {
                eprintln!("Could create CAN frame: {}", e);
                None
            }
        }
    } else {
        match CanFrame::new(id, data?.as_slice()) {
            Ok(frame) => Some(frame),
            Err(e) => {
                eprintln!("Could create CAN frame: {}", e);
                None
            }
        }
    }
}
