//! Run the real CAN broker and adapter bridge, substituting only the kernel packet rings.
use crosscan::{can::CanFrame, protocol::*};
use std::{
    collections::{HashMap, VecDeque},
    io,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    sync::{mpsc, oneshot},
    task::{JoinHandle, JoinSet},
    time::timeout,
};
use win_can_utils::{
    config::InterfaceConfig,
    hardware::HardwareFactory,
    network_adapter::{
        NetworkConfig, PacketDevice,
        bridge::{self, BridgeOptions, BridgeReport},
        ip::{self, Incoming},
    },
    service::ServiceHandle,
};

struct SimulatedAdapter {
    from_windows: Mutex<std::sync::mpsc::Receiver<Vec<u8>>>,
    to_windows: mpsc::Sender<Vec<u8>>,
    ring_full: AtomicBool,
    removed: AtomicBool,
}
impl PacketDevice for SimulatedAdapter {
    fn receive(&self, wait: Duration) -> io::Result<Option<Vec<u8>>> {
        if self.removed.load(Ordering::Relaxed) {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "Adapter removed",
            ));
        }
        match self.from_windows.lock().unwrap().recv_timeout(wait) {
            Ok(packet) => Ok(Some(packet)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Ok(None),
            Err(_) => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "Packet queue closed",
            )),
        }
    }
    fn send(&self, packet: &[u8]) -> io::Result<()> {
        if self.ring_full.load(Ordering::Relaxed) {
            return Err(io::Error::new(io::ErrorKind::WouldBlock, "Ring full"));
        }
        self.to_windows
            .try_send(packet.to_vec())
            .map_err(|_| io::Error::new(io::ErrorKind::WouldBlock, "Simulated ring full"))
    }
}

struct Harness {
    network: NetworkConfig,
    path: PathBuf,
    broker: ServiceHandle,
    actor: JoinHandle<()>,
    pipes: JoinSet<io::Result<()>>,
    device: Arc<SimulatedAdapter>,
    transmit: std::sync::mpsc::Sender<Vec<u8>>,
    receive: mpsc::Receiver<Vec<u8>>,
    stop: Option<oneshot::Sender<()>>,
    bridge: Option<JoinHandle<io::Result<BridgeReport>>>,
    pending: HashMap<u16, VecDeque<ServerMessage>>,
}

impl Harness {
    fn new(max_clients: usize, idle: Duration) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let name = format!(
            "nettest_{}_{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        );
        let path = PathBuf::from("target").join(format!("{name}.json"));
        let config = InterfaceConfig {
            version: 1,
            name: name.clone(),
            driver: "virtual".into(),
            device: "test".into(),
            bitrate: 500_000,
            timestamps: true,
            up: true,
        };
        config.save(&path).unwrap();
        let (broker, actor) =
            win_can_utils::service::spawn(config, path.clone(), Arc::new(HardwareFactory)).unwrap();
        let pipes = win_can_utils::ipc::bind(&name, broker.clone()).unwrap();
        let (transmit, from_windows) = std::sync::mpsc::channel();
        let (to_windows, receive) = mpsc::channel(4096);
        let device = Arc::new(SimulatedAdapter {
            from_windows: Mutex::new(from_windows),
            to_windows,
            ring_full: AtomicBool::new(false),
            removed: AtomicBool::new(false),
        });
        let network = NetworkConfig::default();
        let options = BridgeOptions {
            interface: name,
            network: network.clone(),
            idle_timeout: idle,
            max_clients,
        };
        let (stop_tx, stop_rx) = oneshot::channel();
        let bridge = tokio::spawn(bridge::run(device.clone(), options, async {
            let _ = stop_rx.await;
        }));
        Self {
            network,
            path,
            broker,
            actor,
            pipes,
            device,
            transmit,
            receive,
            stop: Some(stop_tx),
            bridge: Some(bridge),
            pending: HashMap::new(),
        }
    }
    fn send(&self, port: u16, id: u64, command: Command) {
        let payload = serde_json::to_vec(&Request {
            version: VERSION,
            id,
            command,
        })
        .unwrap();
        self.transmit
            .send(
                ip::udp_packet(
                    self.network.local,
                    self.network.peer,
                    port,
                    self.network.port,
                    &payload,
                )
                .unwrap(),
            )
            .unwrap();
    }
    async fn receive_message(&mut self) -> (u16, ServerMessage) {
        let packet = timeout(Duration::from_secs(4), self.receive.recv())
            .await
            .unwrap()
            .unwrap();
        let port = u16::from_be_bytes([packet[22], packet[23]]);
        let reverse = NetworkConfig {
            local: self.network.peer,
            peer: self.network.local,
            port,
            ..self.network.clone()
        };
        let Incoming::Datagram {
            source_port,
            payload,
        } = ip::parse(&packet, &reverse).unwrap()
        else {
            panic!("Expected UDP")
        };
        assert_eq!(source_port, self.network.port);
        (port, serde_json::from_slice(&payload).unwrap())
    }
    async fn reply(&mut self, port: u16, id: u64) -> ServerBody {
        let matching = |message: &ServerMessage| matches!(&message.body, ServerBody::Reply { id: found, .. } | ServerBody::Error { id: found, .. } if *found == id);
        if let Some(messages) = self.pending.get_mut(&port) {
            if let Some(index) = messages.iter().position(matching) {
                return messages.remove(index).unwrap().body;
            }
        }
        loop {
            let (found, message) = self.receive_message().await;
            if found == port && matching(&message) {
                return message.body;
            }
            self.pending.entry(found).or_default().push_back(message);
        }
    }
    async fn frame(&mut self, port: u16) -> FrameEvent {
        if let Some(messages) = self.pending.get_mut(&port) {
            if let Some(index) = messages
                .iter()
                .position(|m| matches!(m.body, ServerBody::Frame { .. }))
            {
                let ServerBody::Frame { event } = messages.remove(index).unwrap().body else {
                    unreachable!()
                };
                return event;
            }
        }
        loop {
            let (found, message) = self.receive_message().await;
            if found == port {
                if let ServerBody::Frame { event } = message.body {
                    return event;
                }
            }
            self.pending.entry(found).or_default().push_back(message);
        }
    }
    async fn hello(&mut self, port: u16, subscription: Subscription) -> u64 {
        self.send(port, 1, Command::Hello { subscription });
        match self.reply(port, 1).await {
            ServerBody::Reply {
                reply: Reply::Hello { client_id, .. },
                ..
            } => client_id,
            other => panic!("{other:?}"),
        }
    }
    async fn stop(&mut self) -> io::Result<BridgeReport> {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        let result = timeout(Duration::from_secs(4), self.bridge.take().unwrap())
            .await
            .unwrap()
            .unwrap();
        self.pipes.abort_all();
        while self.pipes.join_next().await.is_some() {}
        self.broker.shutdown().await;
        result
    }
}
impl Drop for Harness {
    fn drop(&mut self) {
        self.stop.take();
        if let Some(bridge) = &self.bridge {
            bridge.abort();
        }
        self.actor.abort();
        self.pipes.abort_all();
        let _ = std::fs::remove_file(&self.path);
    }
}

