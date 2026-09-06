use std::borrow::Cow;
use std::sync::atomic::{AtomicBool, AtomicU32};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
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
use crate::raw_usb::DATA_HEARTBEAT_INTERVAL;

use crate::transports::mock::MockTransport;
#[cfg(all(not(target_arch = "wasm32"), feature = "native-ble"))]
use crate::transports::native_ble::NativeBleTransport;
#[cfg(not(target_arch = "wasm32"))]
use crate::transports::native_serial::NativeSerialTransport;
#[cfg(all(not(target_arch = "wasm32"), feature = "native-usb"))]
use crate::transports::native_usb::NativeUsbTransport;
#[cfg(target_arch = "wasm32")]
use crate::transports::web_serial::WebSerialTransport;
#[cfg(target_arch = "wasm32")]
use crate::transports::web_usb::WebUsbTransport;
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
#[cfg(target_arch = "wasm32")]
pub mod web_usb;

/// Static message ID to ensure we never reuse an ID, even across different
/// transport instances. Generally accessed through
/// [`TransportManager::next_message_id`].
static MESSAGE_ID: AtomicU32 = AtomicU32::new(1);

/// Maximum size of data within a message.
pub const MAX_DATA_SIZE: usize = 896;
/// Maximum time to wait for a command response after its bytes have been
/// accepted by the transport.
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_JOB_POLLS: usize = 15 * 60;
const MAX_MISSING_JOB_POLLS: usize = 30;

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
    #[cfg(target_arch = "wasm32")]
    WebUsbTransport,
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

