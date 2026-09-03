use anyhow::{Context, bail};

pub const PIXCUT_USB_VID: u16 = 0x302c;
pub const PIXCUT_USB_PID: u16 = 0x3101;
pub const COMMAND_INTERFACE: u8 = 2;
pub const COMMAND_OUT_ENDPOINT: u8 = 0x06;
pub const COMMAND_IN_ENDPOINT: u8 = 0x86;
pub const DATA_INTERFACE: u8 = 3;
pub const DATA_OUT_ENDPOINT: u8 = 0x04;
pub const DATA_IN_ENDPOINT: u8 = 0x84;
pub const MAX_DATA_PAYLOAD: usize = 10_215;
pub const USB_WRITE_SLICE: usize = 1_024;

/// Frame a compact RPC command for the PixCut native USB command interface.
pub fn encode_json_command(value: &serde_json::Value) -> anyhow::Result<Vec<u8>> {
    let mut result = b"cmd json\n".to_vec();
    serde_json::to_writer(&mut result, value).context("could not encode USB JSON command")?;
    Ok(result)
}

/// Frame one native USB job-data part. The observed EXTLEN value counts the
/// payload bytes only; the little-endian job id immediately follows the line.
pub fn encode_data_frame(job_id: u32, payload: &[u8]) -> anyhow::Result<Vec<u8>> {
    if payload.len() > MAX_DATA_PAYLOAD {
        bail!(
            "USB data payload is {} bytes; maximum is {}",
            payload.len(),
            MAX_DATA_PAYLOAD
        );
    }
    let mut result = format!("cmd data EXTLEN={}\n", payload.len()).into_bytes();
    result.extend_from_slice(&job_id.to_le_bytes());
    result.extend_from_slice(payload);
    Ok(result)
}

pub fn encode_data_frames(job_id: u32, payload: &[u8]) -> Vec<Vec<u8>> {
    payload
        .chunks(MAX_DATA_PAYLOAD)
        .map(|part| encode_data_frame(job_id, part).expect("bounded chunk"))
        .collect()
}

/// Split a logical frame into the 1024-byte writes used on the native USB
/// command/data endpoints without losing or padding bytes.
pub fn write_slices(frame: &[u8]) -> impl Iterator<Item = &[u8]> {
    frame.chunks(USB_WRITE_SLICE)
}

/// Stateful extraction of balanced JSON objects from native USB reads. Some
/// firmware responses include NUL/CR padding; those bytes are ignored outside
/// quoted strings.
#[derive(Default, Debug)]
pub struct JsonResponseDecoder {
    buffer: Vec<u8>,
}

impl JsonResponseDecoder {
    pub fn push(&mut self, bytes: &[u8]) -> Vec<anyhow::Result<serde_json::Value>> {
        self.buffer.extend_from_slice(bytes);
        let mut output = Vec::new();
        loop {
            let Some(start) = self.buffer.iter().position(|byte| *byte == b'{') else {
                self.buffer.clear();
                break;
            };
            if start > 0 {
                self.buffer.drain(..start);
            }
            let Some(end) = balanced_object_end(&self.buffer) else {
                break;
            };
            let raw = self.buffer.drain(..end).collect::<Vec<_>>();
            let cleaned = raw
                .into_iter()
                .filter(|byte| !matches!(byte, 0 | b'\r'))
                .collect::<Vec<_>>();
            output.push(
                serde_json::from_slice(&cleaned).context("invalid JSON from native USB device"),
            );
        }
        output
    }
}

fn balanced_object_end(bytes: &[u8]) -> Option<usize> {
    let mut depth = 0usize;
    let mut quoted = false;
    let mut escaped = false;
    for (index, byte) in bytes.iter().copied().enumerate() {
        if quoted {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                quoted = false;
            }
            continue;
        }
        match byte {
            b'"' => quoted = true,
            b'{' => depth += 1,
            b'}' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(index + 1);
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_commands_are_compact_and_prefixed() {
        let value = serde_json::json!({"method":"get-prop","params":["printer-state"],"id":7});
        let bytes = encode_json_command(&value).unwrap();
        assert_eq!(
            bytes,
            br#"cmd json
{"id":7,"method":"get-prop","params":["printer-state"]}"#
        );
    }

    #[test]
    fn data_frames_use_observed_payload_limit_and_job_id() {
        let payload = vec![0x55; MAX_DATA_PAYLOAD + 3];
        let frames = encode_data_frames(0x1234_5678, &payload);
        assert_eq!(frames.len(), 2);
        assert!(frames[0].starts_with(b"cmd data EXTLEN=10215\n\x78\x56\x34\x12"));
        assert!(frames[1].starts_with(b"cmd data EXTLEN=3\n\x78\x56\x34\x12"));
        assert_eq!(frames[1].len(), b"cmd data EXTLEN=3\n".len() + 4 + 3);
        assert!(encode_data_frame(1, &vec![0; MAX_DATA_PAYLOAD + 1]).is_err());
    }

    #[test]
    fn usb_slices_reassemble_without_padding() {
        let frame = vec![9; USB_WRITE_SLICE * 2 + 5];
        let slices = write_slices(&frame).collect::<Vec<_>>();
        assert_eq!(
            slices.iter().map(|part| part.len()).collect::<Vec<_>>(),
            [1024, 1024, 5]
        );
        assert_eq!(slices.concat(), frame);
    }

    #[test]
    fn response_decoder_handles_split_coalesced_padded_and_nested_json() {
        let mut decoder = JsonResponseDecoder::default();
        assert!(decoder.push(b"noise\0{\"id\":1,\"result\":{").is_empty());
        let values = decoder.push(b"\"text\":\"} escaped \\\" {\"}}\r\0{\"id\":2}");
        assert_eq!(values.len(), 2);
        assert_eq!(values[0].as_ref().unwrap()["id"], 1);
        assert_eq!(values[1].as_ref().unwrap()["id"], 2);
    }
}
