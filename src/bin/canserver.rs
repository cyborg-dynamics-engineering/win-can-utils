use clap::Parser;
use std::{
    io::{self, IsTerminal},
    path::{Path, PathBuf},
    sync::Arc,
};
use win_can_utils::{
    GsUsbDriver,
    config::{InterfaceConfig, default_path},
    hardware::HardwareFactory,
    instance_lock::InstanceLock,
};

#[path = "canserver/adapters.rs"]
mod adapters;

/// Reserve an IPC name for this process. A kernel reservation prevents concurrent
/// starts from choosing the same name; pipe checks also detect older servers.
fn reserve_channel(requested: Option<&str>, base: &str) -> std::io::Result<(String, InstanceLock)> {
    if let Some(name) = requested {
        crosscan::protocol::validate_name(name)?;
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
#[command(about = "Run a persistent, multi-client CAN interface broker")]
struct Cli {
    /// gsusb, pcan, slcan or virtual. Omit to load a saved configuration.
    driver: Option<String>,
    #[arg(short = 'c', long, default_value = "auto")]
    channel: String,
    #[arg(long, conflicts_with = "channel")]
    device: Option<String>,
    #[arg(long)]
    name: Option<String>,
    /// Saved configuration path; defaults to LOCALAPPDATA/win-can-utils/interfaces/NAME.json
    #[arg(long)]
    config: Option<PathBuf>,
    #[arg(long, alias = "list")]
    list_adapters: bool,
    #[arg(short = 'b', long)]
    bitrate: Option<u32>,
    /// Start down; clients can subsequently bring the interface up.
    #[arg(long)]
    down: bool,
    #[arg(long)]
    no_timestamps: bool,
}

#[tokio::main]
async fn main() -> io::Result<()> {
    env_logger::init();
    let cli = Cli::parse();
    if cli.list_adapters {
        if !matches!(cli.driver.as_deref(), Some("gsusb" | "gs_usb")) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "--list-adapters requires gsusb",
            ));
        }
        return adapters::print_adapters(&GsUsbDriver::list_adapters()?, &mut io::stdout().lock());
    }
    let (mut config, path, _reservation) = if let Some(driver) = &cli.driver {
        let driver = match driver.to_ascii_lowercase().as_str() {
            "gs_usb" => "gsusb".to_owned(),
            driver => driver.to_owned(),
        };
        let legacy = (cli.device.is_none()
            && !cli.channel.eq_ignore_ascii_case("auto")
            && crosscan::protocol::validate_name(&cli.channel).is_ok())
        .then_some(cli.channel.as_str());
        let requested = cli
            .name
            .as_deref()
            .filter(|name| !name.eq_ignore_ascii_case("auto"));
        let (name, reservation) = reserve_channel(requested.or(legacy), "can")?;
        let path = cli
            .config
            .clone()
            .map(Ok)
            .unwrap_or_else(|| default_path(&name))?;
        let mut device = cli.device.clone().unwrap_or_else(|| cli.channel.clone());
        if driver == "gsusb" && device.eq_ignore_ascii_case("auto") && !cli.down {
            device = tokio::task::spawn_blocking(|| {
                let list = GsUsbDriver::list_adapters()?;
                let id = adapters::select_adapter(
                    &list,
                    io::stdin().is_terminal(),
                    &mut io::stdin().lock(),
                    &mut io::stdout().lock(),
                )?;
                // Persist a serial selector where possible, so reconnects keep their identity.
                let selected = list.iter().find(|a| a.id == id).unwrap();
                Ok::<_, io::Error>(match (&selected.serial, selected.interface) {
                    (Some(serial), Some(interface)) if !serial.is_empty() => {
                        format!("{serial}/{interface}")
                    }
                    _ => id,
                })
            })
            .await
            .map_err(io::Error::other)??;
        }
        if driver == "slcan" && device.eq_ignore_ascii_case("auto") {
            let ports = serialport::available_ports().map_err(io::Error::other)?;
            if ports.len() != 1 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Select the SLCAN serial port with --channel",
                ));
            }
            device = ports[0].port_name.clone();
        }
        let bitrate = match cli.bitrate {
            Some(bitrate) => bitrate,
            None if driver == "virtual" => 500_000,
            None if driver == "slcan" => {
                use win_can_utils::CanDriver;
                let mut driver = win_can_utils::SlcanDriver::open(&device).await?;
                driver.close_channel().await?;
                driver.get_measured_bitrate().await?
            }
            None => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Specify --bitrate",
                ));
            }
        };
        (
            InterfaceConfig {
                version: 1,
                name,
                driver,
                device,
                bitrate,
                timestamps: !cli.no_timestamps,
                up: !cli.down,
            },
            path,
            reservation,
        )
    } else {
        if cli.device.is_some()
            || cli.bitrate.is_some()
            || cli.channel != "auto"
            || cli.no_timestamps
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Load saved settings with --name or --config; use canctl to change them",
            ));
        }
        let path = match &cli.config {
            Some(path) => path.clone(),
            None => default_path(cli.name.as_deref().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Specify a driver, --name or --config",
                )
            })?)?,
        };
        let config = InterfaceConfig::load(&path)?;
        if cli
            .name
            .as_ref()
            .is_some_and(|name| !name.eq_ignore_ascii_case(&config.name))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "--name does not match saved interface",
            ));
        }
        let (_, reservation) = reserve_channel(Some(&config.name), "can")?;
        (config, path, reservation)
    };
    if cli.down {
        config.up = false;
    }
    if path.exists()
        && !InterfaceConfig::load(&path)?
            .name
            .eq_ignore_ascii_case(&config.name)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Config path belongs to a different interface",
        ));
    }
    config.save(&path)?;
    let (service, mut actor) =
        win_can_utils::service::spawn(config.clone(), path.clone(), Arc::new(HardwareFactory))?;
    let mut listeners = match win_can_utils::ipc::bind(&config.name, service.clone()) {
        Ok(listeners) => listeners,
        Err(error) => {
            service.shutdown().await;
            let _ = actor.await;
            return Err(error);
        }
    };
    println!("CAN interface {} (config: {})", config.name, path.display());
    println!(
        "Use canctl {} show/up/down; Ctrl+C stops the broker.",
        config.name
    );
    let result = tokio::select! {
        signal = tokio::signal::ctrl_c() => signal,
        result = &mut actor => result.map_err(io::Error::other).and_then(|_| Err(io::Error::other("Interface actor stopped"))),
        result = listeners.join_next() => match result {
            Some(Ok(Err(error))) => Err(error),
            Some(Err(error)) => Err(io::Error::other(error)),
            _ => Err(io::Error::other("Pipe listener stopped")),
        },
    };
    listeners.abort_all();
    while listeners.join_next().await.is_some() {}
    if !actor.is_finished() {
        service.shutdown().await;
        let _ = actor.await;
    }
    result
}
