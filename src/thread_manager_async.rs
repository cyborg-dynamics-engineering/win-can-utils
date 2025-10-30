use serde::{Deserialize, Serialize};
use std::io::ErrorKind;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use tokio::sync::mpsc::{Receiver, Sender};

/// Blocking helper: create a [`NamedPipeServer`] and wait for a client
async fn create_server_and_wait(pipe_name: &str) -> std::io::Result<NamedPipeServer> {
    let server = ServerOptions::new().create(pipe_name)?;
    server.connect().await?;
    Ok(server)
}

/// Start the IPC reader
pub async fn start_ipc_reader(channel_name: String, tx: Sender<Vec<u8>>) -> std::io::Result<()> {
    let pipe_name = format!(r"\\.\pipe\can_{}_in", channel_name);

    loop {
        let server = create_server_and_wait(&pipe_name).await?;
        println!("Client connected to IPC Reader");

        let mut reader = BufReader::new(server);
        loop {
            let mut line: Vec<u8> = vec![];
            let bytes_read = reader.read_buf(&mut line).await?;

            if bytes_read == 0 {
                println!("Client disconnected from IPC Reader");
                break;
            }
            if tx.send(line).await.is_err() {
                println!("Receiver closed");
                break;
            }
        }
    }
}

pub async fn start_ipc_writer(
    channel_name: String,
    mut rx: Receiver<Vec<u8>>,
) -> std::io::Result<()> {
    let pipe_name = format!(r"\\.\pipe\can_{}_out", channel_name);

    let mut server = create_server_and_wait(&pipe_name).await?;
    println!("Client connected to IPC Writer");

    loop {
        match rx.recv().await {
            Some(msg) => {
                if let Err(e) = server.write_all(&msg).await {
                    if e.kind() == ErrorKind::BrokenPipe {
                        println!("Client disconnected from IPC Writer");

                        server.shutdown().await?;
                        server = create_server_and_wait(&pipe_name).await?;
                        println!("Client connected to IPC Writer");
                    } else {
                        return Err(e);
                    }
                }
                server.flush().await?;
            }
            None => {
                println!("Writer channel closed, exiting IPC Writer.");
                return Ok(());
            }
        }
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub struct CanServerConfig {
    pub bitrate: Option<u32>,
}

pub async fn start_ipc_config_handler(
    channel_name: String,
    config: CanServerConfig,
) -> std::io::Result<()> {
    let pipe_name = format!(r"\\.\pipe\can_{}_config_out", channel_name);
    loop {
        let mut server = create_server_and_wait(&pipe_name).await?;

        let data = serde_json::to_vec(&config)?;

        if data.len() > (u8::MAX as usize) {
            return Err(std::io::Error::new(
                ErrorKind::InvalidData,
                format!("Serialized Config is too large: {}", data.len()).to_string(),
            ));
        }

        server.write_all(&data).await?;
        server.flush().await?;
        server.shutdown().await?;
    }
}
