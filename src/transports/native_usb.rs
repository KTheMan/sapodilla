use std::{borrow::Cow, collections::VecDeque, sync::mpsc as std_mpsc, thread, time::Duration};

use anyhow::{Context, bail};
use async_trait::async_trait;
use futures::channel::{mpsc, oneshot};
use nusb::{
    MaybeFuture,
    transfer::{Buffer, Bulk, In, Out},
};
use tracing::{debug, info, trace};

use crate::{
    protocol::{AvocadoPacket, ContentType, EncodingType, EncryptionMode, InteractionType},
    raw_usb::{
        COMMAND_IN_ENDPOINT, COMMAND_INTERFACE, COMMAND_OUT_ENDPOINT, DATA_IN_ENDPOINT,
        DATA_INTERFACE, DATA_OUT_ENDPOINT, JsonResponseDecoder, PIXCUT_USB_PID, PIXCUT_USB_VID,
        encode_data_frame, write_slices,
    },
    transports::{DiscoveredDevice, TransportControl, TransportEvent, TransportStatus},
};

const WRITE_TIMEOUT: Duration = Duration::from_secs(20);
const READ_TIMEOUT: Duration = Duration::from_secs(15);
const RESPONSE_BUFFER_SIZE: usize = 16 * 1024;

#[derive(Debug)]
enum TransportAction {
    SendPacket(AvocadoPacket, oneshot::Sender<()>),
    Disconnect,
}

/// Native WinUSB/libusb-style transport for the PixCut vendor bulk
/// interfaces. On Windows those interfaces must be associated with WinUSB.
#[derive(Default)]
pub struct NativeUsbTransport {
    selected_device: Option<String>,
    tx: Option<std_mpsc::Sender<TransportAction>>,
}

#[async_trait]
impl TransportControl for NativeUsbTransport {
    fn name(&self) -> Cow<'static, str> {
        "Native USB (bulk)".into()
    }

    fn supports_discovery(&self) -> bool {
        true
    }

    async fn discover_devices(&mut self) -> anyhow::Result<Vec<DiscoveredDevice>> {
        Ok(pixcut_devices()?
            .map(|device| {
                let id = device_key(&device);
                let name = device
                    .product_string()
                    .unwrap_or("Liene PixCut S1")
                    .to_owned();
                let mut details = vec![format!("USB {PIXCUT_USB_VID:04X}:{PIXCUT_USB_PID:04X}")];
                if let Some(manufacturer) = device.manufacturer_string() {
                    details.push(manufacturer.to_owned());
                }
                if let Some(serial) = device.serial_number() {
                    details.push(format!("S/N {serial}"));
                }
                DiscoveredDevice {
                    id,
                    name,
                    details: Some(details.join(" · ")),
                }
            })
            .collect())
    }

    fn select_device(&mut self, id: &str) -> anyhow::Result<()> {
        if self.tx.is_some() {
            bail!("cannot change USB device while connected");
        }
        if id.trim().is_empty() {
            bail!("USB device identifier cannot be empty");
        }
        self.selected_device = Some(id.to_owned());
        Ok(())
    }

    async fn start(
        &mut self,
        event_tx: mpsc::UnboundedSender<TransportEvent>,
    ) -> anyhow::Result<()> {
        if self.tx.is_some() {
            bail!("native USB transport is already started");
        }
        let selected = self
            .selected_device
            .clone()
            .context("select a native USB device before connecting")?;
        let _ =
            event_tx.unbounded_send(TransportEvent::TransportStatus(TransportStatus::Connecting));

        let info = pixcut_devices()?
            .find(|device| device_key(device) == selected)
            .with_context(|| format!("selected USB device {selected} is no longer connected"))?;
        let device = info
            .open()
            .wait()
            .context("could not open PixCut USB device")?;
        let command = device
            .detach_and_claim_interface(COMMAND_INTERFACE)
            .wait()
            .context("could not claim PixCut command interface 2")?;
        let data = device
            .detach_and_claim_interface(DATA_INTERFACE)
            .wait()
            .context("could not claim PixCut data interface 3")?;

        let command_out = command
            .endpoint::<Bulk, Out>(COMMAND_OUT_ENDPOINT)
            .context("missing PixCut command OUT endpoint 0x06")?;
        let command_in = command
            .endpoint::<Bulk, In>(COMMAND_IN_ENDPOINT)
            .context("missing PixCut command IN endpoint 0x86")?;
        let data_out = data
            .endpoint::<Bulk, Out>(DATA_OUT_ENDPOINT)
            .context("missing PixCut data OUT endpoint 0x04")?;
        let data_in = data
            .endpoint::<Bulk, In>(DATA_IN_ENDPOINT)
            .context("missing PixCut data IN endpoint 0x84")?;

        let (action_tx, action_rx) = std_mpsc::channel();
        thread::Builder::new()
            .name("sapodilla-usb-bulk".into())
            .spawn(move || {
                run_usb(
                    command_out,
                    command_in,
                    data_out,
                    data_in,
                    action_rx,
                    event_tx,
                )
            })
            .context("could not start native USB I/O thread")?;
        self.tx = Some(action_tx);
        Ok(())
    }

    async fn disconnect(&mut self) -> anyhow::Result<()> {
        let tx = self.tx.take().context("transport was not started")?;
        tx.send(TransportAction::Disconnect)
            .context("native USB I/O thread is no longer running")
    }

    async fn send_packet(
        &mut self,
        packet: AvocadoPacket,
    ) -> anyhow::Result<oneshot::Receiver<()>> {
        let tx = self.tx.as_ref().context("transport was not started")?;
        let (completion_tx, completion_rx) = oneshot::channel();
        tx.send(TransportAction::SendPacket(packet, completion_tx))
            .context("native USB I/O thread is no longer running")?;
        Ok(completion_rx)
    }
}

