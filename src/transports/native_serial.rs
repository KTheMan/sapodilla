use std::{
    borrow::Cow,
    io::{Read, Write},
    sync::mpsc as std_mpsc,
    thread,
    time::Duration,
};

use anyhow::{Context, bail};
use async_trait::async_trait;
use futures::channel::{mpsc, oneshot};
use serialport::{DataBits, FlowControl, Parity, SerialPortInfo, SerialPortType, StopBits};
use tracing::{debug, error, info, trace};

use crate::{
    protocol::AvocadoPacket,
    transports::{
        DiscoveredDevice, TransportControl, TransportEvent, TransportStatus, framing::FrameDecoder,
    },
};

const BAUD_RATE: u32 = 9_600;
const IO_TIMEOUT: Duration = Duration::from_millis(100);

#[derive(Debug)]
enum TransportAction {
    SendPacket(AvocadoPacket, oneshot::Sender<()>),
    Disconnect,
}

/// Native serial connection for an OS-visible USB serial or paired Bluetooth
/// Serial Port Profile (RFCOMM) device.
#[derive(Default)]
pub struct NativeSerialTransport {
    selected_port: Option<String>,
    tx: Option<std_mpsc::Sender<TransportAction>>,
}

#[async_trait]
impl TransportControl for NativeSerialTransport {
    fn name(&self) -> Cow<'static, str> {
        "Native Serial (USB / Bluetooth)".into()
    }

    fn supports_discovery(&self) -> bool {
        true
    }

    async fn discover_devices(&mut self) -> anyhow::Result<Vec<DiscoveredDevice>> {
        let ports = serialport::available_ports().context("could not enumerate serial ports")?;
        Ok(discovered_devices(ports))
    }

    fn select_device(&mut self, id: &str) -> anyhow::Result<()> {
        if id.trim().is_empty() {
            bail!("serial port name cannot be empty");
        }
        if self.tx.is_some() {
            bail!("cannot change serial port while connected");
        }
        self.selected_port = Some(id.to_owned());
        Ok(())
    }

    async fn start(
        &mut self,
        event_tx: mpsc::UnboundedSender<TransportEvent>,
    ) -> anyhow::Result<()> {
        if self.tx.is_some() {
            bail!("serial transport is already started");
        }
        let port_name = self
            .selected_port
            .clone()
            .context("select a serial port before connecting")?;

        let _ =
            event_tx.unbounded_send(TransportEvent::TransportStatus(TransportStatus::Connecting));

        let port = match serialport::new(&port_name, BAUD_RATE)
            .data_bits(DataBits::Eight)
            .flow_control(FlowControl::None)
            .parity(Parity::None)
            .stop_bits(StopBits::One)
            .timeout(IO_TIMEOUT)
            .open()
        {
            Ok(port) => port,
            Err(error) => {
                let _ = event_tx.unbounded_send(TransportEvent::TransportStatus(
                    TransportStatus::Disconnected,
                ));
                return Err(error)
                    .with_context(|| format!("could not open serial port {port_name}"));
            }
        };

        let (action_tx, action_rx) = std_mpsc::channel();
        thread::Builder::new()
            .name("sapodilla-serial".into())
            .spawn(move || run_serial(port, action_rx, event_tx))
            .context("could not start serial I/O thread")?;
        self.tx = Some(action_tx);

        Ok(())
    }

    async fn disconnect(&mut self) -> anyhow::Result<()> {
        let Some(tx) = self.tx.take() else {
            bail!("transport was not started");
        };
        tx.send(TransportAction::Disconnect)
            .context("serial I/O thread is no longer running")
    }

    async fn send_packet(
        &mut self,
        packet: AvocadoPacket,
    ) -> anyhow::Result<oneshot::Receiver<()>> {
        let Some(tx) = self.tx.as_ref() else {
            bail!("transport was not started");
        };
        let (completion_tx, completion_rx) = oneshot::channel();
        tx.send(TransportAction::SendPacket(packet, completion_tx))
            .context("serial I/O thread is no longer running")?;
        Ok(completion_rx)
    }
}

fn run_serial(
    mut port: Box<dyn serialport::SerialPort>,
    action_rx: std_mpsc::Receiver<TransportAction>,
    event_tx: mpsc::UnboundedSender<TransportEvent>,
) {
    let _ = event_tx.unbounded_send(TransportEvent::TransportStatus(TransportStatus::Connected));
    let mut decoder = FrameDecoder::default();
    let mut read_buffer = [0u8; 4096];

    'connection: loop {
        while let Ok(action) = action_rx.try_recv() {
            match action {
                TransportAction::Disconnect => break 'connection,
                TransportAction::SendPacket(packet, completion) => {
                    let encoded = packet.encode();
                    trace!(bytes = encoded.len(), "writing serial packet");
                    if let Err(error) = port.write_all(&encoded).and_then(|_| port.flush()) {
                        let _ = event_tx.unbounded_send(TransportEvent::Error(
                            anyhow::Error::new(error).context("could not write to serial port"),
                        ));
                        break 'connection;
                    }
                    if completion.send(()).is_err() {
                        debug!("serial packet completion receiver was dropped");
                    }
                }
            }
        }

        match port.read(&mut read_buffer) {
            Ok(0) => {}
            Ok(count) => {
                trace!(count, "read serial bytes");
                for result in decoder.push(&read_buffer[..count]) {
                    match result {
                        Ok(packet) => {
                            if event_tx
                                .unbounded_send(TransportEvent::Packet(packet))
                                .is_err()
                            {
                                break 'connection;
                            }
                        }
                        Err(error) => {
                            error!(%error, "discarded invalid serial frame");
                            let _ = event_tx.unbounded_send(TransportEvent::Error(error.into()));
                        }
                    }
                }
            }
            Err(error) if matches!(error.kind(), std::io::ErrorKind::TimedOut) => {}
            Err(error) => {
                let _ = event_tx.unbounded_send(TransportEvent::Error(
                    anyhow::Error::new(error).context("could not read from serial port"),
                ));
                break;
            }
        }
    }

    drop(port);
    let _ = event_tx.unbounded_send(TransportEvent::TransportStatus(
        TransportStatus::Disconnected,
    ));
    info!("native serial handler stopped");
}

