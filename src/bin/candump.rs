use clap::Parser;
use crosscan::win_can::CanSocket;
use tokio::time::{Duration, sleep};

#[derive(Parser, Debug)]
struct Args {
    #[arg()]
    channel: String,
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let args = Args::parse();
    let mut pipe = connect_pipe_retry(&args.channel).await;

    loop {
        let frame = pipe.read_frame().await?;
        println!(
            "{:?} ID=0x{:X} Extended={} RTR={} Error={} [{}]",
            frame.timestamp().unwrap_or(0),
            frame.id(),
            frame.is_extended(),
            frame.is_rtr(),
            frame.is_error(),
            frame
                .data()
                .iter()
                .map(|b| format!("{:02X}", b))
                .collect::<Vec<_>>()
                .join(" "),
        );
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
