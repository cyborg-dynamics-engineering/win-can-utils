//! Exercises the actual Windows pipes and crosscan client without CAN hardware.
use async_trait::async_trait;
use crosscan::{
    CanInterface, can::CanFrame, protocol::*, service::ServiceClient, win_can::WindowsCan,
};
use std::{
    collections::VecDeque,
    io,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{sync::Notify, task::JoinSet, time::timeout};
use win_can_utils::{
    CanDriver,
    config::InterfaceConfig,
    drivers::can_driver::DriverHealth,
    service::{DriverFactory, ServiceHandle},
};

#[derive(Default)]
struct Hardware {
    frames: Mutex<VecDeque<CanFrame>>,
    ready: Arc<Notify>,
    disconnected: AtomicBool,
    reject_tx: AtomicBool,
    opens: AtomicUsize,
    closes: AtomicUsize,
    bus: Mutex<BusState>,
    reads: AtomicUsize,
    during_empty_read: Mutex<Option<CanFrame>>,
}
struct Factory(Arc<Hardware>);
struct Driver(Arc<Hardware>);

#[async_trait]
impl DriverFactory for Factory {
    async fn open(&self, _: &InterfaceConfig) -> io::Result<Box<dyn CanDriver>> {
        if self.0.disconnected.load(Ordering::SeqCst) {
            return Err(io::Error::new(io::ErrorKind::NotConnected, "unplugged"));
        }
        self.0.opens.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(Driver(self.0.clone())))
    }
}

#[async_trait]
impl CanDriver for Driver {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            hardware_can_fd: Some(true),
            can_fd: false,
            hardware_timestamps: Some(true),
            bus_state: true,
            software_tx_echo: true,
        }
    }
    async fn health(&mut self) -> io::Result<DriverHealth> {
        Ok(DriverHealth {
            bus_state: *self.0.bus.lock().unwrap(),
            rx_error_counter: Some(12),
            tx_error_counter: Some(34),
        })
    }
    fn receive_notifier(&self) -> Option<Arc<Notify>> {
        Some(self.0.ready.clone())
    }
    async fn enable_timestamp(&mut self) -> io::Result<()> {
        Ok(())
    }
    async fn set_bitrate(&mut self, _: u32) -> io::Result<()> {
        Ok(())
    }
    async fn get_bitrate(&self) -> Option<u32> {
        Some(500_000)
    }
    async fn open_channel(&mut self) -> io::Result<()> {
        Ok(())
    }
    async fn close_channel(&mut self) -> io::Result<()> {
        self.0.closes.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn send_frame(&mut self, _: &CanFrame) -> io::Result<()> {
        if self.0.reject_tx.load(Ordering::SeqCst) {
            return Err(io::Error::new(io::ErrorKind::WouldBlock, "busy"));
        }
        Ok(())
    }
    async fn read_frames(&mut self) -> io::Result<Vec<CanFrame>> {
        self.0.reads.fetch_add(1, Ordering::SeqCst);
        if self.0.disconnected.load(Ordering::SeqCst) {
            return Err(io::Error::new(io::ErrorKind::NotConnected, "unplugged"));
        }
        let mut queue = self.0.frames.lock().unwrap();
        let frames: Vec<_> = queue.drain(..).collect();
        if frames.is_empty() {
            if let Some(frame) = self.0.during_empty_read.lock().unwrap().take() {
                queue.push_back(frame);
                self.0.ready.notify_one();
            }
        }
        Ok(frames)
    }
}

struct Harness {
    config: InterfaceConfig,
    path: PathBuf,
    service: ServiceHandle,
    actor: tokio::task::JoinHandle<()>,
    listeners: JoinSet<io::Result<()>>,
    hw: Arc<Hardware>,
}