#[tokio::test]
async fn udp_sessions_preserve_filters_echo_status_and_lifecycle() {
    let mut h = Harness::new(32, Duration::from_secs(10));
    let sender = h.hello(43000, Subscription::default()).await;
    h.hello(
        43001,
        Subscription {
            filters: vec![Filter {
                id: 0x123,
                mask: 0x8000_07ff,
                invert: false,
            }],
            ..Default::default()
        },
    )
    .await;
    let data = Frame::from(&CanFrame::new(0x123, &[0, 10, 255]).unwrap());
    h.send(43000, 2, Command::Send { frame: data });
    assert!(matches!(
        h.reply(43000, 2).await,
        ServerBody::Reply {
            reply: Reply::Ok,
            ..
        }
    ));
    let echo = h.frame(43001).await;
    assert_eq!(echo.direction, Direction::LocalTx);
    assert_eq!(echo.source_client_id, Some(sender));
    assert_eq!(echo.frame.data, [0, 10, 255]);
    h.send(
        43000,
        3,
        Command::Subscribe {
            subscription: Subscription {
                receive_own_messages: true,
                ..Default::default()
            },
        },
    );
    assert!(matches!(
        h.reply(43000, 3).await,
        ServerBody::Reply {
            reply: Reply::Ok,
            ..
        }
    ));
    h.send(
        43000,
        4,
        Command::Send {
            frame: Frame::from(&CanFrame::new(0x124, &[2]).unwrap()),
        },
    );
    assert!(matches!(
        h.reply(43000, 4).await,
        ServerBody::Reply {
            reply: Reply::Ok,
            ..
        }
    ));
    assert_eq!(h.frame(43000).await.frame.id, 0x124);
    h.send(43000, 5, Command::SetUp { up: false });
    assert!(
        matches!(h.reply(43000, 5).await, ServerBody::Reply { reply: Reply::Status { status }, .. } if status.state == InterfaceState::Down)
    );
    h.send(
        43000,
        6,
        Command::Configure {
            bitrate: Some(250_000),
            timestamps: None,
        },
    );
    assert!(matches!(
        h.reply(43000, 6).await,
        ServerBody::Reply {
            reply: Reply::Ok,
            ..
        }
    ));
    assert_eq!(InterfaceConfig::load(&h.path).unwrap().bitrate, 250_000);
    h.send(43000, 7, Command::SetUp { up: true });
    assert!(
        matches!(h.reply(43000, 7).await, ServerBody::Reply { reply: Reply::Status { status }, .. } if status.state == InterfaceState::Up)
    );
    let report = h.stop().await.unwrap();
    assert_eq!(report.sessions_created, 2);
    assert_eq!(report.accepted_sends, 2);
    assert_eq!(report.active_clients, 0);
    assert_eq!(report.invalid_packets, 0);
}

