//! Local-only named-pipe listeners for v1 and the legacy crosscan protocol.
use crate::{
    broker::Broker,
    service::ServiceHandle,
    thread_manager_async::{CanServerConfig, IpcFrameDecoder},
};
use crosscan::protocol::*;
use std::{io, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::windows::named_pipe::{NamedPipeServer, ServerOptions},
    sync::mpsc,
    task::JoinSet,
};

#[derive(Clone, Copy)]
enum Kind {
    V1,
    LegacyIn,
    LegacyOut,
    LegacyConfig,
}

fn create(path: &str, first: bool) -> io::Result<NamedPipeServer> {
    ServerOptions::new()
        .first_pipe_instance(first)
        .reject_remote_clients(true)
        .create(path)
}

/// Bind every endpoint before spawning anything, so startup errors are visible.
pub fn bind(name: &str, service: ServiceHandle) -> io::Result<JoinSet<io::Result<()>>> {
    validate_name(name)?;
    let mut bound = vec![];
    for (path, kind) in [
        (pipe_name(name)?, Kind::V1),
        (format!(r"\\.\pipe\can_{name}_in"), Kind::LegacyIn),
        (format!(r"\\.\pipe\can_{name}_out"), Kind::LegacyOut),
        (
            format!(r"\\.\pipe\can_{name}_config_out"),
            Kind::LegacyConfig,
        ),
    ] {
        bound.push((create(&path, true)?, path, kind));
    }
    let mut tasks = JoinSet::new();
    for (pipe, path, kind) in bound {
        tasks.spawn(listen(pipe, path, kind, service.clone()));
    }
    Ok(tasks)
}

async fn listen(
    mut pipe: NamedPipeServer,
    path: String,
    kind: Kind,
    service: ServiceHandle,
) -> io::Result<()> {
    let mut sessions = JoinSet::new();
    loop {
        tokio::select! {
            result = pipe.connect() => {
                result?;
                let next = create(&path, false)?;
                let connected = std::mem::replace(&mut pipe, next);
                if sessions.len() >= crate::broker::MAX_CLIENTS { drop(connected); continue; }
                let service = service.clone();
                sessions.spawn(async move {
                    let result = match kind {
                        Kind::V1 => v1(connected, service).await,
                        Kind::LegacyIn => legacy_in(connected, service).await,
                        Kind::LegacyOut => start_ipc_writer(connected, service).await,
                        Kind::LegacyConfig => legacy_config(connected, service).await,
                    };
                    if let Err(error) = result {
                        if !matches!(error.kind(), io::ErrorKind::BrokenPipe | io::ErrorKind::UnexpectedEof | io::ErrorKind::ConnectionReset) { log::debug!("CAN client disconnected: {error}"); }
                    }
                });
            }
            _ = sessions.join_next(), if !sessions.is_empty() => {},
        }
    }
}

struct AbortReader(tokio::task::JoinHandle<()>);
impl Drop for AbortReader {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn send_reply<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    id: u64,
    result: Result<Reply, crate::service::ServiceError>,
) -> io::Result<()> {
    let body = match result {
        Ok(reply) => ServerBody::Reply { id, reply },
        Err(error) => ServerBody::Error {
            id,
            code: error.code.into(),
            message: error.message,
        },
    };
    write_message(
        writer,
        &ServerMessage {
            version: VERSION,
            body,
        },
    )
    .await
}

