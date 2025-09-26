use bincode;
use clap::Parser;
use crosscan::can::CanFrame;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio;
use tokio::sync::mpsc;
use win_can_utils::thread_manager_async;

#[derive(Parser, Debug)]
struct Cli {
    #[arg(short = 'd', long = "driver", default_value = "slcan")]
    driver: String,
    channel: String,
    #[arg(short = 'b', long = "bitrate")]
    bitrate: Option<u32>,
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let cli = Cli::parse();

    let shutdown = Arc::new(AtomicBool::new(false));

    let (tx_out_pipe, rx_out_pipe) = mpsc::channel::<Vec<u8>>(100);
    let (tx_in_pipe, _rx_in_pipe) = mpsc::channel::<Vec<u8>>(100);

    tokio::spawn(thread_manager_async::start_ipc_reader(
        cli.channel.clone(),
        tx_in_pipe,
    ));
    tokio::spawn(thread_manager_async::start_ipc_writer(
        cli.channel.clone(),
        rx_out_pipe,
    ));

    // Ctrl-C handling (async)
    let shutdown_clone = shutdown.clone();
    tokio::spawn(async move {
        tokio::signal::ctrl_c()
            .await
            .expect("Failed to listen for ctrl_c");
        println!("Ctrl+C received, shutting down...");
        shutdown_clone.store(true, Ordering::SeqCst);

        std::process::exit(0);
    });

    // Main loop
    while !shutdown.load(Ordering::SeqCst) {
        // Generate test CanFrame data
        const CAN_ID: u32 = 55;
        const DATA_FREQ_HZ: f32 = 10.0;
        let frame = CanFrame::new(CAN_ID, &[1, 2, 3, 5]).unwrap();

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
                    // If the IPC cannot be written to right now, wait for a short period and try again
                    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                    continue;
                }

                let data_period_ms = (1000.0 / DATA_FREQ_HZ).round() as u64;
                tokio::time::sleep(tokio::time::Duration::from_millis(data_period_ms)).await;
            }
            Err(e) => eprintln!("Failed to serialize CanFrame: {:?}", e),
        }
    }

    Ok(())
}