#[tokio::test]
async fn malformed_clients_and_versions_do_not_take_down_the_bridge() {
    let mut h = Harness::new(32, Duration::from_secs(10));
    h.send(43000, 10, Command::Status);
    assert!(
        matches!(h.reply(43000, 10).await, ServerBody::Error { code, .. } if code == "hello_required")
    );
    let payload = serde_json::to_vec(&Request {
        version: 99,
        id: 11,
        command: Command::Status,
    })
    .unwrap();
    h.transmit
        .send(
            ip::udp_packet(
                h.network.local,
                h.network.peer,
                43000,
                h.network.port,
                &payload,
            )
            .unwrap(),
        )
        .unwrap();
    assert!(
        matches!(h.reply(43000, 11).await, ServerBody::Error { code, .. } if code == "invalid_version")
    );
    h.transmit
        .send(
            ip::udp_packet(
                h.network.local,
                h.network.peer,
                43000,
                h.network.port,
                b"not-json",
            )
            .unwrap(),
        )
        .unwrap();
    assert!(
        matches!(h.reply(43000, 0).await, ServerBody::Error { code, .. } if code == "invalid_request")
    );
    h.transmit.send(vec![0; 3]).unwrap();
    h.hello(
        43001,
        Subscription {
            receive: false,
            ..Default::default()
        },
    )
    .await;
    h.send(43001, 2, Command::Status);
    assert!(matches!(
        h.reply(43001, 2).await,
        ServerBody::Reply {
            reply: Reply::Status { .. },
            ..
        }
    ));
    let report = h.stop().await.unwrap();
    assert_eq!(report.protocol_errors, 2);
    assert_eq!(report.invalid_packets, 1);
}

#[tokio::test]
async fn idle_udp_sessions_expire_and_release_the_client_limit() {
    let mut h = Harness::new(1, Duration::from_secs(1));
    h.hello(
        43000,
        Subscription {
            receive: false,
            ..Default::default()
        },
    )
    .await;
    h.send(
        43001,
        1,
        Command::Hello {
            subscription: Subscription::default(),
        },
    );
    assert!(
        matches!(h.reply(43001, 1).await, ServerBody::Error { code, .. } if code == "client_limit")
    );
    tokio::time::sleep(Duration::from_millis(1200)).await;
    h.hello(
        43001,
        Subscription {
            receive: false,
            ..Default::default()
        },
    )
    .await;
    let report = h.stop().await.unwrap();
    assert_eq!(report.sessions_created, 2);
    assert_eq!(report.sessions_expired, 1);
}

#[tokio::test]
async fn ring_pressure_drops_packets_without_stalling_broker_control() {
    let mut h = Harness::new(32, Duration::from_secs(10));
    h.hello(43000, Subscription::default()).await;
    h.device.ring_full.store(true, Ordering::Relaxed);
    h.broker
        .request(
            0,
            Command::Send {
                frame: Frame::from(&CanFrame::new(1, &[1]).unwrap()),
            },
        )
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    h.device.ring_full.store(false, Ordering::Relaxed);
    h.send(43000, 2, Command::Status);
    assert!(
        matches!(h.reply(43000, 2).await, ServerBody::Reply { reply: Reply::Status { status }, .. } if status.counters.tx_frames == 1)
    );
    let report = h.stop().await.unwrap();
    assert!(report.ring_drops > 0);
}

#[tokio::test]
async fn adapter_removal_stops_workers_and_releases_broker_sessions() {
    let mut h = Harness::new(32, Duration::from_secs(10));
    h.hello(43000, Subscription::default()).await;
    h.device.removed.store(true, Ordering::Relaxed);
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(h.stop().await.is_err());
}

