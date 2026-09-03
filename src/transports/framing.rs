//! Incremental framing for the Avocado byte stream.
//!
//! Serial reads are allowed to split a packet at any byte, or return several
//! packets at once. This decoder keeps the incomplete tail and validates the
//! wrapper, declared payload length, and checksum before handing a packet to
//! the protocol parser.

use std::io::Cursor;

use thiserror::Error;

use crate::{
    protocol::{AvocadoPacket, ProtocolError},
    transports::MAX_DATA_SIZE,
};

const WRAPPER: u8 = 0x7e;
const HEADER_SIZE: usize = 20;
const TRAILER_SIZE: usize = 2;

#[derive(Debug, Error)]
pub enum FrameError {
    #[error("serial frame payload is too large: {0} bytes")]
    PayloadTooLarge(usize),
    #[error("serial frame has an invalid suffix")]
    InvalidSuffix,
    #[error("serial frame checksum mismatch")]
    InvalidChecksum,
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
}

#[derive(Debug, Default)]
pub struct FrameDecoder {
    buffer: Vec<u8>,
}

impl FrameDecoder {
    pub fn push(&mut self, bytes: &[u8]) -> Vec<Result<AvocadoPacket, FrameError>> {
        self.buffer.extend_from_slice(bytes);
        let mut packets = Vec::new();

        loop {
            let Some(prefix) = self.buffer.iter().position(|byte| *byte == WRAPPER) else {
                // iAP2 and other traffic can precede an Avocado session. Do not
                // retain an unbounded amount of unrelated serial data.
                self.buffer.clear();
                break;
            };
            if prefix > 0 {
                self.buffer.drain(..prefix);
            }

            if self.buffer.len() < HEADER_SIZE {
                break;
            }

            let flags = u16::from_le_bytes([self.buffer[18], self.buffer[19]]);
            let payload_len = usize::from(flags & 0x03ff);
            if payload_len > MAX_DATA_SIZE {
                packets.push(Err(FrameError::PayloadTooLarge(payload_len)));
                self.buffer.drain(..1);
                continue;
            }

            let frame_len = HEADER_SIZE + payload_len + TRAILER_SIZE;
            if self.buffer.len() < frame_len {
                break;
            }

            if self.buffer[frame_len - 1] != WRAPPER {
                packets.push(Err(FrameError::InvalidSuffix));
                self.buffer.drain(..1);
                continue;
            }

            let expected_checksum = self.buffer[1..frame_len - 2]
                .iter()
                .fold(0u8, |sum, byte| sum.wrapping_add(*byte));
            if self.buffer[frame_len - 2] != expected_checksum {
                packets.push(Err(FrameError::InvalidChecksum));
                self.buffer.drain(..frame_len);
                continue;
            }

            let frame: Vec<u8> = self.buffer.drain(..frame_len).collect();
            packets
                .push(AvocadoPacket::read_one(&mut Cursor::new(frame)).map_err(FrameError::from));
        }

        packets
    }
}

#[cfg(test)]
mod tests {
    use crate::protocol::{ContentType, EncodingType, EncryptionMode, InteractionType};

    use super::*;

    fn packet(id: u32, data: &[u8]) -> AvocadoPacket {
        AvocadoPacket {
            version: 100,
            content_type: ContentType::Message,
            interaction_type: InteractionType::Response,
            encoding_type: EncodingType::Json,
            encryption_mode: EncryptionMode::None,
            terminal_id: id,
            msg_number: id,
            msg_package_total: 1,
            msg_package_num: 1,
            is_subpackage: false,
            data: data.to_vec(),
        }
    }

    #[test]
    fn reassembles_split_frame() {
        let encoded = packet(7, br#"{"id":7}"#).encode();
        let mut decoder = FrameDecoder::default();

        assert!(decoder.push(&encoded[..5]).is_empty());
        assert!(decoder.push(&encoded[5..21]).is_empty());
        let decoded = decoder.push(&encoded[21..]);

        assert_eq!(decoded.len(), 1);
        let decoded = decoded.into_iter().next().unwrap().unwrap();
        assert_eq!(decoded.msg_number, 7);
        assert_eq!(decoded.data, br#"{"id":7}"#);
    }

    #[test]
    fn emits_every_complete_frame_from_one_read() {
        let mut bytes = packet(1, b"one").encode();
        bytes.extend(packet(2, b"two").encode());

        let decoded = FrameDecoder::default().push(&bytes);

        assert_eq!(decoded.len(), 2);
        assert_eq!(decoded[0].as_ref().unwrap().msg_number, 1);
        assert_eq!(decoded[1].as_ref().unwrap().msg_number, 2);
    }

    #[test]
    fn skips_noise_and_recovers_after_bad_checksum() {
        let mut bad = packet(1, b"bad").encode();
        let checksum = bad.len() - 2;
        bad[checksum] = bad[checksum].wrapping_add(1);
        let good = packet(2, b"good").encode();

        let mut bytes = vec![0xff, 0x55, 0x01];
        bytes.extend(bad);
        bytes.extend(good);
        let decoded = FrameDecoder::default().push(&bytes);

        assert!(matches!(decoded[0], Err(FrameError::InvalidChecksum)));
        assert_eq!(decoded[1].as_ref().unwrap().msg_number, 2);
    }
}
