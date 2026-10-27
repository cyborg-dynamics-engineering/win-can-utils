use bincode;
use clap::Parser;
use crosscan::can::CanFrame;
use serialport::available_ports;
use std::io::IsTerminal;
use std::path::Path;
use std::process::exit;
use std::sync::Arc;
use tokio::signal;
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use tokio::time::Duration;
use win_can_utils::instance_lock::InstanceLock;
use win_can_utils::{CanDriver, GsUsbDriver, PcanDriver, SlcanDriver, thread_manager_async};

#[path = "canserver/adapters.rs"]
mod adapters;

/// Reserve an IPC name for this process. A kernel reservation prevents concurrent
/// starts from choosing the same name; pipe checks also detect older servers.
fn reserve_channel(requested: Option<&str>, base: &str) -> std::io::Result<(String, InstanceLock)> {
    if let Some(name) = requested.filter(|name| !name.eq_ignore_ascii_case("auto")) {
        let reservation = InstanceLock::acquire("pipe", name)?;
        if Path::new(&format!(r"\\.\pipe\can_{name}_in")).exists() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AddrInUse,
                format!("CAN pipe {name} already exists"),
            ));
        }
        return Ok((name.into(), reservation));
    }
    let mut idx = 0;
    loop {
        let candidate = format!("{}{}", base, idx);
        match InstanceLock::acquire("pipe", &candidate) {
            Ok(reservation) => {
                if !Path::new(&format!(r"\\.\pipe\can_{candidate}_in")).exists() {
                    return Ok((candidate, reservation));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {}
            Err(error) => return Err(error),
        }
        idx += 1;
    }
}

#[derive(Parser, Debug)]
struct Cli {
    /// Supported drivers: gsusb, pcan, slcan
    driver: String,
    /// Driver channel or legacy gs_usb adapter selector; auto detects/selects
    #[arg(short = 'c', long = "channel", default_value = "auto")]
    channel: String,
    /// gs_usb adapter ID (bus:address/interface), serial number, or product name
    #[arg(long, conflicts_with = "channel")]
    device: Option<String>,
    /// IPC name, e.g. can0; defaults to the next free canN with --device or auto
    #[arg(long)]
    name: Option<String>,
    /// List connected gs_usb adapters, including unavailable ones, and exit
    #[arg(long, alias = "list")]
    list_adapters: bool,
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
    let bitrate = cli.bitrate.ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::Other,
            "No bitrate provided. Specify one with -b <bitrate> for gs_usb devices.",
        )
    })?;

    let requested = cli.device.as_deref().unwrap_or(&cli.channel);
    let identifier = if requested.eq_ignore_ascii_case("auto") {
        tokio::task::spawn_blocking(|| {
            let connected = GsUsbDriver::list_adapters()?;
            adapters::select_adapter(
                &connected,
                std::io::stdin().is_terminal(),
                &mut std::io::stdin().lock(),
                &mut std::io::stdout().lock(),
            )
        })
        .await
        .map_err(std::io::Error::other)??
    } else {
        requested.to_owned()
    };
    // Availability can change after the menu; open the chosen ID again and
    // report a race instead of silently substituting a different adapter.
    let mut driver = GsUsbDriver::open(&identifier).await?;
    driver.close_channel().await?;

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
fn spawn_ipc_tasks(
    channel_name: String,
    driver: Arc<Mutex<Box<dyn CanDriver>>>,
) -> std::io::Result<(mpsc::Sender<Vec<u8>>, mpsc::Receiver<CanFrame>)> {
    let (tx_out_pipe, rx_out_pipe) = mpsc::channel::<Vec<u8>>(100);
    let (tx_in_pipe, rx_in_pipe) = mpsc::channel::<CanFrame>(100);

    tokio::spawn(thread_manager_async::start_ipc_reader(
        channel_name.clone(),
        tx_in_pipe,
    ));

    tokio::spawn(thread_manager_async::start_ipc_writer(
        channel_name.clone(),
        rx_out_pipe,
    ));

    tokio::spawn(async move {
        let config = thread_manager_async::CanServerConfig {
            bitrate: driver.lock().await.get_bitrate().await,
            version: env!("CARGO_PKG_VERSION").to_string(),
        };
        if let Err(e) = thread_manager_async::start_ipc_config_handler(channel_name, config).await {
            eprintln!("Encounted error when sending Config {:?}", e);
        }
    });

    Ok((tx_out_pipe, rx_in_pipe))
}

