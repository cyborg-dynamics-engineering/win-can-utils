use bincode;
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
use win_can_utils::{CanDriver, PcanDriver, SlcanDriver, thread_manager_async};

#[derive(Parser, Debug)]
struct Cli {
    #[arg(short = 'd', long = "driver", default_value = "slcan")]
    driver: String,
    channel: String,
    #[arg(short = 'b', long = "bitrate")]
    bitrate: Option<u32>,
}

/// Initialize PCAN driver from CLI args.
async fn init_pcan(cli: &Cli) -> std::io::Result<Box<dyn CanDriver>> {
    // Try to open the PCAN channel (e.g., "USBBUS1")
    let mut pcan_driver = if cli.channel.to_ascii_uppercase() == "AUTO" {
        // Try common PCAN channels in order
        let common_channels = [
            "USBBUS1", "USBBUS2", "USBBUS3", "USBBUS4", "PCIBUS1", "PCIBUS2", "LANBUS1", "LANBUS2",
        ];

        let mut found: Option<PcanDriver> = None;
        for ch in &common_channels {
            if let Ok(driver) = PcanDriver::open(ch).await {
                // Try to actually initialize the hardware (set bitrate later)
                found = Some(driver);
                println!("Auto-detected PCAN channel: {}", ch);
                break;
            }
        }

        match found {
            Some(d) => d,
            None => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    "Could not auto-detect a PCAN channel (tried USB, PCI, LAN).",
                ));
            }
        }
    } else {
        // User gave a channel explicitly
        match PcanDriver::open(&cli.channel).await {
            Ok(d) => d,
            Err(_) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!(
                        "Could not open PCAN channel {}. Is the device connected?",
                        &cli.channel
                    ),
                ));
            }
        }
    };

    // Close channel if left open (same pattern as SLCAN)
    let _ = pcan_driver.close_channel().await;

    // Pick bitrate from CLI or hardware
    let bitrate = match cli.bitrate {
        Some(b) => b,
        None => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "No bitrate provided and failed to detect automatically. Use -b <bitrate>.",
            ));
        }
    };

    println!("PCAN Connected on {}", &cli.channel);

    pcan_driver.set_bitrate(bitrate).await?;
    pcan_driver.enable_timestamp().await?;
    pcan_driver.open_channel().await?;

    Ok(Box::new(pcan_driver))
}

async fn init_slcan(cli: &Cli) -> std::io::Result<Box<dyn CanDriver>> {
    let mut slcan_driver = match SlcanDriver::open(&cli.channel).await {
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

    slcan_driver.close_channel().await?;

    // Get slcan driver version
    let firmware_version = match slcan_driver.get_version().await {
        Ok(s) => s,
        Err(_) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "Failed to get version",
            ));
        }
    };

    let bitrate = match cli.bitrate {
        Some(b) => b,
        None => {
            let is_cyder_fw = firmware_version.starts_with("CYDER-CANABLE");
            if is_cyder_fw {
                match slcan_driver.get_measured_bitrate().await {
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

    println!(
        "SLCan Connected on {}. FW Version: {}",
        &cli.channel, firmware_version
    );

    slcan_driver.set_bitrate(bitrate).await?;
    slcan_driver.enable_timestamp().await?;
    slcan_driver.open_channel().await?;

    Ok(Box::new(slcan_driver))
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let cli = Cli::parse();

    // Initialize the specified driver.
    let driver = match cli.driver.to_lowercase().as_str() {
        "slcan" => init_slcan(&cli).await,
        "pcan" => init_pcan(&cli).await,
        _ => {
            eprintln!(
                "Did not recognize driver specified: {}\nSupported drivers are: slcan",
                cli.driver
            );
            exit(1);
        }
    };

    // Check driver start errors.
    let driver = match driver {
        Ok(driver) => Arc::new(Mutex::new(driver)),
        Err(e) => {
            eprintln!("{}", e.to_string());
            exit(1);
        }
    };

    let shutdown = Arc::new(AtomicBool::new(false));

    let (tx_out_pipe, rx_out_pipe) = mpsc::channel::<Vec<u8>>(100);
    let (tx_in_pipe, mut rx_in_pipe) = mpsc::channel::<Vec<u8>>(100);

    tokio::spawn(thread_manager_async::start_ipc_reader(
        cli.channel.clone(),
        tx_in_pipe,
    ));
    tokio::spawn(thread_manager_async::start_ipc_writer(
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

        if let Err(e) = driver_clone.lock().await.close_channel().await {
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
                match bincode::serde::decode_from_slice::<CanFrame, _>(
                    &line,
                    bincode::config::standard(),
                ) {
                    Ok((frame, _)) => {
                        let mut d = driver.lock().await;
                        if let Err(e) = d.send_frame(&frame).await {
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
            match driver.lock().await.read_frames().await {
                Ok(frames) => {
                    for frame in frames {
                        match bincode::serde::encode_to_vec(frame, bincode::config::standard()) {
                            Ok(mut data) => {
                                // Check that message length fits within a single byte
                                if data.len() > (u8::MAX as usize) {
                                    eprintln!(
                                        "Serialized CanFrame is too large to send, size: {:?}",
                                        data.len()
                                    );
                                }

                                // Begin message with payload length byte
                                let mut msg = vec![data.len() as u8];

                                // Add CanFrame payload
                                msg.append(&mut data);

                                if let Err(_) = tx_out_pipe.try_send(msg) {
                                    // If the IPC cannot be written to right now, break and move on until availble
                                    break;
                                }
                            }
                            Err(e) => eprintln!("Failed to serialize CanFrame: {:?}", e),
                        }
                    }
                }
                Err(e) => eprintln!("Failed to read frames from CAN driver: {:?}", e),
            }
        }
    }

    Ok(())
}
