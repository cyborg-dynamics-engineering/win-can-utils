//! Map each local UDP endpoint to an independent existing CAN broker session.
use super::{
    NetworkConfig, PacketDevice,
    ip::{self, Incoming, PacketError},
};
use crosscan::{protocol::*, service::ServiceClient};
use serde::Serialize;
use std::{
    collections::HashMap,
    future::Future,
    io,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{sync::mpsc, task::JoinSet, time::Instant};

#[derive(Clone)]
pub struct BridgeOptions {
    pub interface: String,
    pub network: NetworkConfig,
    pub idle_timeout: Duration,
    pub max_clients: usize,
}

impl Default for BridgeOptions {
    fn default() -> Self {
        Self {
            interface: "can0".into(),
            network: NetworkConfig::default(),
            idle_timeout: Duration::from_secs(60),
            max_clients: 32,
        }
    }
}

macro_rules! counters {
    ($($name:ident),+ $(,)?) => {
        #[derive(Default)] struct Stats { $($name: AtomicU64),+ }
        #[derive(Debug, Serialize)] pub struct BridgeReport { $(pub $name: u64),+ }
        impl Stats { fn report(&self) -> BridgeReport { BridgeReport { $($name: self.$name.load(Ordering::Relaxed)),+ } } }
    }
}
counters!(
    packets_from_windows,
    packets_to_windows,
    invalid_packets,
    ignored_packets,
    ingress_drops,
    egress_drops,
    ring_drops,
    protocol_errors,
    session_queue_drops,
    sessions_created,
    sessions_expired,
    active_clients,
    broker_local_drops,
    accepted_sends,
    icmp_replies
);

#[derive(Clone)]
struct Output {
    tx: mpsc::Sender<Vec<u8>>,
    network: NetworkConfig,
    stats: Arc<Stats>,
}

impl Output {
    fn send(&self, port: u16, body: ServerBody) {
        let message = ServerMessage {
            version: VERSION,
            body,
        };
        let Ok(bytes) = serde_json::to_vec(&message) else {
            self.stats.protocol_errors.fetch_add(1, Ordering::Relaxed);
            return;
        };
        let Ok(packet) = ip::udp_packet(
            self.network.peer,
            self.network.local,
            self.network.port,
            port,
            &bytes,
        ) else {
            self.stats.protocol_errors.fetch_add(1, Ordering::Relaxed);
            return;
        };
        self.packet(packet);
    }
    fn packet(&self, packet: Vec<u8>) {
        if self.tx.try_send(packet).is_err() {
            self.stats.egress_drops.fetch_add(1, Ordering::Relaxed);
        }
    }
    fn error(&self, port: u16, id: u64, code: &str, message: impl ToString) {
        self.send(
            port,
            ServerBody::Error {
                id,
                code: code.into(),
                message: message.to_string(),
            },
        );
    }
    fn broker_error(&self, port: u16, id: u64, error: io::Error) {
        let text = error.to_string();
        let (code, message) = text
            .split_once(": ")
            .filter(|(code, _)| {
                matches!(
                    *code,
                    "invalid_request"
                        | "invalid_version"
                        | "interface_down"
                        | "interface_up"
                        | "driver_error"
                        | "configuration_error"
                        | "unavailable"
                )
            })
            .unwrap_or(("broker_error", &text));
        self.error(port, id, code, message);
    }
}

struct ActiveClient(Arc<Stats>);
impl Drop for ActiveClient {
    fn drop(&mut self) {
        self.0.active_clients.fetch_sub(1, Ordering::Relaxed);
    }
}

struct StopReaderOnDrop(Arc<AtomicBool>);
impl Drop for StopReaderOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

async fn session(
    port: u16,
    first: Request,
    mut input: mpsc::Receiver<Request>,
    options: BridgeOptions,
    output: Output,
) {
    output.stats.active_clients.fetch_add(1, Ordering::Relaxed);
    let _active = ActiveClient(output.stats.clone());
    let Command::Hello { subscription } = first.command else {
        return;
    };
    let mut client = match ServiceClient::connect(&options.interface, subscription).await {
        Ok(client) => client,
        Err(error) => {
            output.broker_error(port, first.id, error);
            return;
        }
    };
    match client.request(Command::Status).await {
        Ok(Reply::Status { status }) => output.send(
            port,
            ServerBody::Reply {
                id: first.id,
                reply: Reply::Hello {
                    client_id: client.client_id,
                    status,
                },
            },
        ),
        Err(error) => {
            output.broker_error(port, first.id, error);
            return;
        }
        _ => return,
    }
    let mut last_activity = Instant::now();
    let mut last_drops = 0;
    loop {
        let local_drops = client.local_drops();
        output
            .stats
            .broker_local_drops
            .fetch_add(local_drops - last_drops, Ordering::Relaxed);
        last_drops = local_drops;
        tokio::select! {
            request = input.recv() => {
                let Some(request) = request else { break; };
                last_activity = Instant::now();
                let sending = matches!(request.command, Command::Send { .. });
                match client.request(request.command).await {
                    Ok(reply) => {
                        if sending { output.stats.accepted_sends.fetch_add(1, Ordering::Relaxed); }
                        output.send(port, ServerBody::Reply { id: request.id, reply });
                    }
                    Err(error) => output.broker_error(port, request.id, error),
                }
            }
            event = client.next_event() => {
                match event {
                    Ok(event) => output.send(port, ServerBody::Frame { event }),
                    Err(error) => { output.broker_error(port, 0, error); break; }
                }
            }
            _ = tokio::time::sleep_until(last_activity + options.idle_timeout) => {
                output.stats.sessions_expired.fetch_add(1, Ordering::Relaxed);
                break;
            }
        }
    }
}

/// Run until shutdown, a fatal adapter error, or adapter removal. All queues are bounded.
pub async fn run(
    device: Arc<dyn PacketDevice>,
    options: BridgeOptions,
    shutdown: impl Future<Output = ()>,
) -> io::Result<BridgeReport> {
    validate_name(&options.interface)?;
    options.network.validate()?;
    if options.max_clients == 0
        || options.max_clients > 128
        || options.idle_timeout < Duration::from_secs(1)
        || options.idle_timeout > Duration::from_secs(86_400)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Use 1..128 clients and an idle timeout of 1..86400 seconds",
        ));
    }
    let stats = Arc::new(Stats::default());
    let stopping = Arc::new(AtomicBool::new(false));
    let _stop_on_cancel = StopReaderOnDrop(stopping.clone());
    let (incoming_tx, mut incoming) = mpsc::channel(1024);
    let read_device = device.clone();
    let read_stopping = stopping.clone();
    let read_stats = stats.clone();
    let reader = tokio::task::spawn_blocking(move || {
        while !read_stopping.load(Ordering::Acquire) {
            if let Some(packet) = read_device.receive(Duration::from_millis(100))? {
                read_stats
                    .packets_from_windows
                    .fetch_add(1, Ordering::Relaxed);
                if incoming_tx.try_send(packet).is_err() {
                    read_stats.ingress_drops.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        Ok::<_, io::Error>(())
    });
    let (outgoing_tx, mut outgoing) = mpsc::channel(1024);
    let output = Output {
        tx: outgoing_tx,
        network: options.network.clone(),
        stats: stats.clone(),
    };
    let mut clients = HashMap::<u16, (u64, mpsc::Sender<Request>)>::new();
    let mut sessions = JoinSet::new();
    let mut generation = 0u64;
    tokio::pin!(shutdown);
    let result = loop {
        tokio::select! {
            _ = &mut shutdown => break Ok(()),
            packet = incoming.recv() => {
                let Some(packet) = packet else { break Err(io::Error::new(io::ErrorKind::BrokenPipe, "Adapter receiver stopped")); };
                match ip::parse(&packet, &options.network) {
                    Ok(Incoming::EchoReply(packet)) => { stats.icmp_replies.fetch_add(1, Ordering::Relaxed); output.packet(packet); }
                    Ok(Incoming::Datagram { source_port: port, payload }) => {
                        let request = match serde_json::from_slice::<Request>(&payload) {
                            Ok(request) => request,
                            Err(error) => { stats.protocol_errors.fetch_add(1, Ordering::Relaxed); clients.remove(&port); output.error(port, 0, "invalid_request", error); continue; }
                        };
                        if request.version != VERSION {
                            stats.protocol_errors.fetch_add(1, Ordering::Relaxed); clients.remove(&port);
                            output.error(port, request.id, "invalid_version", "Only CAN service version 1 is supported"); continue;
                        }
                        if clients.get(&port).is_some_and(|(_, tx)| tx.is_closed()) { clients.remove(&port); }
                        if let Some((_, tx)) = clients.get(&port) {
                            if tx.try_send(request).is_err() { stats.session_queue_drops.fetch_add(1, Ordering::Relaxed); }
                        } else {
                            if !matches!(request.command, Command::Hello { .. }) { output.error(port, request.id, "hello_required", "Start this UDP endpoint with hello"); continue; }
                            if clients.len() >= options.max_clients { output.error(port, request.id, "client_limit", "UDP client limit reached"); continue; }
                            generation += 1;
                            let current = generation;
                            let (tx, rx) = mpsc::channel(16);
                            clients.insert(port, (current, tx));
                            stats.sessions_created.fetch_add(1, Ordering::Relaxed);
                            let task_options = options.clone(); let task_output = output.clone();
                            sessions.spawn(async move { session(port, request, rx, task_options, task_output).await; (port, current) });
                        }
                    }
                    Err(PacketError::Malformed(_)) => { stats.invalid_packets.fetch_add(1, Ordering::Relaxed); }
                    Err(PacketError::Foreign | PacketError::Unsupported(_)) => { stats.ignored_packets.fetch_add(1, Ordering::Relaxed); }
                }
            }
            packet = outgoing.recv() => {
                if let Some(packet) = packet {
                    match device.send(&packet) {
                        Ok(()) => { stats.packets_to_windows.fetch_add(1, Ordering::Relaxed); }
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => { stats.ring_drops.fetch_add(1, Ordering::Relaxed); }
                        Err(error) => break Err(error),
                    }
                }
            }
            complete = sessions.join_next(), if !sessions.is_empty() => {
                if let Some(Ok((port, current))) = complete {
                    if clients.get(&port).is_some_and(|(generation, _)| *generation == current) { clients.remove(&port); }
                }
            }
        }
    };
    stopping.store(true, Ordering::Release);
    drop(incoming);
    sessions.abort_all();
    while sessions.join_next().await.is_some() {}
    let read_result = reader.await.map_err(io::Error::other)?;
    read_result?;
    result?;
    Ok(stats.report())
}
