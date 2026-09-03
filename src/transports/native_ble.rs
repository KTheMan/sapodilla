use std::{borrow::Cow, time::Duration};

use anyhow::{Context, bail};
use async_trait::async_trait;
use btleplug::{
    api::{Central, Characteristic, Manager as _, Peripheral as _, ScanFilter, WriteType},
    platform::{Manager, Peripheral},
};
use futures::{
    StreamExt,
    channel::{mpsc, oneshot},
};
use tracing::{error, trace};
use uuid::Uuid;

use crate::{
    protocol::AvocadoPacket,
    spawn,
    transports::{
        DiscoveredDevice, TransportControl, TransportEvent, TransportStatus, framing::FrameDecoder,
    },
};

const PIXCUT_ADV_SERVICE: Uuid = Uuid::from_u128(0x0000fc00_0000_1000_8000_00805f9b34fb);
const WRITE_CHARACTERISTIC: Uuid = Uuid::from_u128(0x0000ff02_0000_1000_8000_00805f9b34fb);
const DATA_CHARACTERISTIC: Uuid = Uuid::from_u128(0x0000ff01_0000_1000_8000_00805f9b34fb);
const STATUS_CHARACTERISTIC: Uuid = Uuid::from_u128(0x0000ff03_0000_1000_8000_00805f9b34fb);
const DATA_BODY_SIZE: usize = 160; // four-byte job id plus 156 payload bytes
const MAX_FRAME_SIZE: usize = DATA_BODY_SIZE + 22;
const SCAN_TIME: Duration = Duration::from_secs(3);
const ACK_TIMEOUT: Duration = Duration::from_secs(5);
const FRAGMENT_PACING: Duration = Duration::from_millis(20);

/// Native BLE GATT transport using the PixCut FF00 data profile.
#[derive(Default)]
pub struct NativeBleTransport {
    selected_device: Option<String>,
    peripheral: Option<Peripheral>,
    write_characteristic: Option<Characteristic>,
    acknowledgements: Option<mpsc::UnboundedReceiver<()>>,
}

#[async_trait]
impl TransportControl for NativeBleTransport {
    fn name(&self) -> Cow<'static, str> {
        "PixCut Bluetooth LE".into()
    }

    fn supports_discovery(&self) -> bool {
        true
    }

    fn max_data_size(&self) -> usize {
        DATA_BODY_SIZE
    }

    async fn discover_devices(&mut self) -> anyhow::Result<Vec<DiscoveredDevice>> {
        let peripherals = scan_pixcut().await?;
        let mut devices = Vec::with_capacity(peripherals.len());
        for peripheral in peripherals {
            let properties = peripheral.properties().await?.unwrap_or_default();
            let name = properties
                .local_name
                .unwrap_or_else(|| "Liene PixCut S1".into());
            devices.push(DiscoveredDevice {
                id: peripheral.id().to_string(),
                name,
                details: Some(format!("Bluetooth LE · {}", properties.address)),
            });
        }
        Ok(devices)
    }

    fn select_device(&mut self, id: &str) -> anyhow::Result<()> {
        if self.peripheral.is_some() {
            bail!("cannot change BLE device while connected");
        }
        if id.trim().is_empty() {
            bail!("BLE device identifier cannot be empty");
        }
        self.selected_device = Some(id.to_owned());
        Ok(())
    }

    async fn start(
        &mut self,
        event_tx: mpsc::UnboundedSender<TransportEvent>,
    ) -> anyhow::Result<()> {
        if self.peripheral.is_some() {
            bail!("BLE transport is already started");
        }
        let selected = self
            .selected_device
            .as_deref()
            .context("select a PixCut BLE device before connecting")?;
        let _ =
            event_tx.unbounded_send(TransportEvent::TransportStatus(TransportStatus::Connecting));
        let peripheral = scan_pixcut()
            .await?
            .into_iter()
            .find(|peripheral| peripheral.id().to_string() == selected)
            .with_context(|| format!("selected BLE device {selected} is no longer available"))?;
        peripheral
            .connect()
            .await
            .context("could not connect to PixCut over BLE")?;
        peripheral
            .discover_services()
            .await
            .context("could not discover PixCut BLE services")?;
        let characteristics = peripheral.characteristics();
        let write = find_characteristic(&characteristics, WRITE_CHARACTERISTIC)?;
        let data = find_characteristic(&characteristics, DATA_CHARACTERISTIC)?;
        let status = find_characteristic(&characteristics, STATUS_CHARACTERISTIC)?;
        peripheral
            .subscribe(&data)
            .await
            .context("could not subscribe to PixCut BLE data")?;
        peripheral
            .subscribe(&status)
            .await
            .context("could not subscribe to PixCut BLE status")?;

        let mut notifications = peripheral
            .notifications()
            .await
            .context("could not open PixCut BLE notification stream")?;
        let (ack_tx, ack_rx) = mpsc::unbounded();
        let notification_events = event_tx.clone();
        spawn(async move {
            let mut decoder = FrameDecoder::default();
            while let Some(notification) = notifications.next().await {
                if notification.uuid == STATUS_CHARACTERISTIC {
                    if notification.value.as_slice() == [0x01, 0x01]
                        && ack_tx.unbounded_send(()).is_err()
                    {
                        break;
                    }
                } else if notification.uuid == DATA_CHARACTERISTIC {
                    for packet in decoder.push(&notification.value) {
                        match packet {
                            Ok(packet) => {
                                if notification_events
                                    .unbounded_send(TransportEvent::Packet(packet))
                                    .is_err()
                                {
                                    return;
                                }
                            }
                            Err(error) => {
                                let _ = notification_events
                                    .unbounded_send(TransportEvent::Error(error.into()));
                            }
                        }
                    }
                }
            }
            let _ = notification_events.unbounded_send(TransportEvent::TransportStatus(
                TransportStatus::Disconnected,
            ));
        });

        self.peripheral = Some(peripheral);
        self.write_characteristic = Some(write);
        self.acknowledgements = Some(ack_rx);
        let _ =
            event_tx.unbounded_send(TransportEvent::TransportStatus(TransportStatus::Connected));
        Ok(())
    }

    async fn disconnect(&mut self) -> anyhow::Result<()> {
        let peripheral = self
            .peripheral
            .take()
            .context("transport was not started")?;
        self.write_characteristic = None;
        self.acknowledgements = None;
        peripheral
            .disconnect()
            .await
            .context("could not disconnect PixCut BLE")
    }

    async fn send_packet(
        &mut self,
        packet: AvocadoPacket,
    ) -> anyhow::Result<oneshot::Receiver<()>> {
        let peripheral = self
            .peripheral
            .as_ref()
            .context("transport was not started")?;
        let characteristic = self
            .write_characteristic
            .as_ref()
            .context("PixCut BLE write characteristic is unavailable")?;
        let acknowledgements = self
            .acknowledgements
            .as_mut()
            .context("PixCut BLE acknowledgement stream is unavailable")?;
        let encoded = packet.encode();
        // The printer acknowledges complete Hannto frames, not ATT fragments.
        // Job-data frames fit in one observed 182-byte operation; larger JSON
        // commands are delivered as contiguous GATT writes and acknowledged
        // only after the complete Hannto frame has arrived.
        let chunk_count = usize::div_ceil(encoded.len(), MAX_FRAME_SIZE);
        for (index, chunk) in ble_write_chunks(&encoded).enumerate() {
            peripheral
                .write(characteristic, chunk, WriteType::WithoutResponse)
                .await
                .context("could not write PixCut BLE frame fragment")?;
            if index + 1 < chunk_count {
                tokio::time::sleep(FRAGMENT_PACING).await;
            }
        }
        match tokio::time::timeout(ACK_TIMEOUT, acknowledgements.next()).await {
            Ok(Some(())) => trace!(bytes = encoded.len(), "PixCut BLE frame acknowledged"),
            Ok(None) => bail!("PixCut BLE acknowledgement stream ended"),
            Err(_) => bail!("PixCut BLE frame acknowledgement timed out"),
        }
        let (completion_tx, completion_rx) = oneshot::channel();
        let _ = completion_tx.send(());
        Ok(completion_rx)
    }
}