impl Harness {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let name = format!(
            "test_{}_{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        );
        let path = PathBuf::from("target").join(format!("{name}.json"));
        let config = InterfaceConfig {
            version: VERSION,
            name,
            driver: "virtual".into(),
            device: "test".into(),
            bitrate: 500_000,
            timestamps: true,
            up: true,
        };
        config.save(&path).unwrap();
        let hw = Arc::new(Hardware::default());
        let (service, actor) = win_can_utils::service::spawn(
            config.clone(),
            path.clone(),
            Arc::new(Factory(hw.clone())),
        )
        .unwrap();
        let listeners = win_can_utils::ipc::bind(&config.name, service.clone()).unwrap();
        Self {
            config,
            path,
            service,
            actor,
            listeners,
            hw,
        }
    }
    async fn client(&self, subscription: Subscription) -> ServiceClient {
        ServiceClient::connect(&self.config.name, subscription)
            .await
            .unwrap()
    }
    async fn status(&self) -> Status {
        match self.service.request(0, Command::Status).await.unwrap() {
            Reply::Status { status } => status,
            _ => unreachable!(),
        }
    }
    async fn stop(&mut self) {
        self.listeners.abort_all();
        while self.listeners.join_next().await.is_some() {}
        self.service.shutdown().await;
        timeout(Duration::from_secs(3), &mut self.actor)
            .await
            .unwrap()
            .unwrap();
    }
}
impl Drop for Harness {
    fn drop(&mut self) {
        self.actor.abort();
        self.listeners.abort_all();
        let _ = std::fs::remove_file(&self.path);
    }
}