/// Consume CAN frames received from the IPC pipe and forward them to the driver.
async fn forward_pipe_to_can(
    mut rx_in_pipe: mpsc::Receiver<CanFrame>,
    driver: Arc<Mutex<Box<dyn CanDriver>>>,
) {
    while let Some(frame) = rx_in_pipe.recv().await {
        let mut d = driver.lock().await;
        if let Err(e) = d.send_frame(&frame).await {
            eprintln!("Failed to send CAN frame: {:?}", e);
        }
    }
}

/// Forward CAN frames, waiting outside the driver lock when RX can signal readiness.
async fn forward_can_to_pipe(
    driver: Arc<Mutex<Box<dyn CanDriver>>>,
    tx_out_pipe: mpsc::Sender<Vec<u8>>,
) {
    let receive_notifier = driver.lock().await.receive_notifier();
    loop {
        let result = {
            let mut driver = driver.lock().await;
            driver.read_frames().await
        };
        match result {
            Ok(frames) => {
                if frames.is_empty() {
                    if let Some(notifier) = &receive_notifier {
                        notifier.notified().await;
                    } else {
                        tokio::task::yield_now().await;
                    }
                    continue;
                }
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
                        // Preserve bursts when the bounded pipe queue is full.
                        // The driver lock has already been released, so TX is
                        // still available while the pipe applies backpressure.
                        if tx_out_pipe.send(msg).await.is_err() {
                            return;
                        }
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

    let is_gsusb = matches!(cli.driver.to_ascii_lowercase().as_str(), "gsusb" | "gs_usb");
    if !is_gsusb && (cli.device.is_some() || cli.list_adapters) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "--device and --list-adapters currently apply to gsusb.",
        ));
    }
    if cli.list_adapters {
        return adapters::print_adapters(
            &GsUsbDriver::list_adapters()?,
            &mut std::io::stdout().lock(),
        );
    }
    let legacy_name = (cli.device.is_none() && !cli.channel.eq_ignore_ascii_case("auto"))
        .then_some(cli.channel.as_str());
    let (channel_name, _channel_reservation) =
        reserve_channel(cli.name.as_deref().or(legacy_name), "can")?;

    // Initialize the requested CAN driver implementation.
    let driver = match initialize_driver(&cli).await {
        Ok(driver) => Arc::new(Mutex::new(driver)),
        Err(e) => {
            eprintln!("{}", e.to_string());
            exit(1);
        }
    };

    let (tx_out_pipe, rx_in_pipe) = spawn_ipc_tasks(channel_name.clone(), driver.clone())?;

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

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::collections::VecDeque;
    use std::io;
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Notify;
    use tokio::time::{sleep, timeout};

    #[derive(Default)]
    struct TestState {
        frames: StdMutex<VecDeque<CanFrame>>,
        sent: StdMutex<Vec<CanFrame>>,
        reads: AtomicUsize,
        ready: Arc<Notify>,
        read_started: Notify,
    }

    struct TestDriver {
        state: Arc<TestState>,
        // Enqueue during an empty read to exercise the read/wait race.
        frame_during_read: Option<CanFrame>,
    }

    #[async_trait]
    impl CanDriver for TestDriver {
        async fn enable_timestamp(&mut self) -> io::Result<()> {
            Ok(())
        }
        async fn set_bitrate(&mut self, _: u32) -> io::Result<()> {
            Ok(())
        }
        async fn get_bitrate(&self) -> Option<u32> {
            Some(1_000_000)
        }
        async fn open_channel(&mut self) -> io::Result<()> {
            Ok(())
        }
        async fn close_channel(&mut self) -> io::Result<()> {
            Ok(())
        }

        async fn send_frame(&mut self, frame: &CanFrame) -> io::Result<()> {
            self.state.sent.lock().unwrap().push(frame.clone());
            Ok(())
        }

        async fn read_frames(&mut self) -> io::Result<Vec<CanFrame>> {
            self.state.reads.fetch_add(1, Ordering::SeqCst);
            self.state.read_started.notify_one();
            let mut queue = self.state.frames.lock().unwrap();
            let count = queue.len().min(1024);
            let frames: Vec<_> = queue.drain(..count).collect();
            match (frames.is_empty(), self.frame_during_read.take()) {
                (true, Some(frame)) => {
                    queue.push_back(frame);
                    self.state.ready.notify_one();
                }
                (_, frame) => self.frame_during_read = frame,
            }
            Ok(frames)
        }

        fn receive_notifier(&self) -> Option<Arc<Notify>> {
            Some(self.state.ready.clone())
        }
    }

    fn test_driver(
        state: &Arc<TestState>,
        frame_during_read: Option<CanFrame>,
    ) -> Arc<Mutex<Box<dyn CanDriver>>> {
        Arc::new(Mutex::new(Box::new(TestDriver {
            state: state.clone(),
            frame_during_read,
        })))
    }

    async fn next_frame(rx: &mut mpsc::Receiver<Vec<u8>>) -> CanFrame {
        let msg = timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("frame forwarding stalled")
            .expect("forwarder closed");
        assert_eq!(usize::from(msg[0]), msg.len() - 1);
        let (frame, used) = bincode::serde::decode_from_slice::<CanFrame, _>(
            &msg[1..],
            bincode::config::standard(),
        )
        .unwrap();
        assert_eq!(used, msg.len() - 1);
        frame
    }

    #[tokio::test]
    async fn idle_rx_waits_without_polling_or_blocking_tx() {
        let state = Arc::new(TestState::default());
        let driver = test_driver(&state, None);
        let (tx, mut rx) = mpsc::channel(1);
        let forwarder = tokio::spawn(forward_can_to_pipe(driver.clone(), tx));
        timeout(Duration::from_secs(2), state.read_started.notified())
            .await
            .unwrap();
        sleep(Duration::from_millis(30)).await;
        assert_eq!(state.reads.load(Ordering::SeqCst), 1, "idle RX is polling");

        let frame = CanFrame::new(0x123, &[1, 2, 3]).unwrap();
        timeout(Duration::from_secs(2), async {
            driver.lock().await.send_frame(&frame).await.unwrap();
        })
        .await
        .expect("idle RX held the driver lock");
        assert_eq!(*state.sent.lock().unwrap(), vec![frame.clone()]);

        state.frames.lock().unwrap().push_back(frame.clone());
        state.ready.notify_one();
        assert_eq!(next_frame(&mut rx).await, frame);
        forwarder.abort();
        let _ = forwarder.await;
    }

    #[tokio::test]
    async fn notification_between_empty_read_and_wait_is_retained() {
        let state = Arc::new(TestState::default());
        let frame = CanFrame::new_remote(0x321, 8, false).unwrap();
        let driver = test_driver(&state, Some(frame.clone()));
        let (tx, mut rx) = mpsc::channel(1);
        let forwarder = tokio::spawn(forward_can_to_pipe(driver, tx));
        assert_eq!(next_frame(&mut rx).await, frame);
        forwarder.abort();
        let _ = forwarder.await;
    }

    #[tokio::test]
    async fn full_pipe_queue_preserves_burst_order_and_allows_tx() {
        let state = Arc::new(TestState::default());
        let expected: Vec<_> = (0..4096u32)
            .map(|seq| {
                let mut frame = CanFrame::new_eff(0x12345, &seq.to_le_bytes()).unwrap();
                frame.set_timestamp(Some((1u64 << 32) - 2048 + u64::from(seq)));
                frame
            })
            .collect();
        state
            .frames
            .lock()
            .unwrap()
            .extend(expected.iter().cloned());
        let driver = test_driver(&state, None);
        // One queued frame forces backpressure on the very next frame.
        let (tx, mut rx) = mpsc::channel(1);
        let forwarder = tokio::spawn(forward_can_to_pipe(driver.clone(), tx));
        timeout(Duration::from_secs(2), state.read_started.notified())
            .await
            .unwrap();
        timeout(Duration::from_secs(2), async {
            driver.lock().await.send_frame(&expected[0]).await.unwrap();
        })
        .await
        .expect("pipe backpressure held the driver lock");
        for frame in expected {
            assert_eq!(next_frame(&mut rx).await, frame);
        }
        assert!(rx.try_recv().is_err(), "duplicate frames");
        forwarder.abort();
        let _ = forwarder.await;
    }
}
