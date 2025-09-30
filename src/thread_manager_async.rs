use std::io::ErrorKind;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use tokio::sync::mpsc::{Receiver, Sender};

/// Blocking helper: create a [`NamedPipeServer`] and wait for a client
/// connection.
///
/// The `tokio` named pipe API returns immediately after creation, so we need
/// to explicitly wait for an inbound connection before handing the pipe to the
/// caller.  Keeping this logic in a helper keeps the reader and writer
/// routines focused on their respective loops.
async fn create_server_and_wait(pipe_name: &str) -> std::io::Result<NamedPipeServer> {
    // Creating the server does not block; this simply reserves the pipe name
    // until we await the connection below.
    let server = ServerOptions::new().create(pipe_name)?;

    // Wait until a client connects before returning the ready-to-use server.
    server.connect().await.map(|()| server)
}

/// Start the IPC reader, creating and waiting for pipe server connection without blocking async runtime
pub async fn start_ipc_reader(channel_name: String, tx: Sender<Vec<u8>>) -> std::io::Result<()> {
    let pipe_name = format!(r"\\.\pipe\can_{}_in", channel_name);

    loop {
        let server = create_server_and_wait(&pipe_name).await?;

        let mut reader = BufReader::new(server);
        loop {
            let mut line: Vec<u8> = vec![];
            let bytes_read = reader.read_buf(&mut line).await?;

            if bytes_read == 0 {
                println!("Pipe closed by client");
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

    loop {
        match rx.recv().await {
            Some(msg) => {
                if let Err(e) = server.write_all(&msg).await {
                    if e.kind() == ErrorKind::BrokenPipe {
                        println!("Client disconnected from IPC Writer");

                        // Restart the ipc_writer server and wait for another client to connect
                        server.shutdown().await?;
                        server = create_server_and_wait(&pipe_name).await?;
                    } else {
                        return Err(e);
                    }
                }

                // Explicitly flush to ensure the client sees the full frame
                // before we await the next message from the channel.
                server.flush().await?;
            }
            None => {
                // Channel closed: exit cleanly
                return Ok(());
            }
        }
    }
}
