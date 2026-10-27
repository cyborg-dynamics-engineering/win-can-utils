//! Interface actor: serializes hardware operations independently of client IO.
use crate::{
    CanDriver,
    broker::{Broker, SharedBroker},
    config::InterfaceConfig,
};
use async_trait::async_trait;
use crosscan::protocol::*;
use std::{io, path::PathBuf, sync::Arc, time::Duration};
use tokio::sync::{mpsc, oneshot};

#[async_trait]
pub trait DriverFactory: Send + Sync {
    async fn open(&self, config: &InterfaceConfig) -> io::Result<Box<dyn CanDriver>>;
}

#[derive(Debug)]
pub struct ServiceError {
    pub code: &'static str,
    pub message: String,
}
impl From<io::Error> for ServiceError {
    fn from(error: io::Error) -> Self {
        Self {
            code: if error.kind() == io::ErrorKind::InvalidInput {
                "invalid_request"
            } else {
                "driver_error"
            },
            message: error.to_string(),
        }
    }
}

enum Action {
    Request(u64, Command, oneshot::Sender<Result<Reply, ServiceError>>),
    Shutdown(oneshot::Sender<()>),
}

#[derive(Clone)]
pub struct ServiceHandle {
    pub broker: SharedBroker,
    tx: mpsc::Sender<Action>,
}

impl ServiceHandle {
    pub async fn request(&self, client: u64, command: Command) -> Result<Reply, ServiceError> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(Action::Request(client, command, tx))
            .await
            .map_err(|_| ServiceError {
                code: "unavailable",
                message: "Interface stopped".into(),
            })?;
        rx.await.map_err(|_| ServiceError {
            code: "unavailable",
            message: "Interface stopped".into(),
        })?
    }
    pub async fn shutdown(&self) {
        let (tx, rx) = oneshot::channel();
        if self.tx.send(Action::Shutdown(tx)).await.is_ok() {
            let _ = rx.await;
        }
    }
}

pub fn spawn(
    config: InterfaceConfig,
    path: PathBuf,
    factory: Arc<dyn DriverFactory>,
) -> io::Result<(ServiceHandle, tokio::task::JoinHandle<()>)> {
    config.validate()?;
    let broker = Broker::new(Status {
        name: config.name.clone(),
        driver: config.driver.clone(),
        device: config.device.clone(),
        bitrate: config.bitrate,
        desired_up: config.up,
        state: InterfaceState::Down,
        bus_state: BusState::Unknown,
        timestamps: config.timestamps,
        capabilities: Capabilities {
            software_tx_echo: true,
            ..Default::default()
        },
        counters: Counters::default(),
        active_clients: 0,
        last_error: None,
        last_rx_timestamp_us: None,
        hardware_rx_error_counter: None,
        hardware_tx_error_counter: None,
    });
    let (tx, rx) = mpsc::channel(128);
    let handle = ServiceHandle {
        broker: broker.clone(),
        tx,
    };
    let task = tokio::spawn(
        Actor {
            config,
            path,
            factory,
            driver: None,
            broker,
            rx,
        }
        .run(),
    );
    Ok((handle, task))
}

struct Actor {
    config: InterfaceConfig,
    path: PathBuf,
    factory: Arc<dyn DriverFactory>,
    driver: Option<Box<dyn CanDriver>>,
    broker: SharedBroker,
    rx: mpsc::Receiver<Action>,
}

impl Actor {
    async fn up(&mut self) -> io::Result<()> {
        if self.driver.is_some() {
            return Ok(());
        }
        match self.factory.open(&self.config).await {
            Ok(driver) => {
                let mut state = self.broker.lock().unwrap();
                state.status.capabilities = driver.capabilities();
                state.status.capabilities.software_tx_echo = true;
                state.status.state = InterfaceState::Up;
                state.status.last_error = None;
                state.set_bus_state(BusState::Unknown);
                state.status.hardware_rx_error_counter = None;
                state.status.hardware_tx_error_counter = None;
                self.driver = Some(driver);
                Ok(())
            }
            Err(error) => {
                log::error!("Unable to bring {} up: {error}", self.config.name);
                self.failure(&error, false).await;
                Err(error)
            }
        }
    }

    async fn failure(&mut self, error: &io::Error, rx: bool) {
        // A failed transport is retired. Explicit up constructs a fresh backend.
        if let Some(mut driver) = self.driver.take() {
            let _ = driver.close_channel().await;
        }
        let mut state = self.broker.lock().unwrap();
        let disconnected = matches!(
            error.kind(),
            io::ErrorKind::NotConnected
                | io::ErrorKind::BrokenPipe
                | io::ErrorKind::UnexpectedEof
                | io::ErrorKind::NotFound
                | io::ErrorKind::ConnectionReset
        );
        if disconnected && state.status.state != InterfaceState::Disconnected {
            state.status.counters.disconnects += 1;
        }
        if rx {
            state.status.counters.rx_errors += 1;
        }
        state.status.state = if disconnected {
            InterfaceState::Disconnected
        } else {
            InterfaceState::Faulted
        };
        state.status.last_error = Some(error.to_string());
        state.set_bus_state(BusState::Unknown);
        state.status.hardware_rx_error_counter = None;
        state.status.hardware_tx_error_counter = None;
    }

    fn persist(&mut self, config: InterfaceConfig) -> Result<(), ServiceError> {
        config.validate()?;
        config.save(&self.path).map_err(|error| ServiceError {
            code: "configuration_error",
            message: error.to_string(),
        })?;
        self.config = config;
        let mut state = self.broker.lock().unwrap();
        state.status.bitrate = self.config.bitrate;
        state.status.timestamps = self.config.timestamps;
        state.status.desired_up = self.config.up;
        Ok(())
    }