#[tokio::test]
async fn cancelling_the_bridge_also_stops_its_blocking_packet_reader() {
    let mut h = Harness::new(32, Duration::from_secs(10));
    h.hello(43000, Subscription::default()).await;
    let task = h.bridge.take().unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    timeout(Duration::from_secs(3), async {
        while Arc::strong_count(&h.device) != 1 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("Cancelled bridge leaked its blocking reader");
    h.pipes.abort_all();
    while h.pipes.join_next().await.is_some() {}
    h.broker.shutdown().await;
}

/// Emulate only the TUN/Windows IP boundary; the application uses real UDP sockets.
struct LoopbackIpStack {
    socket: std::net::UdpSocket,
    network: NetworkConfig,
}
impl PacketDevice for LoopbackIpStack {
    fn receive(&self, _: Duration) -> io::Result<Option<Vec<u8>>> {
        let mut payload = [0u8; ip::MAX_UDP_PAYLOAD];
        match self.socket.recv_from(&mut payload) {
            Ok((length, source)) => ip::udp_packet(
                self.network.local,
                self.network.peer,
                source.port(),
                self.network.port,
                &payload[..length],
            )
            .map(Some)
            .map_err(|_| io::Error::other("Invalid loopback UDP")),
            // Windows UDP reports ICMP port-unreachable when a previous app has
            // closed its socket. That is an IP event, not adapter removal.
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::ConnectionReset
                ) =>
            {
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }
    fn send(&self, packet: &[u8]) -> io::Result<()> {
        let port = u16::from_be_bytes([packet[22], packet[23]]);
        let reverse = NetworkConfig {
            local: self.network.peer,
            peer: self.network.local,
            port,
            ..self.network.clone()
        };
        let Incoming::Datagram { payload, .. } =
            ip::parse(packet, &reverse).map_err(|_| io::Error::other("Invalid response packet"))?
        else {
            return Err(io::Error::other("Expected UDP"));
        };
        self.socket
            .send_to(&payload, (std::net::Ipv4Addr::LOCALHOST, port))?;
        Ok(())
    }
}

#[tokio::test]
async fn ordinary_udp_socket_and_optional_python_client_use_the_bridge() {
    let mut h = Harness::new(32, Duration::from_secs(10));
    h.stop.take().unwrap().send(()).unwrap();
    h.bridge.take().unwrap().await.unwrap().unwrap();
    let socket = std::net::UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    socket
        .set_write_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    let address = socket.local_addr().unwrap();
    let device = Arc::new(LoopbackIpStack {
        socket,
        network: h.network.clone(),
    });
    let name = h.broker.broker.lock().unwrap().status().name;
    let options = BridgeOptions {
        interface: name,
        network: h.network.clone(),
        ..Default::default()
    };
    let (stop, stopped) = oneshot::channel();
    h.stop = Some(stop);
    h.bridge = Some(tokio::spawn(bridge::run(device, options, async {
        let _ = stopped.await;
    })));
    tokio::task::spawn_blocking(move || {
        let app = std::net::UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        app.connect(address).unwrap();
        app.set_read_timeout(Some(Duration::from_secs(4))).unwrap();
        let mut buffer = [0u8; 65535];
        let hello = Request {
            version: VERSION,
            id: 1,
            command: Command::Hello {
                subscription: Subscription {
                    receive_own_messages: true,
                    ..Default::default()
                },
            },
        };
        app.send(&serde_json::to_vec(&hello).unwrap()).unwrap();
        let length = app.recv(&mut buffer).unwrap();
        assert!(matches!(
            serde_json::from_slice::<ServerMessage>(&buffer[..length])
                .unwrap()
                .body,
            ServerBody::Reply {
                reply: Reply::Hello { .. },
                ..
            }
        ));
        let send = Request {
            version: VERSION,
            id: 2,
            command: Command::Send {
                frame: Frame::from(&CanFrame::new(0x321, &[1, 2]).unwrap()),
            },
        };
        app.send(&serde_json::to_vec(&send).unwrap()).unwrap();
        let mut reply = false;
        let mut echo = false;
        for _ in 0..2 {
            let length = app.recv(&mut buffer).unwrap();
            match serde_json::from_slice::<ServerMessage>(&buffer[..length])
                .unwrap()
                .body
            {
                ServerBody::Reply {
                    id: 2,
                    reply: Reply::Ok,
                } => reply = true,
                ServerBody::Frame { event } if event.frame.id == 0x321 => echo = true,
                other => panic!("{other:?}"),
            }
        }
        assert!(reply && echo);
    })
    .await
    .unwrap();
    let python = std::env::var_os("WINCAN_TEST_PYTHON");
    let expected_sends = if let Some(python) = python {
        use std::os::windows::process::CommandExt;
        let output = tokio::task::spawn_blocking(move || {
            std::process::Command::new(python)
                .arg("examples/network_can.py")
                .args([
                    "--local",
                    "127.0.0.1",
                    "--peer",
                    "127.0.0.1",
                    "--port",
                    &address.port().to_string(),
                    "--send",
                    "123#010203",
                    "--receive-own-messages",
                ])
                .creation_flags(0x08000000)
                .output()
                .unwrap()
        })
        .await
        .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("local_tx"));
        2
    } else {
        1
    };
    let report = h.stop().await.unwrap();
    assert_eq!(report.accepted_sends, expected_sends);
}