fn discovered_devices(mut ports: Vec<SerialPortInfo>) -> Vec<DiscoveredDevice> {
    // macOS enumerates both callout and dial-in names for one endpoint. Prefer
    // /dev/cu.*, which is intended for initiating an outgoing connection.
    let callout_names: Vec<String> = ports
        .iter()
        .filter_map(|port| port.port_name.strip_prefix("/dev/cu.").map(str::to_owned))
        .collect();
    ports.retain(|port| {
        port.port_name
            .strip_prefix("/dev/tty.")
            .is_none_or(|name| !callout_names.iter().any(|callout| callout == name))
    });
    ports.sort_by_cached_key(|port| port.port_name.to_ascii_lowercase());

    ports
        .into_iter()
        .map(|port| DiscoveredDevice {
            id: port.port_name.clone(),
            name: port.port_name,
            details: Some(port_details(&port.port_type)),
        })
        .collect()
}

fn port_details(port_type: &SerialPortType) -> String {
    match port_type {
        SerialPortType::BluetoothPort => "Bluetooth serial port".into(),
        SerialPortType::PciPort => "PCI serial port".into(),
        SerialPortType::Unknown => "Serial port (connection type unknown)".into(),
        SerialPortType::UsbPort(info) => {
            let mut details = vec![format!("USB {:04X}:{:04X}", info.vid, info.pid)];
            if let Some(product) = info.product.as_deref() {
                details.push(product.to_owned());
            }
            if let Some(manufacturer) = info.manufacturer.as_deref() {
                details.push(manufacturer.to_owned());
            }
            if let Some(serial_number) = info.serial_number.as_deref() {
                details.push(format!("S/N {serial_number}"));
            }
            details.join(" · ")
        }
    }
}

#[cfg(test)]
mod tests {
    use serialport::UsbPortInfo;

    use super::*;

    #[test]
    fn discovery_is_sorted_and_deduplicates_macos_dial_in_port() {
        let ports = vec![
            SerialPortInfo {
                port_name: "/dev/tty.PixCut".into(),
                port_type: SerialPortType::BluetoothPort,
            },
            SerialPortInfo {
                port_name: "/dev/cu.PixCut".into(),
                port_type: SerialPortType::BluetoothPort,
            },
            SerialPortInfo {
                port_name: "/dev/cu.A-other".into(),
                port_type: SerialPortType::Unknown,
            },
        ];

        let devices = discovered_devices(ports);

        assert_eq!(
            devices
                .iter()
                .map(|device| device.id.as_str())
                .collect::<Vec<_>>(),
            vec!["/dev/cu.A-other", "/dev/cu.PixCut"]
        );
    }

    #[test]
    fn usb_details_include_identifiers_without_selecting_a_device() {
        let devices = discovered_devices(vec![SerialPortInfo {
            port_name: "COM12".into(),
            port_type: SerialPortType::UsbPort(UsbPortInfo {
                vid: 0x1234,
                pid: 0xabcd,
                serial_number: Some("ABC".into()),
                manufacturer: Some("Liene".into()),
                product: Some("PixCut S1".into()),
            }),
        }]);

        assert_eq!(devices[0].id, "COM12");
        assert_eq!(
            devices[0].details.as_deref(),
            Some("USB 1234:ABCD · PixCut S1 · Liene · S/N ABC")
        );
    }

    #[test]
    fn port_selection_is_explicit_and_preserves_the_platform_identifier() {
        let mut transport = NativeSerialTransport::default();
        assert_eq!(transport.selected_port, None);
        assert!(transport.select_device("  ").is_err());

        transport.select_device("COM27").unwrap();

        assert_eq!(transport.selected_port.as_deref(), Some("COM27"));
    }

    #[test]
    fn connected_port_cannot_be_changed() {
        let (tx, _rx) = std_mpsc::channel();
        let mut transport = NativeSerialTransport {
            selected_port: Some("COM1".into()),
            tx: Some(tx),
        };

        assert!(transport.select_device("COM2").is_err());
        assert_eq!(transport.selected_port.as_deref(), Some("COM1"));
    }
}