fn pixcut_devices() -> anyhow::Result<impl Iterator<Item = nusb::DeviceInfo>> {
    Ok(nusb::list_devices()
        .wait()
        .context("could not enumerate USB devices")?
        .filter(|device| {
            device.vendor_id() == PIXCUT_USB_VID && device.product_id() == PIXCUT_USB_PID
        }))
}

fn device_key(device: &nusb::DeviceInfo) -> String {
    format!("{}:{}", device.bus_id(), device.device_address())
}

fn run_usb(
    mut command_out: nusb::Endpoint<Bulk, Out>,
    mut command_in: nusb::Endpoint<Bulk, In>,
    mut data_out: nusb::Endpoint<Bulk, Out>,
    mut data_in: nusb::Endpoint<Bulk, In>,
    action_rx: std_mpsc::Receiver<TransportAction>,
    event_tx: mpsc::UnboundedSender<TransportEvent>,
) {
    let _ = event_tx.unbounded_send(TransportEvent::TransportStatus(TransportStatus::Connected));
    let mut response_decoder = JsonResponseDecoder::default();
    let mut pending_responses = VecDeque::new();
    while let Ok(action) = action_rx.recv() {
        match action {
            TransportAction::Disconnect => break,
            TransportAction::SendPacket(packet, completion) => {
                let result = if packet.content_type == ContentType::Data {
                    send_data_packet(&mut data_out, &mut data_in, &packet)
                } else {
                    send_json_packet(
                        &mut command_out,
                        &mut command_in,
                        &packet,
                        &mut response_decoder,
                        &mut pending_responses,
                        &event_tx,
                    )
                    .and_then(|response| {
                        event_tx
                            .unbounded_send(TransportEvent::Packet(response))
                            .map_err(|_| anyhow::anyhow!("transport event receiver was dropped"))
                    })
                };
                match result {
                    Ok(()) => {
                        if completion.send(()).is_err() {
                            debug!("native USB completion receiver was dropped");
                        }
                    }
                    Err(error) => {
                        let _ = event_tx.unbounded_send(TransportEvent::Error(error));
                        break;
                    }
                }
            }
        }
    }
    let _ = event_tx.unbounded_send(TransportEvent::TransportStatus(
        TransportStatus::Disconnected,
    ));
    info!("native USB handler stopped");
}

fn send_json_packet(
    output: &mut nusb::Endpoint<Bulk, Out>,
    input: &mut nusb::Endpoint<Bulk, In>,
    request: &AvocadoPacket,
    decoder: &mut JsonResponseDecoder,
    pending: &mut VecDeque<serde_json::Value>,
    event_tx: &mpsc::UnboundedSender<TransportEvent>,
) -> anyhow::Result<AvocadoPacket> {
    if request.encoding_type != EncodingType::Json {
        bail!("native USB command packet is not JSON encoded");
    }
    let mut frame = b"cmd json\n".to_vec();
    frame.extend_from_slice(&request.data);
    write_frame(output, &frame)?;
    read_matching_json_response(input, request, decoder, pending, event_tx)
}