async fn v1(mut pipe: NamedPipeServer, service: ServiceHandle) -> io::Result<()> {
    let hello: Request = tokio::time::timeout(Duration::from_secs(10), read_message(&mut pipe))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "Hello deadline exceeded"))??;
    let subscription = match hello.command {
        Command::Hello { subscription } if hello.version == VERSION => subscription,
        _ => {
            return send_reply(
                &mut pipe,
                hello.id,
                Err(crate::service::ServiceError {
                    code: if hello.version != VERSION {
                        "invalid_version"
                    } else {
                        "invalid_request"
                    },
                    message: "First request must be hello with version 1".into(),
                }),
            )
            .await;
        }
    };
    let (lease, mut frames) = match Broker::subscribe(&service.broker, subscription) {
        Ok(client) => client,
        Err(error) => return send_reply(&mut pipe, hello.id, Err(error.into())).await,
    };
    let status = match service.request(lease.id, Command::Status).await {
        Ok(Reply::Status { status }) => status,
        _ => return Err(io::Error::other("Interface unavailable")),
    };
    send_reply(
        &mut pipe,
        hello.id,
        Ok(Reply::Hello {
            client_id: lease.id,
            status,
        }),
    )
    .await?;
    let (mut reader, mut writer) = tokio::io::split(pipe);
    let (tx, mut requests) = mpsc::channel(8);
    // A persistent reader owns framing; cancelling a select cannot lose half a packet.
    let _reader = AbortReader(tokio::spawn(async move {
        loop {
            let request = read_message::<_, Request>(&mut reader).await;
            let failed = request.is_err();
            if tx.send(request).await.is_err() || failed {
                break;
            }
        }
    }));
    loop {
        tokio::select! {
            request = requests.recv() => {
                let Some(request) = request else { return Ok(()); };
                let request = request?;
                if request.version != VERSION {
                    send_reply(&mut writer, request.id, Err(crate::service::ServiceError { code: "invalid_version", message: "Only version 1 is supported".into() })).await?;
                    return Ok(());
                }
                let result = service.request(lease.id, request.command).await;
                send_reply(&mut writer, request.id, result).await?;
            }
            event = frames.recv() => {
                let Some(event) = event else { return Ok(()); };
                write_message(&mut writer, &ServerMessage { version: VERSION, body: ServerBody::Frame { event } }).await?;
                service.broker.lock().unwrap().delivered(lease.id);
            }
        }
    }
}

async fn legacy_in(mut pipe: NamedPipeServer, service: ServiceHandle) -> io::Result<()> {
    let (lease, _frames) = Broker::subscribe(
        &service.broker,
        Subscription {
            receive: false,
            ..Default::default()
        },
    )?;
    let mut decoder = IpcFrameDecoder::default();
    let mut bytes = [0u8; 8192];
    loop {
        let count = pipe.read(&mut bytes).await?;
        if count == 0 {
            return Ok(());
        }
        for frame in decoder.push(&bytes[..count])? {
            service
                .request(
                    lease.id,
                    Command::Send {
                        frame: Frame::from(&frame),
                    },
                )
                .await
                .map_err(|e| io::Error::other(e.message))?;
        }
    }
}

/// Each legacy reader has a separate bounded queue, just like a v1 subscriber.
pub async fn start_ipc_writer(mut pipe: NamedPipeServer, service: ServiceHandle) -> io::Result<()> {
    let (lease, mut frames) = Broker::subscribe(
        &service.broker,
        Subscription {
            error_mask: 0x1fff_ffff,
            ..Default::default()
        },
    )?;
    let mut disconnected = [0u8; 1];
    loop {
        tokio::select! {
            _ = pipe.read(&mut disconnected) => return Ok(()),
            event = frames.recv() => {
                let Some(event) = event else { return Ok(()); };
                let mut frame = event.frame.to_can()?;
                frame.set_timestamp(event.hardware_timestamp_us.or(Some(event.timestamp_us)));
                let bytes = bincode::serde::encode_to_vec(frame, bincode::config::standard()).map_err(io::Error::other)?;
                if bytes.len() > 255 { return Err(io::Error::other("Legacy CAN frame too large")); }
                pipe.write_u8(bytes.len() as u8).await?;
                pipe.write_all(&bytes).await?;
                service.broker.lock().unwrap().delivered(lease.id);
            }
        }
    }
}

async fn legacy_config(mut pipe: NamedPipeServer, service: ServiceHandle) -> io::Result<()> {
    let status = service.broker.lock().unwrap().status();
    let config = CanServerConfig {
        bitrate: Some(status.bitrate),
        version: env!("CARGO_PKG_VERSION").into(),
    };
    pipe.write_all(&serde_json::to_vec(&config).map_err(io::Error::other)?)
        .await
}
