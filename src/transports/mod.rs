use std::borrow::Cow;
use std::sync::atomic::{AtomicBool, AtomicU32};
use std::time::Duration;

use anyhow::bail;
use async_trait::async_trait;
use egui::ahash::HashMap;
use enum_dispatch::enum_dispatch;
use futures::{
    SinkExt, StreamExt,
    channel::{mpsc, oneshot},
    lock::Mutex,
};
use tracing::{debug, error, info, instrument, trace, warn};

use crate::protocol::*;

use crate::transports::mock::MockTransport;
#[cfg(all(not(target_arch = "wasm32"), feature = "native-ble"))]
use crate::transports::native_ble::NativeBleTransport;
#[cfg(not(target_arch = "wasm32"))]
use crate::transports::native_serial::NativeSerialTransport;
#[cfg(all(not(target_arch = "wasm32"), feature = "native-usb"))]
use crate::transports::native_usb::NativeUsbTransport;
#[cfg(target_arch = "wasm32")]
use crate::transports::web_serial::WebSerialTransport;
use crate::{Rc, interval, spawn};

pub mod framing;
pub mod mock;
#[cfg(all(not(target_arch = "wasm32"), feature = "native-ble"))]
pub mod native_ble;
#[cfg(not(target_arch = "wasm32"))]
pub mod native_serial;
#[cfg(all(not(target_arch = "wasm32"), feature = "native-usb"))]
pub mod native_usb;
#[cfg(target_arch = "wasm32")]
pub mod web_serial;

/// Static message ID to ensure we never reuse an ID, even across different
/// transport instances. Generally accessed through
/// [`TransportManager::next_message_id`].
static MESSAGE_ID: AtomicU32 = AtomicU32::new(1);

/// Maximum size of data within a message.
pub const MAX_DATA_SIZE: usize = 896;
/// Maximum time to wait for a command response after its bytes have been
/// accepted by the transport.
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(15);

/// A transport for sending packet data.
///
/// You should construct a [`TransportManager`] from this `Transport` rather
/// than trying to use it directly.
#[enum_dispatch(TransportControl)]
#[derive(strum::EnumIter)]
#[allow(clippy::enum_variant_names)]
pub enum Transport {
    #[cfg(all(not(target_arch = "wasm32"), feature = "native-ble"))]
    NativeBleTransport,
    #[cfg(all(not(target_arch = "wasm32"), feature = "native-usb"))]
    NativeUsbTransport,
    #[cfg(not(target_arch = "wasm32"))]
    NativeSerialTransport,
    #[cfg(target_arch = "wasm32")]
    WebSerialTransport,
    MockTransport,
}

/// Information about a discovered device.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveredDevice {
    /// Stable platform identifier used to open the device (for example COM4).
    pub id: String,
    /// The primary name of the device.
    pub name: String,
    /// An optional detail string about the device.
    pub details: Option<String>,
}

/// An event from the [`TransportManager`].
#[allow(dead_code)]
#[derive(Debug)]
pub enum TransportEvent {
    /// Sent when the transport is connecting, disconnected, etc.
    TransportStatus(TransportStatus),
    /// Info about the status of the device, automatically fetched every few
    /// seconds when the transport is not sending large data.
    DeviceStatus((PrinterState, PrinterSubState, String)),
    /// Info about a job, sent after calling [`TransportManager::poll_job`]
    /// until the job reaches a terminal state.
    JobStatus(JobStatusInfo),
    /// Sent for all received packets.
    Packet(AvocadoPacket),
    /// An error from the transport.
    Error(anyhow::Error),
}

/// The transport's current device connection status.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Hash)]
pub enum TransportStatus {
    Connecting,
    Connected,
    Disconnecting,
    Disconnected,
}

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
#[enum_dispatch]
pub trait TransportControl {
    fn name(&self) -> Cow<'static, str>;
    #[allow(dead_code)]
    fn supports_discovery(&self) -> bool;

    /// Maximum encoded Hannto body size accepted efficiently by this link.
    fn max_data_size(&self) -> usize {
        MAX_DATA_SIZE
    }

    #[allow(dead_code)]
    async fn discover_devices(&mut self) -> anyhow::Result<Vec<DiscoveredDevice>> {
        bail!("discovery not supported for transport");
    }

    /// Choose a discovered device. Discovery transports must require an
    /// explicit selection rather than opening an arbitrary first port.
    #[allow(dead_code)]
    fn select_device(&mut self, _id: &str) -> anyhow::Result<()> {
        bail!("device selection not supported for transport");
    }

    async fn start(
        &mut self,
        mut event_tx: mpsc::UnboundedSender<TransportEvent>,
    ) -> Result<(), anyhow::Error>;

