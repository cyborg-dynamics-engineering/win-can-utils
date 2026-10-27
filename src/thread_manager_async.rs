use crosscan::can::CanFrame;
use serde::{Deserialize, Serialize};
use std::io;
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
pub async fn start_ipc_reader(channel_name: String, tx: Sender<CanFrame>) -> std::io::Result<()> {
    let pipe_name = format!(r"\\.\pipe\can_{}_in", channel_name);

    loop {
        let server = create_server_and_wait(&pipe_name).await?;
        println!("Client connected to IPC Reader");

        let mut reader = BufReader::new(server);
        let mut decoder = IpcFrameDecoder::default();
        loop {
            let mut line: Vec<u8> = vec![];
            let bytes_read = reader.read_buf(&mut line).await?;

            if bytes_read == 0 {
                println!("Client disconnected from IPC Reader");
                break;
            }
            let frames = match decoder.push(&line) {
                Ok(frames) => frames,
                Err(error) => {
                    eprintln!("Invalid CAN pipe message: {error}");
                    break;
                }
            };
            for frame in frames {
                if tx.send(frame).await.is_err() {
                    return Ok(());
                }
            }
        }
    }
}

/// Decode the existing bincode-frame + newline protocol across arbitrary pipe
/// read boundaries. Binary CAN data can itself contain newline bytes.
#[derive(Default)]
struct IpcFrameDecoder {
    buffer: Vec<u8>,
}

impl IpcFrameDecoder {
    fn push(&mut self, bytes: &[u8]) -> io::Result<Vec<CanFrame>> {
        self.buffer.extend_from_slice(bytes);
        let mut frames = Vec::new();
        let mut offset = 0;
        while offset < self.buffer.len() {
            let (frame, consumed) = match bincode::serde::decode_from_slice::<CanFrame, _>(
                &self.buffer[offset..],
                bincode::config::standard(),
            ) {
                Ok(frame) => frame,
                Err(bincode::error::DecodeError::UnexpectedEnd { .. }) => break,
                Err(error) => return Err(io::Error::new(io::ErrorKind::InvalidData, error)),
            };
            if offset + consumed == self.buffer.len() {
                break; // The delimiter may arrive in a later read.
            }
            if self.buffer[offset + consumed] != b'\n' || frame.dlc() > 8 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Invalid CAN frame or delimiter",
                ));
            }
            offset += consumed + 1;
            frames.push(frame);
        }
        self.buffer.drain(..offset);
        if self.buffer.len() > 256 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Incomplete CAN pipe message exceeds 256 bytes",
            ));
        }
        Ok(frames)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode(frame: &CanFrame) -> Vec<u8> {
        let mut bytes = bincode::serde::encode_to_vec(frame, bincode::config::standard()).unwrap();
        bytes.push(b'\n');
        bytes
    }

    #[test]
    fn coalesced_pipe_messages_preserve_all_frames() {
        let expected: Vec<_> = (0..4096u32)
            .map(|seq| CanFrame::new(0x123, &seq.to_le_bytes()).unwrap())
            .collect();
        let bytes: Vec<_> = expected.iter().flat_map(encode).collect();
        assert_eq!(IpcFrameDecoder::default().push(&bytes).unwrap(), expected);
    }

    #[test]
    fn split_messages_and_binary_newlines_are_preserved() {
        let expected = vec![
            CanFrame::new(0x123, &[10, 0, 10, 255]).unwrap(),
            CanFrame::new_remote(0x456, 8, false).unwrap(),
        ];
        let bytes: Vec<_> = expected.iter().flat_map(encode).collect();
        let mut decoder = IpcFrameDecoder::default();
        let actual: Vec<_> = bytes
            .into_iter()
            .flat_map(|byte| decoder.push(&[byte]).unwrap())
            .collect();
        assert_eq!(actual, expected);
        assert!(decoder.buffer.is_empty());
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
    pub version: String,
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