fn read_matching_json_response(
    input: &mut nusb::Endpoint<Bulk, In>,
    request: &AvocadoPacket,
    decoder: &mut JsonResponseDecoder,
    pending: &mut VecDeque<serde_json::Value>,
    event_tx: &mpsc::UnboundedSender<TransportEvent>,
) -> anyhow::Result<AvocadoPacket> {
    loop {
        if pending.is_empty() {
            let response = read_response(input)?;
            for result in decoder.push(&response) {
                pending.push_back(result?);
            }
        }
        while let Some(value) = pending.pop_front() {
            let response_id = value
                .get("id")
                .and_then(serde_json::Value::as_u64)
                .and_then(|id| u32::try_from(id).ok());
            let packet = json_response_packet(request, response_id.unwrap_or(0), value)?;
            if response_id == Some(request.msg_number) {
                return Ok(packet);
            }
            // Preserve events and stale/out-of-order responses so the manager
            // can route a matching id to another waiter or expose it in logs.
            event_tx
                .unbounded_send(TransportEvent::Packet(packet))
                .map_err(|_| anyhow::anyhow!("transport event receiver was dropped"))?;
        }
    }
}

fn json_response_packet(
    request: &AvocadoPacket,
    message_id: u32,
    value: serde_json::Value,
) -> anyhow::Result<AvocadoPacket> {
    let data = serde_json::to_vec(&value).context("could not preserve native USB response")?;
    trace!(bytes = data.len(), "received native USB JSON response");
    Ok(AvocadoPacket {
        version: request.version,
        content_type: ContentType::Message,
        interaction_type: InteractionType::Response,
        encoding_type: EncodingType::Json,
        encryption_mode: EncryptionMode::None,
        terminal_id: message_id,
        msg_number: message_id,
        msg_package_total: 1,
        msg_package_num: 1,
        is_subpackage: false,
        data,
    })
}

fn send_data_packet(
    output: &mut nusb::Endpoint<Bulk, Out>,
    input: &mut nusb::Endpoint<Bulk, In>,
    request: &AvocadoPacket,
) -> anyhow::Result<()> {
    if request.data.len() < 4 {
        bail!("native USB data packet is missing its job id");
    }
    let job_id = u32::from_le_bytes(request.data[..4].try_into().expect("four-byte slice"));
    let frame = encode_data_frame(job_id, &request.data[4..])?;
    write_frame(output, &frame)?;
    let ack = read_response(input)?;
    if ack.is_empty() {
        bail!("native USB data endpoint returned an empty acknowledgement");
    }
    trace!(
        bytes = ack.len(),
        "received native USB data acknowledgement"
    );
    Ok(())
}

fn write_frame(output: &mut nusb::Endpoint<Bulk, Out>, frame: &[u8]) -> anyhow::Result<()> {
    for slice in write_slices(frame) {
        output
            .transfer_blocking(slice.to_vec().into(), WRITE_TIMEOUT)
            .into_result()
            .context("native USB bulk write failed")?;
    }
    Ok(())
}

fn read_response(input: &mut nusb::Endpoint<Bulk, In>) -> anyhow::Result<Vec<u8>> {
    let completion = input.transfer_blocking(Buffer::new(RESPONSE_BUFFER_SIZE), READ_TIMEOUT);
    let buffer = completion
        .into_result()
        .context("native USB bulk read failed")?;
    Ok(buffer.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_identity_uses_observed_vid_pid_and_interfaces() {
        assert_eq!((PIXCUT_USB_VID, PIXCUT_USB_PID), (0x302c, 0x3101));
        assert_eq!(
            (COMMAND_INTERFACE, COMMAND_OUT_ENDPOINT, COMMAND_IN_ENDPOINT),
            (2, 0x06, 0x86)
        );
        assert_eq!(
            (DATA_INTERFACE, DATA_OUT_ENDPOINT, DATA_IN_ENDPOINT),
            (3, 0x04, 0x84)
        );
    }

    #[test]
    fn selection_is_explicit_and_locked_while_connected() {
        let mut transport = NativeUsbTransport::default();
        assert!(transport.select_device(" ").is_err());
        transport.select_device("bus:2").unwrap();
        assert_eq!(transport.selected_device.as_deref(), Some("bus:2"));
        let (tx, _rx) = std_mpsc::channel();
        transport.tx = Some(tx);
        assert!(transport.select_device("bus:3").is_err());
    }

    #[test]
    fn raw_json_packets_keep_the_wire_response_id() {
        let request = AvocadoPacket {
            version: 100,
            content_type: ContentType::Message,
            interaction_type: InteractionType::Request,
            encoding_type: EncodingType::Json,
            encryption_mode: EncryptionMode::None,
            terminal_id: 9,
            msg_number: 9,
            msg_package_total: 1,
            msg_package_num: 1,
            is_subpackage: false,
            data: br#"{"id":9}"#.to_vec(),
        };
        let packet = json_response_packet(&request, 4, serde_json::json!({"id": 4})).unwrap();
        assert_eq!(packet.msg_number, 4);
        assert_eq!(packet.terminal_id, 4);
        assert_eq!(packet.as_json::<serde_json::Value>().unwrap()["id"], 4);
    }
}
