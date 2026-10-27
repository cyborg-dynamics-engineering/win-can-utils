//! Experimental CAN-over-UDP endpoint behind a real Wintun NDIS adapter.
use clap::{Parser, Subcommand};
use crosscan::{
    protocol::{Command, Subscription, validate_name},
    service::ServiceClient,
};
use std::{io, net::Ipv4Addr, path::PathBuf, time::Duration};
use win_can_utils::{
    instance_lock::InstanceLock,
    network_adapter::{
        NetworkConfig,
        bridge::{self, BridgeOptions},
        wintun::{self, Api},
    },
};

#[derive(Parser)]
#[command(about = "Experimental real Windows CAN network adapter using Wintun")]
struct Args {
    #[command(subcommand)]
    command: Action,
}

#[derive(Subcommand)]
enum Action {
    /// Read-only DLL/API, driver version and elevation diagnostics.
    Probe {
        #[arg(long)]
        dll: PathBuf,
    },
    /// Bridge a dedicated Wintun adapter to a running CAN broker.
    Run {
        #[arg(long)]
        dll: PathBuf,
        #[arg(long, default_value = "can0")]
        interface: String,
        #[arg(long)]
        adapter: Option<String>,
        /// Create a dedicated temporary adapter. Requires administrator elevation.
        #[arg(long)]
        create: bool,
        /// Add the local IPv4 address and set MTU 65535 on the newly created adapter.
        #[arg(long, requires = "create")]
        configure_ip: bool,
        #[arg(long, default_value = "198.18.0.1")]
        local: Ipv4Addr,
        #[arg(long, default_value = "198.18.0.2")]
        peer: Ipv4Addr,
        #[arg(long, default_value_t = 30)]
        prefix_length: u8,
        #[arg(long, default_value_t = 29536)]
        port: u16,
        #[arg(long, default_value_t = 60)]
        idle_seconds: u64,
        #[arg(long, default_value_t = 32)]
        max_clients: usize,
        #[arg(long, default_value_t = 4_194_304)]
        ring_capacity: u32,
        #[arg(long)]
        stats_file: Option<PathBuf>,
    },
}

#[tokio::main]
async fn main() -> io::Result<()> {
    env_logger::init();
    match Args::parse().command {
        Action::Probe { dll } => println!(
            "{}",
            serde_json::to_string_pretty(&Api::load(&dll)?.probe()?)?
        ),
        Action::Run {
            dll,
            interface,
            adapter,
            create,
            configure_ip,
            local,
            peer,
            prefix_length,
            port,
            idle_seconds,
            max_clients,
            ring_capacity,
            stats_file,
        } => {
            validate_name(&interface)?;
            let network = NetworkConfig {
                local,
                peer,
                prefix_length,
                port,
            };
            network.validate()?;
            if max_clients == 0 || max_clients > 128 || !(1..=86_400).contains(&idle_seconds) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Use 1..128 clients and an idle timeout of 1..86400 seconds",
                ));
            }
            if create || configure_ip {
                wintun::require_elevated()?;
            }
            let name = adapter.unwrap_or_else(|| format!("WinCAN {interface}"));
            let _reservation = InstanceLock::acquire("netadapter", &name)?;
            // Verify broker access before changing the network configuration.
            let check = ServiceClient::connect(
                &interface,
                Subscription {
                    receive: false,
                    ..Default::default()
                },
            )
            .await?;
            check.request(Command::Status).await?;
            drop(check);
            let api = Api::load(&dll)?;
            if configure_ip {
                wintun::ensure_subnet_unused(&network)?;
            }
            let adapter = api.adapter(&name, create)?;
            let session = adapter.start(ring_capacity)?;
            if configure_ip {
                adapter.configure_ipv4(&network)?;
            }
            println!(
                "Adapter: {name}, index {}, CAN interface: {interface}",
                adapter.interface_index()?
            );
            println!(
                "UDP endpoint: {peer}:{port}; bind clients to {local}. Ctrl+C closes the bridge."
            );
            let report = bridge::run(
                session,
                BridgeOptions {
                    interface,
                    network,
                    idle_timeout: Duration::from_secs(idle_seconds),
                    max_clients,
                },
                async {
                    let _ = tokio::signal::ctrl_c().await;
                },
            )
            .await?;
            let json = serde_json::to_string_pretty(&report)?;
            if let Some(path) = stats_file {
                std::fs::write(path, &json)?;
            }
            println!("{json}");
        }
    }
    Ok(())
}