/// Job-command schema expected by a transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobCommandProfile {
    Hannto,
    PixCutUsb,
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

    fn job_command_profile(&self) -> JobCommandProfile {
        JobCommandProfile::Hannto
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
                    TransportEvent::TransportStatus(TransportStatus::Disconnected) => {
                        if ready_tx.take().is_some() {
                            debug!("transport disconnected before becoming ready");
                        }
                    }
                    _ => trace!("got other event: {event:?}"),
                }

                cb(event);
            }
        });

        spawn(async move {
            let mut transport = transport.lock().await;
            if let Err(err) = transport.start(event_tx.clone()).await {
                if let Err(send_err) = event_tx.send(TransportEvent::Error(err)).await {
                    error!("could not send transport start error: {send_err}");
                }
                if let Err(send_err) = event_tx
                    .send(TransportEvent::TransportStatus(
                        TransportStatus::Disconnected,
                    ))
                    .await
                {
                    error!("could not send transport disconnected status: {send_err}");
                }
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

    pub async fn job_command_profile(&self) -> JobCommandProfile {
        self.transport.lock().await.job_command_profile()
    }

    async fn request_json(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> anyhow::Result<AvocadoPacket> {
        let id = self.next_message_id();
        self.wait_for_response(AvocadoPacket {
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
                "method": method,
                "params": params,
            }))?,
        })
        .await
    }

    /// Warm up the PixCut USB command channel and wake a sleeping printer
    /// before declaring a job. Device-reported paper labels are intentionally
    /// not used here: they are static firmware metadata, not a media sensor.
    pub async fn prepare_pixcut_usb_job(&self) -> anyhow::Result<()> {
        self.request_json(
            "get-prop",
            serde_json::json!([
                "firmware-revision",
                "hardware-revision",
                "model",
                "sku",
                "serial-number",
                "media-size",
                "auto-off-interval"
            ]),
        )
        .await
        .context("PixCut USB identity preflight failed")?;

        let mut state = self.pixcut_usb_state().await?;
        if state.0 == PrinterState::Sleep {
            self.request_json("resume-printer", serde_json::json!({}))
                .await
                .context("could not wake sleeping PixCut")?;
        }

        for _ in 0..20 {
            match state.0 {
                PrinterState::Idle => return Ok(()),
                PrinterState::Error | PrinterState::Off => {
                    bail!(
                        "PixCut is not ready ({:?} / {:?}): {}",
                        state.0,
                        state.1,
                        describe_pixcut_alerts(&state.2)
                    )
                }
                PrinterState::Processing => {
                    bail!("PixCut is busy ({:?} / {:?})", state.0, state.1)
                }
                PrinterState::Sleep | PrinterState::Initializing => {}
            }
            delay(Duration::from_millis(250)).await;
            state = self.pixcut_usb_state().await?;
        }
        bail!(
            "PixCut did not become idle before job submission ({:?} / {:?})",
            state.0,
            state.1
        )
    }

    async fn pixcut_usb_state(&self) -> anyhow::Result<(PrinterState, PrinterSubState, String)> {
        self.request_json(
            "get-prop",
            serde_json::json!(["printer-state", "printer-sub-state", "printer-state-alerts"]),
        )
        .await?
        .as_json::<AvocadoResult<(PrinterState, PrinterSubState, String)>>()
        .map(|result| result.result)
        .context("printer returned an invalid PixCut state response")
    }

    /// Fetch job status immediately after creation so the command/data phases
    /// match the sequencing used by working USB clients.
    pub async fn prime_pixcut_usb_job(&self, job_id: u32) -> anyhow::Result<()> {
        let packet = self
            .request_json("get-job-info", serde_json::json!({ "job-id": job_id }))
            .await
            .context("initial PixCut job-status request failed")?;
        if packet
            .as_json::<AvocadoResult<JobStatusResult>>()
            .and_then(|result| result.result.into_for_job(job_id))
            .is_none()
        {
            bail!("printer returned an invalid initial status for job {job_id}");
        }
        Ok(())
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
        let mut poll_count = 0usize;
        let mut missing_count = 0usize;

        let mut stream = interval(Duration::from_secs(1));
        while stream.next().await.is_some() {
            if event_tx.is_closed() {
                warn!("event sender was closed, ending job status stream");
                break;
            }

            poll_count += 1;
            validate_job_poll_budget(job_id, poll_count, missing_count)?;

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
            trace!(job_id, "got get-job-info response");

            if let Some(result) = packet.as_json::<AvocadoResult<JobStatusResult>>() {
                let Some(info) = result.result.into_for_job(job_id) else {
                    warn!("result was missing job info");
                    missing_count += 1;
                    validate_job_poll_budget(job_id, poll_count, missing_count)?;
                    continue;
                };
                missing_count = 0;
                debug!(
                    job_id = info.job_id,
                    state = ?info.job_state,
                    sub_state = ?info.job_sub_state,
                    reason = ?info.job_state_reason,
                    transfer_status = ?info.transfer_status,
                    transfer_size = ?info.transfer_size,
                    "got get-job-info status"
                );

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
                if let Some(error_code) = packet
                    .as_json::<serde_json::Value>()
                    .as_ref()
                    .and_then(find_device_error_code)
                {
                    bail!(
                        "printer rejected get-job-info for job {job_id} with error code {error_code}"
                    );
                }
                warn!(
                    job_id,
                    response_bytes = packet.data.len(),
                    "ignoring one undecodable job status response"
                );
                // Job-info telemetry is firmware-dependent and can be partial
                // immediately after upload. Treat one undecodable response the
                // same as a temporarily missing job, while retaining the
                // existing consecutive-miss and total-duration limits.
                missing_count += 1;
                validate_job_poll_budget(job_id, poll_count, missing_count)?;
                continue;
            }
        }

        Ok(())
    }

    /// Send one or more binary documents to the device for a given job.
    ///
    /// Each document restarts its chunk sequence. PixCut combo jobs depend on
    /// the PLT and JPEG remaining separate streams even though both use the
    /// same job id. USB uploads also receive a command-channel heartbeat while
    /// the background status poller is suppressed.
    #[instrument(skip(self, documents, f))]
    pub async fn send_data_streams<F>(
        &self,
        job_id: u32,
        documents: &[&[u8]],
        f: F,
    ) -> anyhow::Result<()>
    where
        F: Fn(usize, usize),
    {
        let Some(_guard) = SendingDropGuard::new(self.sending.clone()) else {
            bail!("cannot start sending data while other send is in progress");
        };
        let max_data_size = self.transport.lock().await.max_data_size().max(5);
        let payload_size = max_data_size - size_of::<u32>();
        let total = documents
            .iter()
            .map(|data| usize::div_ceil(data.len(), payload_size))
            .sum();
        debug!(
            documents = documents.len(),
            chunks = total,
            "sending job data"
        );
        let usb_profile = self.job_command_profile().await == JobCommandProfile::PixCutUsb;
        let mut sent = 0usize;
        let mut last_heartbeat = Instant::now();

        for (document_index, data) in documents.iter().enumerate() {
            let count = usize::div_ceil(data.len(), payload_size);
            let package_total =
                u16::try_from(count).context("document requires too many transport data chunks")?;
            for (index, chunk) in data.chunks(payload_size).enumerate() {
                if usb_profile && last_heartbeat.elapsed() >= DATA_HEARTBEAT_INTERVAL {
                    self.pixcut_usb_state()
                        .await
                        .context("PixCut upload heartbeat failed")?;
                    last_heartbeat = Instant::now();
                }

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
                    msg_package_total: package_total,
                    msg_package_num: u16::try_from(index + 1)
                        .context("transport data chunk index overflowed")?,
                    is_subpackage: count > 1,
                    data: buf,
                };
                trace!(
                    job_id,
                    document = document_index + 1,
                    frame = index + 1,
                    frames = count,
                    payload_bytes = chunk.len(),
                    "sending data packet"
                );
                self.transport
                    .lock()
                    .await
                    .send_packet(packet)
                    .await?
                    .await?;
                sent += 1;
                f(total, sent);
            }
        }

        Ok(())
    }
}

