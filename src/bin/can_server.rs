use clap::Parser;
use crosscan::can::CanFrame;
use std::process::exit;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::signal;
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use tokio::time::{Duration, timeout};
use win_can_utils;

#[derive(Parser, Debug)]
struct Cli {
    channel: String,
    #[arg(short = 'b', long = "bitrate")]
    bitrate: Option<u32>,
}

async fn init_can_async(cli: &Cli) -> std::io::Result<win_can_utils::SlcanDriver> {
    let mut can_driver = match win_can_utils::SlcanDriver::open(&cli.channel) {
        Ok(d) => d,
        Err(_) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!(
                    "Could not open serial port {}. Is it an slcan device?",
                    &cli.channel
                ),
            ));
        }
    };

    if can_driver.close_channel().is_err() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!(
                "Could not open serial port {}. Is it an slcan device?",
                &cli.channel
            ),
        ));
    }

    // Get slcan driver version
    let firmware_version = match can_driver.get_version() {
        Ok(s) => s,
        Err(_) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "Failed to get version",
            ));
        }
    };

    println!(
        "SLCan Connected on {}. FW Version: {}",
        &cli.channel, firmware_version
    );

    let bitrate = match cli.bitrate {
        Some(b) => b,
        None => {
            let is_cyder_fw = firmware_version.starts_with("CYDER-CANABLE");
            if is_cyder_fw {
                match can_driver.get_measured_bitrate() {
                    Ok(b) => {
                        println!("Using measured bitrate: {}", b);
                        b
                    }
                    Err(_) => {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::Other,
                            "Error retrieving measured bitrate. Try specifying it manually using the bitrate flag (-b)",
                        ));
                    }
                }
            } else {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    "No bitrate provided. Use the bitrate flag -b or upgrade to Cyder-Canable firmware for auto-bitrate detection.",
                ));
            }
        }
    };

    can_driver.set_can_bitrate(bitrate)?;
    can_driver.enable_timestamp()?;
    can_driver.open_channel()?;
    Ok(can_driver)
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let cli = Cli::parse();

    let driver = match init_can_async(&cli).await {
        Ok(d) => d,
        Err(e) => {
            eprintln!("{}", e.to_string());
            exit(1);
        }
    };
    let driver = Arc::new(Mutex::new(driver));
    let shutdown = Arc::new(AtomicBool::new(false));

    let (tx_out_pipe, rx_out_pipe) = mpsc::channel::<String>(100);
    let (tx_in_pipe, mut rx_in_pipe) = mpsc::channel::<String>(100);

    tokio::spawn(win_can_utils::thread_manager_async::start_ipc_reader(
        cli.channel.clone(),
        tx_in_pipe,
    ));
    tokio::spawn(win_can_utils::thread_manager_async::start_ipc_writer(
        cli.channel.clone(),
        rx_out_pipe,
    ));

    // Ctrl-C handling (async)
    let driver_clone = driver.clone();
    let shutdown_clone = shutdown.clone();
    tokio::spawn(async move {
        signal::ctrl_c().await.expect("Failed to listen for ctrl_c");
        println!("Ctrl+C received, shutting down...");
        shutdown_clone.store(true, Ordering::SeqCst);

        if let Err(e) = driver_clone.lock().await.close_channel() {
            eprintln!("Failed to close CAN driver: {:?}", e);
        } else {
            println!("CAN driver closed.");
        }
        std::process::exit(0);
    });

    // Main loop
    while !shutdown.load(Ordering::SeqCst) {
        // Try to receive a line with timeout
        match timeout(Duration::from_millis(5), rx_in_pipe.recv()).await {
            Ok(Some(line)) => {
                // Got a line, process it
                match serde_json::from_str::<CanFrame>(&line) {
                    Ok(frame) => {
                        let mut d = driver.lock().await;
                        if let Err(e) = d.send_frame(&frame) {
                            eprintln!("Failed to send CAN frame: {:?}", e);
                        } else {
                            println!("Sent CAN frame ID=0x{:X}", frame.id());
                        }
                    }
                    Err(e) => eprintln!("Failed to deserialize CanFrame: {:?}", e),
                }
            }
            Ok(None) => {
                // Sender dropped, no more incoming frames
                break;
            }
            Err(_) => {
                // Timeout elapsed, no message received, continue
            }
        }

        // Now read frames from CAN and send out
        {
            match driver.lock().await.read_frames() {
                Ok(frames) => {
                    for frame in frames {
                        let mut json = serde_json::to_string(&frame).unwrap_or_default();
                        json.push('\n');

                        if let Err(_) = tx_out_pipe.try_send(json) {
                            // If the IPC cannot be written to right now, move on until availble
                            break;
                        }
                    }
                }
                Err(e) => eprintln!("Failed to read frames from CAN driver: {:?}", e),
            }
        }
    }

    Ok(())
}
