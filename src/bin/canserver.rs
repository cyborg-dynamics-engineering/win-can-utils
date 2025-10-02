use bincode;
use clap::Parser;
use crosscan::can::CanFrame;
use serialport::available_ports;
use std::path::Path;
use std::process::exit;
use std::sync::Arc;
use tokio::signal;
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use tokio::time::Duration;
use win_can_utils::{CanDriver, GsUsbDriver, PcanDriver, SlcanDriver, thread_manager_async};

/// Determine the next available IPC channel name by probing for an unused pipe.
///
/// The IPC implementation uses Windows named pipes whose names are derived from the
/// channel.  We iterate a numeric suffix until we find a pipe that does not exist
/// yet and return the corresponding channel identifier.
fn next_auto_channel(base: &str) -> String {
    let mut idx = 0;
    loop {
        let candidate = format!("{}{}", base, idx);
        let path = &format!(r"\\.\pipe\can_{}_in", candidate);

        let pipe_path = Path::new(path);

        if !pipe_path.exists() {
            return candidate;
        }

        idx += 1;
    }
}

#[derive(Parser, Debug)]
struct Cli {
    /// Supported drivers: gsusb, pcan, slcan
    driver: String,
    /// Channel: use auto for auto-detect
    #[arg(short = 'c', long = "channel", default_value = "auto")]
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
    let mut slcan_driver = if cli.channel.to_ascii_lowercase() == "auto" {
        // Enumerate all serial ports on the system
        let ports = available_ports().map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("Failed to list serial ports: {}", e),
            )
        })?;

        if ports.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "No serial ports found on this system.",
            ));
        }

        let mut found: Option<SlcanDriver> = None;
        for p in ports {
            let port_name = p.port_name;
            println!("Trying SLCAN auto-detect on {}", port_name);

            if let Ok(mut driver) = SlcanDriver::open(&port_name).await {
                if driver.get_version().await.is_err() {
                    continue;
                }
                println!("Auto-detected SLCAN device on {}", port_name);
                found = Some(driver);
                break;
            }
        }

        match found {
            Some(d) => d,
            None => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    "Could not auto-detect an SLCAN device (checked all available COM/tty ports).",
                ));
            }
        }
    } else {
        // User provided a channel manually
        match SlcanDriver::open(&cli.channel).await {
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

    println!("SLCan Connected. FW Version: {}", firmware_version);

    slcan_driver.set_bitrate(bitrate).await?;
    slcan_driver.enable_timestamp().await?;
    slcan_driver.open_channel().await?;

    Ok(Box::new(slcan_driver))
}

async fn init_gsusb(cli: &Cli) -> std::io::Result<Box<dyn CanDriver>> {
    let mut driver = GsUsbDriver::open(&cli.channel).await?;

    driver.close_channel().await?;

    let bitrate = cli.bitrate.ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::Other,
            "No bitrate provided. Specify one with -b <bitrate> for gs_usb devices.",
        )
    })?;

    println!("gs_usb connected to {}", driver.device_label());

    driver.set_bitrate(bitrate).await?;
    driver.enable_timestamp().await?;
    driver.open_channel().await?;

    Ok(Box::new(driver))
}

/// Resolve the requested driver implementation from the CLI arguments.
async fn initialize_driver(cli: &Cli) -> std::io::Result<Box<dyn CanDriver>> {
    match cli.driver.to_lowercase().as_str() {
        "slcan" => init_slcan(cli).await,
        "pcan" => init_pcan(cli).await,
        "gsusb" | "gs_usb" => init_gsusb(cli).await,
        other => Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!(
                "Did not recognize driver specified: {}\nSupported drivers are: slcan, pcan, gsusb",
                other
            ),
        )),
    }
}