fn data(id: u32) -> Frame {
    Frame::from(&CanFrame::new(id, &[10, 255, 0]).unwrap())
}
async fn event(client: &mut ServiceClient) -> FrameEvent {
    timeout(Duration::from_secs(3), client.next_event())
        .await
        .unwrap()
        .unwrap()
}
async fn legacy(name: &str, write: bool) -> WindowsCan {
    timeout(Duration::from_secs(3), async {
        loop {
            let result = if write {
                WindowsCan::open_write_only(name)
            } else {
                WindowsCan::open_read_only(name)
            };
            match result {
                Ok(client) => return client,
                Err(error) if error.raw_os_error() == Some(231) => {
                    tokio::time::sleep(Duration::from_millis(1)).await
                }
                Err(error) => panic!("{error}"),
            }
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn subscribers_filters_echo_and_legacy_clients_share_an_interface() {
    let mut h = Harness::new();
    let mut sender = h.client(Subscription::default()).await;
    let mut a = h
        .client(Subscription {
            filters: vec![Filter {
                id: 0x123,
                mask: 0x7ff,
                invert: false,
            }],
            ..Default::default()
        })
        .await;
    let mut b = h.client(Subscription::default()).await;
    let mut legacy_a = legacy(&h.config.name, false).await;
    let mut legacy_b = legacy(&h.config.name, false).await;
    assert_eq!(legacy_a.get_bitrate().await.unwrap(), Some(500_000));
    // Ensure both legacy sessions have registered before publishing.
    timeout(Duration::from_secs(3), async {
        while h.status().await.active_clients < 5 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    sender
        .request(Command::Send { frame: data(0x123) })
        .await
        .unwrap();
    let first = event(&mut a).await;
    assert_eq!(first.direction, Direction::LocalTx);
    assert_eq!(first.source_client_id, Some(sender.client_id));
    assert_eq!(first.hardware_timestamp_us, None);
    assert_eq!(event(&mut b).await.sequence, first.sequence);
    assert_eq!(
        timeout(Duration::from_secs(3), legacy_a.read_frame())
            .await
            .unwrap()
            .unwrap()
            .id(),
        0x123
    );
    assert_eq!(
        timeout(Duration::from_secs(3), legacy_b.read_frame())
            .await
            .unwrap()
            .unwrap()
            .id(),
        0x123
    );
    assert!(
        timeout(Duration::from_millis(30), sender.next_event())
            .await
            .is_err()
    );

    sender
        .request(Command::Subscribe {
            subscription: Subscription {
                receive_own_messages: true,
                ..Default::default()
            },
        })
        .await
        .unwrap();
    sender
        .request(Command::Send { frame: data(0x124) })
        .await
        .unwrap();
    assert_eq!(event(&mut sender).await.frame.id, 0x124);
    assert_eq!(event(&mut b).await.frame.id, 0x124);
    assert!(
        timeout(Duration::from_millis(30), a.next_event())
            .await
            .is_err()
    );

    let mut rx = CanFrame::new(0x123, &[4]).unwrap();
    rx.set_timestamp(Some(9876));
    h.hw.frames.lock().unwrap().push_back(rx);
    h.hw.ready.notify_one();
    let rx = event(&mut a).await;
    assert_eq!(rx.direction, Direction::Rx);
    assert_eq!(rx.hardware_timestamp_us, Some(9876));
    assert!(rx.timestamp_us > 1_000_000_000);
    let status = h.status().await;
    assert_eq!(status.counters.tx_frames, 2);
    assert_eq!(status.counters.rx_frames, 1);
    assert_eq!(status.capabilities.hardware_can_fd, Some(true));
    assert!(!status.capabilities.can_fd);

    // Several transmitters can remain connected together too.
    let mut legacy_tx_a = legacy(&h.config.name, true).await;
    let mut legacy_tx_b = legacy(&h.config.name, true).await;
    legacy_tx_a
        .write_frame(CanFrame::new(0x123, &[1]).unwrap())
        .await
        .unwrap();
    legacy_tx_b
        .write_frame(CanFrame::new(0x123, &[2]).unwrap())
        .await
        .unwrap();
    let one = event(&mut a).await;
    let two = event(&mut a).await;
    assert_ne!(one.source_client_id, two.source_client_id);
    h.stop().await;
}

#[tokio::test]
async fn state_configuration_disconnect_recovery_and_counters_persist_correctly() {
    let mut h = Harness::new();
    let client = h
        .client(Subscription {
            receive: false,
            ..Default::default()
        })
        .await;
    assert_eq!(h.status().await.state, InterfaceState::Up);
    client.request(Command::SetUp { up: false }).await.unwrap();
    assert_eq!(h.status().await.state, InterfaceState::Down);
    assert!(!InterfaceConfig::load(&h.path).unwrap().up);
    assert!(
        client
            .request(Command::Send { frame: data(1) })
            .await
            .is_err()
    );
    client
        .request(Command::Configure {
            bitrate: Some(250_000),
            timestamps: Some(false),
        })
        .await
        .unwrap();
    assert_eq!(InterfaceConfig::load(&h.path).unwrap().bitrate, 250_000);
    client.request(Command::SetUp { up: true }).await.unwrap();
    assert_eq!(h.hw.opens.load(Ordering::SeqCst), 2);
    assert!(
        client
            .request(Command::Configure {
                bitrate: Some(500_000),
                timestamps: Some(true)
            })
            .await
            .is_err()
    );

    h.hw.reject_tx.store(true, Ordering::SeqCst);
    assert!(
        client
            .request(Command::Send { frame: data(1) })
            .await
            .is_err()
    );
    assert_eq!(h.status().await.state, InterfaceState::Up);
    assert_eq!(h.status().await.counters.tx_errors, 1);
    assert_eq!(h.status().await.counters.tx_frames, 0);
    h.hw.reject_tx.store(false, Ordering::SeqCst);
    *h.hw.bus.lock().unwrap() = BusState::BusOff;
    timeout(Duration::from_secs(3), async {
        while h.status().await.bus_state != BusState::BusOff {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let status = h.status().await;
    assert_eq!(status.counters.bus_off_events, 1);
    assert_eq!(status.hardware_rx_error_counter, Some(12));
    assert_eq!(status.hardware_tx_error_counter, Some(34));

    h.hw.disconnected.store(true, Ordering::SeqCst);
    h.hw.ready.notify_one();
    timeout(Duration::from_secs(3), async {
        while h.status().await.state != InterfaceState::Disconnected {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(h.status().await.counters.disconnects, 1);
    assert!(h.status().await.last_error.is_some());
    h.hw.disconnected.store(false, Ordering::SeqCst);
    client.request(Command::SetUp { up: true }).await.unwrap();
    assert_eq!(h.status().await.state, InterfaceState::Up);
    assert_eq!(h.hw.opens.load(Ordering::SeqCst), 3);
    client.request(Command::SetUp { up: false }).await.unwrap();
    h.stop().await;

    let saved = InterfaceConfig::load(&h.path).unwrap();
    assert!(!saved.up);
    assert_eq!(saved.bitrate, 250_000);
    let (service, actor) =
        win_can_utils::service::spawn(saved, h.path.clone(), Arc::new(Factory(h.hw.clone())))
            .unwrap();
    assert!(
        matches!(service.request(0, Command::Status).await.unwrap(), Reply::Status { status } if status.state == InterfaceState::Down && !status.desired_up)
    );
    assert_eq!(
        h.hw.opens.load(Ordering::SeqCst),
        3,
        "Restart brought a saved-down interface up"
    );
    service.shutdown().await;
    actor.await.unwrap();
}

#[tokio::test]
async fn full_subscriber_queue_never_blocks_other_subscribers() {
    let mut h = Harness::new();
    let (slow, mut slow_rx) = win_can_utils::broker::Broker::subscribe(
        &h.service.broker,
        Subscription {
            queue_capacity: 1,
            ..Default::default()
        },
    )
    .unwrap();
    let (fast, mut fast_rx) = win_can_utils::broker::Broker::subscribe(
        &h.service.broker,
        Subscription {
            queue_capacity: 1,
            ..Default::default()
        },
    )
    .unwrap();
    for sequence in 0..4096u32 {
        let frame = CanFrame::new(1, &sequence.to_le_bytes()).unwrap();
        h.service.broker.lock().unwrap().publish(&frame, None);
        assert_eq!(
            fast_rx.try_recv().unwrap().frame.data,
            sequence.to_le_bytes()
        );
    }
    let clients = h.service.broker.lock().unwrap().clients();
    assert_eq!(
        clients
            .iter()
            .find(|c| c.client_id == slow.id)
            .unwrap()
            .dropped,
        4095
    );
    assert_eq!(
        clients
            .iter()
            .find(|c| c.client_id == fast.id)
            .unwrap()
            .dropped,
        0
    );
    assert_eq!(h.status().await.counters.client_drops, 4095);
    assert_eq!(slow_rx.try_recv().unwrap().frame.data, 0u32.to_le_bytes());
    drop(slow);
    drop(fast);
    assert_eq!(h.status().await.active_clients, 0);
    h.stop().await;
}

#[tokio::test]
async fn slow_pipe_reader_does_not_block_transmit_or_a_fast_reader() {
    use tokio::net::windows::named_pipe::ClientOptions;
    let mut h = Harness::new();
    let sender = h
        .client(Subscription {
            receive: false,
            ..Default::default()
        })
        .await;
    let mut fast = h
        .client(Subscription {
            queue_capacity: 4096,
            ..Default::default()
        })
        .await;
    let mut slow = ClientOptions::new()
        .open(pipe_name(&h.config.name).unwrap())
        .unwrap();
    write_message(
        &mut slow,
        &Request {
            version: VERSION,
            id: 1,
            command: Command::Hello {
                subscription: Subscription {
                    queue_capacity: 1,
                    ..Default::default()
                },
            },
        },
    )
    .await
    .unwrap();
    let hello: ServerMessage = read_message(&mut slow).await.unwrap();
    let slow_id = match hello.body {
        ServerBody::Reply {
            reply: Reply::Hello { client_id, .. },
            ..
        } => client_id,
        _ => panic!("No hello"),
    };
    // Leave slow's read side idle until its Windows pipe buffer and queue fill.
    timeout(Duration::from_secs(15), async {
        for _ in 0..2000 {
            sender
                .request(Command::Send { frame: data(1) })
                .await
                .unwrap();
        }
        sender
            .request(Command::Send { frame: data(0x7ff) })
            .await
            .unwrap();
        loop {
            if event(&mut fast).await.frame.id == 0x7ff {
                break;
            }
        }
    })
    .await
    .expect("A slow pipe reader blocked the interface");
    let clients = h.service.broker.lock().unwrap().clients();
    assert!(
        clients
            .iter()
            .find(|c| c.client_id == slow_id)
            .unwrap()
            .dropped
            > 0
    );
    assert_eq!(h.status().await.counters.tx_frames, 2001);
    drop(slow);
    timeout(Duration::from_secs(3), async {
        while h.status().await.active_clients != 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    h.stop().await;
}

#[tokio::test]
async fn idle_notifier_does_not_poll_and_retains_a_read_wait_race() {
    let mut h = Harness::new();
    let mut client = h.client(Subscription::default()).await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    let reads = h.hw.reads.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        h.hw.reads.load(Ordering::SeqCst),
        reads,
        "Idle driver was polled"
    );
    let frame = CanFrame::new(0x123, &[1]).unwrap();
    *h.hw.during_empty_read.lock().unwrap() = Some(frame);
    h.hw.ready.notify_one();
    assert_eq!(event(&mut client).await.frame.id, 0x123);
    h.stop().await;
}

#[tokio::test]
async fn rejected_config_save_leaves_live_state_and_saved_settings_unchanged() {
    use std::os::windows::fs::OpenOptionsExt;
    let mut h = Harness::new();
    let client = h
        .client(Subscription {
            receive: false,
            ..Default::default()
        })
        .await;
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(1)
        .open(&h.path)
        .unwrap();
    let error = client
        .request(Command::SetUp { up: false })
        .await
        .unwrap_err();
    assert!(error.to_string().contains("configuration_error"));
    assert_eq!(h.status().await.state, InterfaceState::Up);
    assert!(h.status().await.desired_up);
    assert_eq!(h.hw.closes.load(Ordering::SeqCst), 0);
    drop(lock);
    assert!(InterfaceConfig::load(&h.path).unwrap().up);
    client.request(Command::SetUp { up: false }).await.unwrap();
    assert!(!InterfaceConfig::load(&h.path).unwrap().up);
    h.stop().await;
}

#[tokio::test]
async fn protocol_rejects_bad_version_and_malformed_clients_without_stopping_listener() {
    use tokio::{io::AsyncWriteExt, net::windows::named_pipe::ClientOptions};
    let mut h = Harness::new();
    let mut pipe = ClientOptions::new()
        .open(pipe_name(&h.config.name).unwrap())
        .unwrap();
    write_message(
        &mut pipe,
        &Request {
            version: 99,
            id: 12,
            command: Command::Hello {
                subscription: Subscription::default(),
            },
        },
    )
    .await
    .unwrap();
    let response: ServerMessage = read_message(&mut pipe).await.unwrap();
    assert!(
        matches!(response.body, ServerBody::Error { id: 12, code, .. } if code == "invalid_version")
    );
    drop(pipe);
    let client = h
        .client(Subscription {
            receive: false,
            ..Default::default()
        })
        .await;
    let mut malformed = ClientOptions::new()
        .open(pipe_name(&h.config.name).unwrap())
        .unwrap();
    malformed
        .write_u32_le((MAX_MESSAGE + 1) as u32)
        .await
        .unwrap();
    drop(malformed);
    assert!(
        matches!(client.request(Command::Status).await.unwrap(), Reply::Status { status } if status.state == InterfaceState::Up)
    );
    h.stop().await;
}