    async fn disconnect(&mut self) -> anyhow::Result<()>;

    async fn send_packet(&mut self, packet: AvocadoPacket)
    -> anyhow::Result<oneshot::Receiver<()>>;
}

/// A wrapper around a transport to add needed functions such as waiting for the
/// result of a package and handling background status updates.
#[derive(Clone)]
pub struct TransportManager {
    transport: Rc<Mutex<Transport>>,
    event_tx: mpsc::UnboundedSender<TransportEvent>,

    sending: Rc<AtomicBool>,
    pending: Rc<Mutex<HashMap<u32, oneshot::Sender<AvocadoPacket>>>>,
}

impl TransportManager {
    /// Create a new manager for a given transport.
    ///
    /// Handles starting the transport, polling device status, and attaching
    /// incoming packets to waiting requests.
    pub fn new<F>(transport: Rc<Mutex<Transport>>, cb: F) -> Rc<Self>
    where
        F: Fn(TransportEvent) + Send + Sync + 'static,
    {
        let (mut event_tx, mut event_rx) = mpsc::unbounded();
        let (ready_tx, ready_rx) = oneshot::channel();

        let sending = Rc::new(AtomicBool::new(false));
        let pending: Rc<Mutex<HashMap<u32, oneshot::Sender<AvocadoPacket>>>> = Default::default();

        let manager = Rc::new(Self {
            transport: transport.clone(),
            event_tx: event_tx.clone(),

            sending: sending.clone(),
            pending: pending.clone(),
        });

        spawn({
            let manager = manager.clone();
            let mut event_tx = event_tx.clone();

            async move {
                if ready_rx.await.is_err() {
                    warn!("ready was dropped before ready");
                    return;
                }

                info!("connection marked as ready, starting info polling");

                let mut stream = interval(Duration::from_secs(1));
                while stream.next().await.is_some() {
                    if event_tx.is_closed() {
                        warn!("event sender was closed, ending status stream");
                        break;
                    }

                    if sending.load(std::sync::atomic::Ordering::SeqCst) {
                        trace!("skipping status request because sending data");
                        continue;
                    }

                    let id = manager.next_message_id();
                    let packet = AvocadoPacket {
                        version: 100,
                        content_type: ContentType::Message,
                        interaction_type: InteractionType::Request,
                        encoding_type: EncodingType::Json,
                        encryption_mode: EncryptionMode::None,
                        terminal_id: id,
                        msg_number: id,
                        msg_package_total: 1,
                        msg_package_num: 1,
                        is_subpackage: false,
                        data: serde_json::to_vec(&serde_json::json!({
                            "id" : id,
                            "method" : "get-prop",
                            "params" : [
                                "printer-state",
                                "printer-sub-state",
                                "printer-state-alerts",
                            ]
                        }))
                        .unwrap(),
                    };
                    trace!(?packet, "prepared get-prop request");

                    let packet = match manager.wait_for_response(packet).await {
                        Ok(packet) => packet,
                        Err(err) => {
                            error!("error fetching status packet: {err}");
                            break;
                        }
                    };
                    trace!(?packet, "got get-prop response");

                    if let Some(result) =
                        packet.as_json::<AvocadoResult<(PrinterState, PrinterSubState, String)>>()
                    {
                        debug!("got status: {:?}", result.result);

                        if let Err(err) = event_tx
                            .send(TransportEvent::DeviceStatus(result.result))
                            .await
                        {
                            error!("could not send device status: {err:?}");
                            break;
                        }
                    } else {
                        error!("could not decode printer status: {packet:?}");
                    }
                }

                info!("status interval stream ended");
            }
        });

        spawn(async move {
            let mut ready_tx = Some(ready_tx);

            while let Some(event) = event_rx.next().await {
                match &event {
                    TransportEvent::Packet(packet) => {
                        if let Some(data) = packet.as_json::<AvocadoId>() {
                            if let Some(pending) = pending.lock().await.remove(&data.id)
                                && pending.send(packet.clone()).is_err()
                            {
                                error!("could not send packet to pending");
                            }
                        } else if packet.content_type == ContentType::Message
                            && packet.encoding_type == EncodingType::Json
                        {
                            warn!("got json message without id");
                        }
                    }
                    TransportEvent::TransportStatus(TransportStatus::Connected) => {
                        if let Some(ready_tx) = ready_tx.take() {
                            let _ = ready_tx.send(());
                        }
                    }
                    _ => trace!("got other event: {event:?}"),
                }

                cb(event);
            }
        });

        spawn(async move {
            let mut transport = transport.lock().await;
            if let Err(err) = transport.start(event_tx.clone()).await
                && let Err(err) = event_tx.send(TransportEvent::Error(err)).await
            {
                error!("could not send transport start error: {err}");
            }
        });

        manager
    }