    async fn command(&mut self, client: u64, command: Command) -> Result<Reply, ServiceError> {
        match command {
            Command::Status => Ok(Reply::Status {
                status: self.broker.lock().unwrap().status(),
            }),
            Command::Clients => Ok(Reply::Clients {
                clients: self.broker.lock().unwrap().clients(),
            }),
            Command::Hello { .. } => Err(ServiceError {
                code: "invalid_request",
                message: "Hello is only valid as the first request".into(),
            }),
            Command::Subscribe { subscription } => {
                self.broker.lock().unwrap().update(client, subscription)?;
                Ok(Reply::Ok)
            }
            Command::Configure {
                bitrate,
                timestamps,
            } => {
                if self.driver.is_some() {
                    return Err(ServiceError {
                        code: "interface_up",
                        message: "Bring the interface down before changing bitrate or timestamps"
                            .into(),
                    });
                }
                let mut config = self.config.clone();
                if bitrate.is_none() && timestamps.is_none() {
                    return Err(ServiceError {
                        code: "invalid_request",
                        message: "Specify bitrate or timestamps".into(),
                    });
                }
                if let Some(bitrate) = bitrate {
                    config.bitrate = bitrate;
                }
                if let Some(timestamps) = timestamps {
                    config.timestamps = timestamps;
                }
                self.persist(config)?;
                Ok(Reply::Ok)
            }
            Command::SetUp { up } => {
                let mut config = self.config.clone();
                config.up = up;
                self.persist(config)?;
                if up {
                    self.up().await?;
                } else {
                    let result = if let Some(mut driver) = self.driver.take() {
                        driver.close_channel().await
                    } else {
                        Ok(())
                    };
                    let mut state = self.broker.lock().unwrap();
                    state.status.state = if result.is_ok() {
                        InterfaceState::Down
                    } else {
                        InterfaceState::Faulted
                    };
                    state.set_bus_state(if result.is_ok() {
                        BusState::Stopped
                    } else {
                        BusState::Unknown
                    });
                    state.status.last_error = result.as_ref().err().map(|e| e.to_string());
                    state.status.hardware_rx_error_counter = None;
                    state.status.hardware_tx_error_counter = None;
                    result?;
                }
                Ok(Reply::Status {
                    status: self.broker.lock().unwrap().status(),
                })
            }
            Command::Send { frame } => {
                let frame = frame.to_can()?;
                if frame.is_error() {
                    return Err(ServiceError {
                        code: "invalid_request",
                        message: "Error frames cannot be transmitted".into(),
                    });
                }
                let driver = self.driver.as_mut().ok_or_else(|| ServiceError {
                    code: "interface_down",
                    message: "Interface is not up".into(),
                })?;
                match driver.send_frame(&frame).await {
                    Ok(()) => {
                        let mut state = self.broker.lock().unwrap();
                        state.status.counters.tx_frames += 1;
                        state.publish(&frame, Some(client));
                        Ok(Reply::Ok)
                    }
                    Err(error) => {
                        self.broker.lock().unwrap().status.counters.tx_errors += 1;
                        // Backpressure is a per-send failure, not a transport disconnect.
                        if !matches!(
                            error.kind(),
                            io::ErrorKind::TimedOut
                                | io::ErrorKind::WouldBlock
                                | io::ErrorKind::InvalidInput
                        ) {
                            self.failure(&error, false).await;
                        }
                        Err(error.into())
                    }
                }
            }
        }
    }

    async fn run(mut self) {
        if self.config.up {
            let _ = self.up().await;
        }
        let mut health = tokio::time::interval(Duration::from_secs(1));
        health.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut drain = true;
        loop {
            let notifier = self
                .driver
                .as_ref()
                .and_then(|driver| driver.receive_notifier());
            let is_up = self.driver.is_some();
            let ready = async {
                if !is_up {
                    std::future::pending::<()>().await;
                }
                if !drain {
                    if let Some(notifier) = notifier {
                        notifier.notified().await;
                    } else {
                        tokio::time::sleep(Duration::from_millis(1)).await;
                    }
                }
            };
            tokio::select! {
                action = self.rx.recv() => {
                    match action {
                        Some(Action::Request(client, command, response)) => {
                            let _ = response.send(self.command(client, command).await);
                            drain = true;
                        }
                        Some(Action::Shutdown(response)) => {
                            if let Some(mut driver) = self.driver.take() { let _ = driver.close_channel().await; }
                            let _ = response.send(()); break;
                        }
                        None => break,
                    }
                }
                _ = ready => {
                    if let Some(driver) = &mut self.driver {
                        match driver.read_frames().await {
                            Ok(frames) => {
                                drain = !frames.is_empty();
                                let mut state = self.broker.lock().unwrap();
                                for mut frame in frames {
                                    if !self.config.timestamps { frame.set_timestamp(None); }
                                    state.publish(&frame, None);
                                }
                            }
                            Err(error) => { self.failure(&error, true).await; drain = false; }
                        }
                    }
                }
                _ = health.tick(), if is_up => {
                    if let Some(driver) = &mut self.driver {
                        match driver.health().await {
                            Ok(health) => {
                                let mut state = self.broker.lock().unwrap();
                                // Unknown is absence of information, not evidence of recovery.
                                if health.bus_state != BusState::Unknown { state.set_bus_state(health.bus_state); }
                                state.status.hardware_rx_error_counter = health.rx_error_counter;
                                state.status.hardware_tx_error_counter = health.tx_error_counter;
                            }
                            Err(error) => self.failure(&error, true).await,
                        }
                    }
                }
            }
        }
        if let Some(mut driver) = self.driver.take() {
            let _ = driver.close_channel().await;
        }
    }
}
