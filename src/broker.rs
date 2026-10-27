//! Per-subscriber queues. Publishing never awaits a reader or holds a driver lock.
use crosscan::protocol::*;
use std::{
    collections::BTreeMap,
    io,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::sync::mpsc;

pub const MAX_CLIENTS: usize = 128;

struct Subscriber {
    options: Subscription,
    tx: mpsc::Sender<FrameEvent>,
    stats: ClientStats,
}

pub struct Broker {
    pub status: Status,
    clients: BTreeMap<u64, Subscriber>,
    next_client: u64,
    sequence: u64,
}

pub type SharedBroker = Arc<Mutex<Broker>>;

pub struct ClientLease {
    pub id: u64,
    broker: SharedBroker,
}
impl Drop for ClientLease {
    fn drop(&mut self) {
        self.broker.lock().unwrap().clients.remove(&self.id);
    }
}

impl Broker {
    pub fn new(status: Status) -> SharedBroker {
        Arc::new(Mutex::new(Self {
            status,
            clients: BTreeMap::new(),
            next_client: 0,
            sequence: 0,
        }))
    }

    pub fn subscribe(
        broker: &SharedBroker,
        options: Subscription,
    ) -> io::Result<(ClientLease, mpsc::Receiver<FrameEvent>)> {
        options.validate()?;
        let mut state = broker.lock().unwrap();
        if state.clients.len() >= MAX_CLIENTS {
            return Err(io::Error::other("Interface client limit reached"));
        }
        state.next_client = state
            .next_client
            .checked_add(1)
            .ok_or_else(|| io::Error::other("Client ID exhausted"))?;
        let id = state.next_client;
        let (tx, rx) = mpsc::channel(options.queue_capacity);
        state.clients.insert(
            id,
            Subscriber {
                stats: ClientStats {
                    client_id: id,
                    queue_capacity: options.queue_capacity,
                    queued: 0,
                    delivered: 0,
                    dropped: 0,
                    filtered: 0,
                },
                options,
                tx,
            },
        );
        Ok((
            ClientLease {
                id,
                broker: broker.clone(),
            },
            rx,
        ))
    }

    pub fn update(&mut self, id: u64, options: Subscription) -> io::Result<()> {
        options.validate()?;
        let client = self
            .clients
            .get_mut(&id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "Client disconnected"))?;
        if options.queue_capacity != client.options.queue_capacity {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Reconnect to change queue capacity",
            ));
        }
        client.options = options;
        Ok(())
    }

    pub fn status(&self) -> Status {
        let mut status = self.status.clone();
        status.active_clients = self.clients.len();
        status
    }

    pub fn clients(&self) -> Vec<ClientStats> {
        self.clients
            .values()
            .map(|client| {
                let mut stats = client.stats.clone();
                stats.queued = client.tx.max_capacity() - client.tx.capacity();
                stats
            })
            .collect()
    }

    pub fn delivered(&mut self, id: u64) {
        if let Some(client) = self.clients.get_mut(&id) {
            client.stats.delivered += 1;
        }
    }

    pub fn set_bus_state(&mut self, state: BusState) {
        if state == BusState::BusOff && self.status.bus_state != BusState::BusOff {
            self.status.counters.bus_off_events += 1;
        }
        self.status.bus_state = state;
    }

    pub fn publish(&mut self, frame: &crosscan::can::CanFrame, source: Option<u64>) {
        self.sequence += 1;
        let timestamp_us = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros() as u64;
        let event = FrameEvent {
            sequence: self.sequence,
            frame: Frame::from(frame),
            direction: if source.is_some() {
                Direction::LocalTx
            } else {
                Direction::Rx
            },
            source_client_id: source,
            timestamp_us,
            hardware_timestamp_us: if source.is_none() {
                frame.timestamp()
            } else {
                None
            },
        };
        if source.is_none() {
            self.status.counters.rx_frames += 1;
            self.status.last_rx_timestamp_us = Some(timestamp_us);
            if frame.is_error() {
                self.status.counters.rx_errors += 1;
                if frame.id() & 0x40 != 0 {
                    self.set_bus_state(BusState::BusOff);
                } else if frame.id() & 0x100 != 0 {
                    self.set_bus_state(BusState::ErrorActive);
                }
            }
        }
        for (&id, client) in &mut self.clients {
            if !client.options.matches(&event.frame)
                || (source == Some(id) && !client.options.receive_own_messages)
            {
                client.stats.filtered += 1;
                continue;
            }
            match client.tx.try_send(event.clone()) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_)) => {
                    client.stats.dropped += 1;
                    self.status.counters.client_drops += 1;
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {}
            }
        }
    }
}
