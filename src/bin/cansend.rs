use clap::Parser;
use crosscan::{
    can::CanFrame,
    protocol::{Command, Frame, Subscription},
    service::ServiceClient,
};
use std::io;
use std::process;

#[derive(Parser)]
struct Args {
    /// Named CAN interface, e.g. can0
    channel: String,

    /// CAN frame to send, format: ID#DATA, e.g. 123#11223344 or 1ABCDEFC#11AA22
    frame: String,
    /// Receive and print this connection's software transmit echo
    #[arg(long)]
    receive_own_messages: bool,
}

#[tokio::main]
async fn main() -> io::Result<()> {
    let args = Args::parse();

    // Attempt to parse the CAN frame from the arg provided
    let frame = match parse_cansend_frame(&args.frame) {
        Some(f) => f,
        None => {
            eprintln!("Failed to parse CAN frame");
            process::exit(1);
        }
    };

    let mut client = ServiceClient::connect(
        &args.channel,
        Subscription {
            receive: args.receive_own_messages,
            receive_own_messages: args.receive_own_messages,
            ..Default::default()
        },
    )
    .await?;
    client
        .request(Command::Send {
            frame: Frame::from(&frame),
        })
        .await?;
    if args.receive_own_messages {
        let echo = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                let event = client.next_event().await?;
                if event.source_client_id == Some(client.client_id) {
                    return Ok::<_, io::Error>(event);
                }
            }
        })
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "Transmit echo was not received"))??;
        println!("{}", serde_json::to_string(&echo)?);
    }
    Ok(())
}

fn parse_cansend_frame(input: &str) -> Option<CanFrame> {
    let parts: Vec<&str> = input.split('#').collect();
    if parts.len() != 2 {
        eprintln!("Invalid frame format, expected ID#DATA");
        return None;
    }

    let id = u32::from_str_radix(parts[0], 16).ok()?;
    let extended = parts[0].len() == 8 || id > 0x7FF;

    let data_str = parts[1];
    if let Some(dlc) = data_str
        .strip_prefix('R')
        .or_else(|| data_str.strip_prefix('r'))
    {
        let dlc = if dlc.is_empty() {
            0
        } else {
            dlc.parse::<usize>().ok()?
        };
        return CanFrame::new_remote(id, dlc, extended).ok();
    }
    if data_str.len() > 16 || !data_str.is_ascii() {
        return None;
    }
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