fn ble_write_chunks(frame: &[u8]) -> impl Iterator<Item = &[u8]> {
    frame.chunks(MAX_FRAME_SIZE)
}

fn find_characteristic(
    characteristics: &std::collections::BTreeSet<Characteristic>,
    uuid: Uuid,
) -> anyhow::Result<Characteristic> {
    characteristics
        .iter()
        .find(|characteristic| characteristic.uuid == uuid)
        .cloned()
        .with_context(|| format!("PixCut BLE characteristic {uuid} is unavailable"))
}

async fn scan_pixcut() -> anyhow::Result<Vec<Peripheral>> {
    let manager = Manager::new()
        .await
        .context("could not initialize Bluetooth")?;
    let adapters = manager
        .adapters()
        .await
        .context("could not enumerate Bluetooth adapters")?;
    let mut matches = Vec::new();
    for adapter in adapters {
        adapter
            .start_scan(ScanFilter {
                services: vec![PIXCUT_ADV_SERVICE],
            })
            .await
            .context("could not start Bluetooth scan")?;
        tokio::time::sleep(SCAN_TIME).await;
        for peripheral in adapter.peripherals().await? {
            let properties = peripheral.properties().await?.unwrap_or_default();
            let named_pixcut = properties
                .local_name
                .as_deref()
                .is_some_and(|name| name.starts_with("Liene PixCut S1"));
            let advertises_pixcut = properties.services.contains(&PIXCUT_ADV_SERVICE)
                || properties.service_data.contains_key(&PIXCUT_ADV_SERVICE);
            if named_pixcut || advertises_pixcut {
                matches.push(peripheral);
            }
        }
        if let Err(error) = adapter.stop_scan().await {
            error!(%error, "could not stop Bluetooth scan");
        }
    }
    matches.sort_by_key(|peripheral| peripheral.id().to_string());
    matches.dedup_by_key(|peripheral| peripheral.id().to_string());
    Ok(matches)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pixcut_ble_profile_constants_match_observed_gatt_layout() {
        assert_eq!(
            WRITE_CHARACTERISTIC.to_string(),
            "0000ff02-0000-1000-8000-00805f9b34fb"
        );
        assert_eq!(
            DATA_CHARACTERISTIC.to_string(),
            "0000ff01-0000-1000-8000-00805f9b34fb"
        );
        assert_eq!(
            STATUS_CHARACTERISTIC.to_string(),
            "0000ff03-0000-1000-8000-00805f9b34fb"
        );
        assert_eq!(DATA_BODY_SIZE - 4, 156);
        assert_eq!(MAX_FRAME_SIZE, 182);
    }

    #[test]
    fn large_json_frame_fragments_but_reassembles_before_one_frame_ack() {
        let frame = (0..700)
            .map(|index| (index % 251) as u8)
            .collect::<Vec<_>>();
        let chunks = ble_write_chunks(&frame).collect::<Vec<_>>();
        assert_eq!(
            chunks.iter().map(|chunk| chunk.len()).collect::<Vec<_>>(),
            [182, 182, 182, 154]
        );
        assert_eq!(chunks.concat(), frame);
    }
}