/// Spawn background tasks responsible for reading and writing the IPC pipes.
///
/// The caller receives the sender towards the writer and the receiver from the
/// reader so it can bridge the IPC traffic with the CAN driver.
fn spawn_ipc_tasks(channel_name: String) -> (mpsc::Sender<Vec<u8>>, mpsc::Receiver<Vec<u8>>) {
    let (tx_out_pipe, rx_out_pipe) = mpsc::channel::<Vec<u8>>(100);
    let (tx_in_pipe, rx_in_pipe) = mpsc::channel::<Vec<u8>>(100);

    tokio::spawn(thread_manager_async::start_ipc_reader(
        channel_name.clone(),
        tx_in_pipe,
    ));

    tokio::spawn(thread_manager_async::start_ipc_writer(
        channel_name,
        rx_out_pipe,
    ));

    (tx_out_pipe, rx_in_pipe)
}

/// Consume CAN frames received from the IPC pipe and forward them to the driver.
async fn forward_pipe_to_can(
    mut rx_in_pipe: mpsc::Receiver<Vec<u8>>,
    driver: Arc<Mutex<Box<dyn CanDriver>>>,
) {
    while let Some(line) = rx_in_pipe.recv().await {
        if let Ok((frame, _)) =
            bincode::serde::decode_from_slice::<CanFrame, _>(&line, bincode::config::standard())
        {
            let mut d = driver.lock().await;
            if let Err(e) = d.send_frame(&frame).await {
                eprintln!("Failed to send CAN frame: {:?}", e);
            }
        }
    }
}

/// Continuously poll the CAN driver and push any frames to the IPC writer.
async fn forward_can_to_pipe(
    driver: Arc<Mutex<Box<dyn CanDriver>>>,
    tx_out_pipe: mpsc::Sender<Vec<u8>>,
) {
    loop {
        match driver.lock().await.read_frames().await {
            Ok(frames) => {
                for frame in frames {
                    if let Ok(mut data) =
                        bincode::serde::encode_to_vec(frame, bincode::config::standard())
                    {
                        if data.len() > (u8::MAX as usize) {
                            eprintln!("Serialized CanFrame too large: {}", data.len());
                            continue;
                        }
                        let mut msg = vec![data.len() as u8];
                        msg.append(&mut data);
                        let _ = tx_out_pipe.try_send(msg);
                    }
                }
            }
            Err(e) => {
                eprintln!("Failed to read frames from CAN driver: {:?}", e);
                break;
            }
        }
    }
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    env_logger::init();
    let cli = Cli::parse();

    let channel_name = if cli.channel.to_ascii_lowercase() == "auto" {
        next_auto_channel("can")
    } else {
        cli.channel.clone()
    };

    // Initialize the requested CAN driver implementation.
    let driver = match initialize_driver(&cli).await {
        Ok(driver) => Arc::new(Mutex::new(driver)),
        Err(e) => {
            eprintln!("{}", e.to_string());
            exit(1);
        }
    };

    let (tx_out_pipe, rx_in_pipe) = spawn_ipc_tasks(channel_name.clone());

    println!("\nCreated CAN server: {}", channel_name);

    // Task to bridge IPC traffic into the CAN driver.
    let mut task_in = tokio::spawn(forward_pipe_to_can(rx_in_pipe, driver.clone()));

    // Task to bridge CAN traffic out to the IPC pipe.
    let mut task_out = tokio::spawn(forward_can_to_pipe(driver.clone(), tx_out_pipe));

    // Wait for ctrl+c OR a task finishing
    tokio::select! {
        _ = signal::ctrl_c() => {
            println!("Ctrl+C received, shutting down...");
        }
        res = &mut task_in => {
            if let Err(e) = res { eprintln!("Incoming task panicked: {:?}", e); }
            println!("Incoming task ended.");
        }
        res = &mut task_out => {
            if let Err(e) = res { eprintln!("Outgoing task panicked: {:?}", e); }
            println!("Outgoing task ended.");
        }
    }

    // stop worker tasks first so they release the mutex
    task_in.abort();
    task_out.abort();

    // (optional) give them a moment to unwind
    let _ = tokio::time::timeout(Duration::from_millis(200), async {
        let _ = task_in.await;
        let _ = task_out.await;
    })
    .await;

    // now it's safe to close the driver (no one holds the lock)
    if let Err(e) = driver.lock().await.close_channel().await {
        eprintln!("Failed to close CAN driver: {:?}", e);
    } else {
        println!("CAN driver closed.");
    }

    Ok(())
}