    /// Disconnect transport.
    pub async fn disconnect(&self) -> anyhow::Result<()> {
        info!("disconnecting transport");
        self.event_tx
            .clone()
            .send(TransportEvent::TransportStatus(
                TransportStatus::Disconnecting,
            ))
            .await?;
        self.transport.lock().await.disconnect().await
    }

    /// Release transport-owned connection state after its worker has already
    /// reported a terminal disconnect. Errors are intentionally ignored by
    /// callers because the physical link is already gone; taking the channel
    /// or peripheral handle is what makes a subsequent `start` possible.
    pub async fn reset_after_loss(&self) {
        let _ = self.transport.lock().await.disconnect().await;
    }

    /// Get the next message ID.
    pub fn next_message_id(&self) -> u32 {
        let id = MESSAGE_ID.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        trace!(id, "generated next message id");
        id
    }

    /// Send a packet and wait for the resulting packet.
    ///
    /// The response wait is bounded and its pending-map entry is removed on
    /// write failure, channel closure, or timeout.
    #[instrument(skip_all, fields(msg_number = packet.msg_number))]
    pub async fn wait_for_response(&self, packet: AvocadoPacket) -> anyhow::Result<AvocadoPacket> {
        self.wait_for_response_with_timeout(packet, RESPONSE_TIMEOUT)
            .await
    }

    async fn wait_for_response_with_timeout(
        &self,
        packet: AvocadoPacket,
        timeout: Duration,
    ) -> anyhow::Result<AvocadoPacket> {
        let (tx, rx) = oneshot::channel();
        let message_number = packet.msg_number;

        debug!("sending packet");
        self.pending.lock().await.insert(message_number, tx);
        let write_result = self.transport.lock().await.send_packet(packet).await;
        let write_result = match write_result {
            Ok(completion) => completion.await.map_err(anyhow::Error::from),
            Err(error) => Err(error),
        };
        if let Err(error) = write_result {
            self.pending.lock().await.remove(&message_number);
            return Err(error);
        }
        trace!("packet marked as sent");

        let result = receive_with_timeout(rx, timeout).await;
        if result.is_err() {
            self.pending.lock().await.remove(&message_number);
        }
        result
    }

    /// Poll a job for status updates.
    ///
    /// Updates are sent through the manager's event stream. This method returns
    /// after the job has reached a terminal state.
    #[instrument(skip(self))]
    pub async fn poll_job(&self, job_id: u32) -> anyhow::Result<()> {
        let mut event_tx = self.event_tx.clone();

        let mut stream = interval(Duration::from_secs(1));
        while stream.next().await.is_some() {
            if event_tx.is_closed() {
                warn!("event sender was closed, ending job status stream");
                break;
            }

            let id = self.next_message_id();
            let packet = AvocadoPacket {
                version: 100,
                content_type: ContentType::Message,
                interaction_type: InteractionType::Request,
                encoding_type: EncodingType::Json,
                encryption_mode: EncryptionMode::None,
                terminal_id: id,
                msg_number: id,
                msg_package_total: 1,
                msg_package_num: 1,
                is_subpackage: false,
                data: serde_json::to_vec(&serde_json::json!({
                    "id": id,
                    "method": "get-job-info",
                    "params": { "job-id": job_id },
                }))
                .unwrap(),
            };
            trace!(?packet, "prepared get-job-info request");

            let packet = match self.wait_for_response(packet).await {
                Ok(packet) => packet,
                Err(err) => {
                    error!("error fetching job status packet: {err}");
                    return Err(err);
                }
            };
            trace!(?packet, "got get-job-info response");

            if let Some(result) = packet.as_json::<AvocadoResult<JobStatusResult>>() {
                debug!("got get-job-info info: {:?}", result.result);

                let Some(info) = result.result.into_for_job(job_id) else {
                    warn!("result was missing job info");
                    continue;
                };

                let is_complete = matches!(
                    info.job_state,
                    JobState::Aborted | JobState::Cancelled | JobState::Completed
                );

                if let Err(err) = event_tx.send(TransportEvent::JobStatus(info)).await {
                    error!("could not send job status: {err:?}");
                    break;
                }

                if is_complete {
                    info!("job reached terminal state, ending status polling");
                    break;
                }
            } else {
                let value = packet.as_json::<serde_json::Value>();
                error!("could not decode job status: {packet:?}, {value:?}");
                bail!("printer returned an invalid get-job-info response: {value:?}");
            }
        }

        Ok(())
    }

