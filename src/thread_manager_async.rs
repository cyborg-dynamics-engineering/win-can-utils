//! Legacy wire decoder. Pipe sessions are served by `crate::ipc`.
use crosscan::can::CanFrame;
use serde::{Deserialize, Serialize};
use std::io;

/// Decode the existing bincode-frame + newline protocol across arbitrary pipe
/// read boundaries. Binary CAN data can itself contain newline bytes.
#[derive(Default)]
pub(crate) struct IpcFrameDecoder {
    buffer: Vec<u8>,
}

impl IpcFrameDecoder {
    pub(crate) fn push(&mut self, bytes: &[u8]) -> io::Result<Vec<CanFrame>> {
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

#[derive(Serialize, Deserialize, Debug)]
pub struct CanServerConfig {
    pub bitrate: Option<u32>,
    pub version: String,
}