fn describe_pixcut_alerts(alerts: &str) -> Cow<'_, str> {
    if alerts
        .split(|character: char| !character.is_ascii_digit())
        .any(|code| code == "5414")
    {
        Cow::Borrowed(
            "paper feed/length fault (5414); check sheet orientation and feed path, then power-cycle the printer",
        )
    } else {
        Cow::Borrowed(alerts)
    }
}

#[cfg(not(target_arch = "wasm32"))]
async fn delay(duration: Duration) {
    tokio::time::sleep(duration).await;
}

#[cfg(target_arch = "wasm32")]
async fn delay(duration: Duration) {
    gloo_timers::future::TimeoutFuture::new(
        u32::try_from(duration.as_millis()).unwrap_or(u32::MAX),
    )
    .await;
}

fn validate_job_poll_budget(
    job_id: u32,
    poll_count: usize,
    missing_count: usize,
) -> anyhow::Result<()> {
    if poll_count > MAX_JOB_POLLS {
        bail!("job {job_id} did not reach a terminal state within 15 minutes");
    }
    if missing_count >= MAX_MISSING_JOB_POLLS {
        bail!(
            "printer returned no usable status for job {job_id} in {MAX_MISSING_JOB_POLLS} consecutive responses"
        );
    }
    Ok(())
}

fn find_device_error_code(value: &serde_json::Value) -> Option<i64> {
    match value {
        serde_json::Value::Object(object) => {
            for key in ["error-code", "error_code"] {
                if let Some(value) = object.get(key) {
                    let code = value
                        .as_i64()
                        .or_else(|| value.as_str().and_then(|value| value.parse().ok()));
                    if code.is_some_and(|code| code != 0) {
                        return code;
                    }
                }
            }
            object.values().find_map(find_device_error_code)
        }
        serde_json::Value::Array(values) => values.iter().find_map(find_device_error_code),
        _ => None,
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
    use std::sync::Arc;

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

    #[test]
    fn job_poll_budget_bounds_missing_and_nonterminal_statuses() {
        validate_job_poll_budget(7, MAX_JOB_POLLS, 0).unwrap();
        assert!(validate_job_poll_budget(7, MAX_JOB_POLLS + 1, 0).is_err());
        validate_job_poll_budget(7, 1, MAX_MISSING_JOB_POLLS - 1).unwrap();
        assert!(validate_job_poll_budget(7, 1, MAX_MISSING_JOB_POLLS).is_err());
    }

    #[test]
    fn explicit_device_error_codes_are_found_without_treating_zero_as_failure() {
        assert_eq!(
            find_device_error_code(&serde_json::json!({
                "result": [{"error-code": "8001"}]
            })),
            Some(8001)
        );
        assert_eq!(
            find_device_error_code(&serde_json::json!({
                "error-code": 0,
                "result": {"error_code": 0}
            })),
            None
        );
    }

    #[test]
    fn known_feed_fault_has_an_actionable_description() {
        assert_eq!(
            describe_pixcut_alerts("::5414"),
            "paper feed/length fault (5414); check sheet orientation and feed path, then power-cycle the printer"
        );
        assert_eq!(describe_pixcut_alerts("::0"), "::0");
    }

    #[tokio::test]
    async fn disconnect_before_ready_does_not_retain_manager() {
        let disconnected = Arc::new(AtomicBool::new(false));
        let event_disconnected = disconnected.clone();
        let manager = TransportManager::new(
            Rc::new(Mutex::new(Transport::MockTransport(
                MockTransport::default(),
            ))),
            move |event| {
                if matches!(
                    event,
                    TransportEvent::TransportStatus(TransportStatus::Disconnected)
                ) {
                    event_disconnected.store(true, std::sync::atomic::Ordering::SeqCst);
                }
            },
        );
        let weak = Arc::downgrade(&manager);

        for _ in 0..100 {
            if disconnected.load(std::sync::atomic::Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            disconnected.load(std::sync::atomic::Ordering::SeqCst),
            "mock transport should disconnect before ready"
        );
        drop(manager);

        for _ in 0..20 {
            if weak.upgrade().is_none() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("pre-ready disconnect retained the transport manager");
    }
}
