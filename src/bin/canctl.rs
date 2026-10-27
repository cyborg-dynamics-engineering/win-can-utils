use clap::{Parser, Subcommand};
use crosscan::{
    protocol::{Command, Reply, Subscription},
    service::ServiceClient,
};
use std::{io, path::PathBuf};
use win_can_utils::{
    config::{InterfaceConfig, default_path},
    instance_lock::InstanceLock,
};

#[derive(Parser)]
#[command(about = "Configure and inspect a named Windows CAN interface")]
struct Args {
    interface: String,
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Action,
}

#[derive(Subcommand)]
enum Action {
    /// Create a saved configuration without opening hardware. Start it with canserver --name NAME.
    Create {
        #[arg(long)]
        driver: String,
        #[arg(long)]
        device: String,
        #[arg(long)]
        bitrate: u32,
        #[arg(long)]
        up: bool,
        #[arg(long)]
        no_timestamps: bool,
    },
    Show,
    Clients,
    Up,
    Down,
    /// Change saved settings while the interface is down.
    Set {
        #[arg(long)]
        bitrate: Option<u32>,
        #[arg(long, action = clap::ArgAction::Set)]
        timestamps: Option<bool>,
    },
}

#[tokio::main]
async fn main() -> io::Result<()> {
    let args = Args::parse();
    crosscan::protocol::validate_name(&args.interface)?;
    let path = args
        .config
        .map(Ok)
        .unwrap_or_else(|| default_path(&args.interface))?;
    if let Action::Create {
        driver,
        device,
        bitrate,
        up,
        no_timestamps,
    } = args.command
    {
        let _reservation = InstanceLock::acquire("pipe", &args.interface)?;
        if path.exists() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "Configuration already exists; use canctl set while the server is running",
            ));
        }
        let config = InterfaceConfig {
            version: 1,
            name: args.interface,
            driver,
            device,
            bitrate,
            up,
            timestamps: !no_timestamps,
        };
        config.save(&path)?;
        println!("{}", serde_json::to_string_pretty(&config)?);
        return Ok(());
    }
    let client = match ServiceClient::connect(
        &args.interface,
        Subscription {
            receive: false,
            ..Default::default()
        },
    )
    .await
    {
        Ok(client) => client,
        Err(error) if matches!(args.command, Action::Show) && error.raw_os_error() == Some(2) => {
            let config = InterfaceConfig::load(&path)?;
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &serde_json::json!({ "server_running": false, "configuration": config })
                )?
            );
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    let command = match args.command {
        Action::Show => Command::Status,
        Action::Clients => Command::Clients,
        Action::Up => Command::SetUp { up: true },
        Action::Down => Command::SetUp { up: false },
        Action::Set {
            bitrate,
            timestamps,
        } => Command::Configure {
            bitrate,
            timestamps,
        },
        Action::Create { .. } => unreachable!(),
    };
    let reply = client.request(command).await?;
    match reply {
        Reply::Status { status } => println!("{}", serde_json::to_string_pretty(&status)?),
        Reply::Clients { clients } => println!("{}", serde_json::to_string_pretty(&clients)?),
        _ => println!("{}", serde_json::to_string_pretty(&reply)?),
    }
    Ok(())
}