    /// Send binary data to the device for a given job.
    ///
    /// Will return an error if data is already being sent.
    #[instrument(skip(self, data, f))]
    pub async fn send_data<F>(&self, job_id: u32, data: &[u8], f: F) -> anyhow::Result<()>
    where
        F: Fn(usize, usize),
    {
        let Some(_guard) = SendingDropGuard::new(self.sending.clone()) else {
            bail!("cannot start sending data while other send is in progress");
        };

        let max_data_size = self.transport.lock().await.max_data_size().max(5);
        let count = usize::div_ceil(data.len(), max_data_size - 4);
        debug!(chunks = count, "sending data with {} bytes", data.len());

        for (index, chunk) in data.chunks(max_data_size - 4).enumerate() {
            let mut buf: Vec<u8> = Vec::with_capacity(max_data_size);
            buf.extend(&job_id.to_le_bytes());
            buf.extend_from_slice(chunk);

            let id = self.next_message_id();
            let packet = AvocadoPacket {
                version: 100,
                content_type: ContentType::Data,
                interaction_type: InteractionType::Request,
                encoding_type: EncodingType::Hexadecimal,
                encryption_mode: EncryptionMode::None,
                terminal_id: id,
                msg_number: id,
                msg_package_total: u16::try_from(count).unwrap(),
                msg_package_num: u16::try_from(index + 1).unwrap(),
                is_subpackage: count > 1,
                data: buf,
            };
            trace!(index, ?packet, "sending data packet");

            // Make sure we're waiting for the internal write to happen before
            // we attempt to write the next packet in this package.
            self.transport
                .lock()
                .await
                .send_packet(packet)
                .await?
                .await?;

            f(count, index + 1);
        }

        Ok(())
    }
}

#[cfg(not(target_arch = "wasm32"))]
async fn receive_with_timeout<T>(rx: oneshot::Receiver<T>, timeout: Duration) -> anyhow::Result<T> {
    match tokio::time::timeout(timeout, rx).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => Err(error.into()),
        Err(_) => bail!(
            "printer response timed out after {} seconds",
            timeout.as_secs()
        ),
    }
}

#[cfg(target_arch = "wasm32")]
async fn receive_with_timeout<T>(rx: oneshot::Receiver<T>, timeout: Duration) -> anyhow::Result<T> {
    use futures::FutureExt as _;

    let timeout_ms = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX);
    let response = rx.fuse();
    let timer = gloo_timers::future::TimeoutFuture::new(timeout_ms).fuse();
    futures::pin_mut!(response, timer);
    futures::select! {
        response = response => response.map_err(Into::into),
        _ = timer => bail!("printer response timed out after {} seconds", timeout.as_secs()),
    }
}

/// Helper to set and remove the sending flag in a [`TransportManager`].
///
/// Automatically marks it as sending upon creation and unmarks it when dropped.
struct SendingDropGuard {
    sending: Rc<AtomicBool>,
}

impl SendingDropGuard {
    /// Create a new guard, if the sending flag was not already set.
    fn new(sending: Rc<AtomicBool>) -> Option<Self> {
        if sending.swap(true, std::sync::atomic::Ordering::SeqCst) {
            warn!("attempted to create sending guard when already sending");
            return None;
        }

        trace!("marking as sending");
        Some(Self { sending })
    }
}

impl Drop for SendingDropGuard {
    fn drop(&mut self) {
        trace!("sending dropped, releasing");
        self.sending
            .store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;

    fn request(id: u32) -> AvocadoPacket {
        AvocadoPacket {
            version: 100,
            content_type: ContentType::Message,
            interaction_type: InteractionType::Request,
            encoding_type: EncodingType::Json,
            encryption_mode: EncryptionMode::None,
            terminal_id: id,
            msg_number: id,
            msg_package_total: 1,
            msg_package_num: 1,
            is_subpackage: false,
            data: serde_json::to_vec(&serde_json::json!({ "id": id })).unwrap(),
        }
    }

    #[tokio::test]
    async fn response_timeout_removes_pending_request() {
        let (event_tx, _event_rx) = mpsc::unbounded();
        let manager = TransportManager {
            transport: Rc::new(Mutex::new(Transport::MockTransport(
                MockTransport::default(),
            ))),
            event_tx,
            sending: Rc::new(AtomicBool::new(false)),
            pending: Default::default(),
        };

        let error = manager
            .wait_for_response_with_timeout(request(42), Duration::from_millis(1))
            .await
            .unwrap_err();

        assert!(error.to_string().contains("timed out"));
        assert!(manager.pending.lock().await.is_empty());
    }
}
