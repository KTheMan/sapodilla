use std::{borrow::Cow, collections::VecDeque, time::Duration};

use anyhow::{Context, bail};
use async_trait::async_trait;
use eframe::wasm_bindgen::{JsCast, JsValue};
use futures::{
    FutureExt, SinkExt, StreamExt,
    channel::{mpsc, oneshot},
};
use gloo_timers::future::TimeoutFuture;
use tracing::{debug, error, info, trace};
use wasm_bindgen_futures::JsFuture;
use web_sys::js_sys::{self, Array, Function, Promise, Reflect, Uint8Array};
use web_time::Instant;

use crate::{
    protocol::{AvocadoPacket, ContentType, EncodingType, EncryptionMode, InteractionType},
    raw_usb::{
        COMMAND_IN_ENDPOINT, COMMAND_INTERFACE, COMMAND_OUT_ENDPOINT, DATA_ACK_DELAY,
        DATA_IN_ENDPOINT, DATA_INTERFACE, DATA_OUT_ENDPOINT, JsonResponseDecoder,
        MAX_TRANSPORT_DATA_SIZE, PIXCUT_USB_PID, PIXCUT_USB_VID, encode_data_frame,
        validate_data_ack, write_slices,
    },
    transports::{
        JobCommandProfile, TransportControl, TransportEvent, TransportStatus,
        validate_webusb_response_budget,
    },
};

const RESPONSE_BUFFER_SIZE: u32 = 16 * 1024;
const READ_TIMEOUT: Duration = Duration::from_secs(15);
const COMMAND_RESPONSE_TIMEOUT: Duration = Duration::from_secs(15);
const WRITE_TIMEOUT: Duration = Duration::from_secs(20);
const CONTROL_TIMEOUT: Duration = Duration::from_secs(20);
const CLOSE_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug)]
enum TransportAction {
    SendPacket(AvocadoPacket, oneshot::Sender<()>),
    Disconnect,
}

/// Browser WebUSB transport for the PixCut vendor bulk interfaces.
///
/// WebUSB device access can only be requested in response to a user gesture,
/// so `start` opens a VID/PID-filtered browser picker exactly once. The app
/// calls `start` directly from its Connect action, matching Web Serial.
#[derive(Default)]
pub struct WebUsbTransport {
    tx: Option<mpsc::UnboundedSender<TransportAction>>,
}

#[async_trait(?Send)]
impl TransportControl for WebUsbTransport {
    fn name(&self) -> Cow<'static, str> {
        "WebUSB (bulk)".into()
    }

    fn supports_discovery(&self) -> bool {
        false
    }

    fn max_data_size(&self) -> usize {
        MAX_TRANSPORT_DATA_SIZE
    }

    fn job_command_profile(&self) -> JobCommandProfile {
        JobCommandProfile::PixCutUsb
    }

    async fn start(
        &mut self,
        event_tx: mpsc::UnboundedSender<TransportEvent>,
    ) -> anyhow::Result<()> {
        if self.tx.is_some() {
            bail!("WebUSB transport is already started");
        }
        event_tx
            .unbounded_send(TransportEvent::TransportStatus(TransportStatus::Connecting))
            .map_err(|_| anyhow::anyhow!("transport event receiver was dropped"))?;

        // This is deliberately the first awaited operation: requestDevice()
        // is invoked synchronously while the Connect user gesture is active.
        let device = request_pixcut_device().await.map_err(full_error)?;
        validate_pixcut_identity(&device).map_err(full_error)?;
        if let Err(error) = open_and_claim(&device).await {
            return Err(match close_device(&device).await {
                Ok(()) => full_error(error),
                Err(cleanup) => operation_and_cleanup_error(error, cleanup),
            });
        }

        let (action_tx, action_rx) = mpsc::unbounded();
        self.tx = Some(action_tx);
        wasm_bindgen_futures::spawn_local(run_usb(device, action_rx, event_tx));
        Ok(())
    }

    async fn disconnect(&mut self) -> anyhow::Result<()> {
        let mut tx = self.tx.take().context("transport was not started")?;
        tx.send(TransportAction::Disconnect)
            .await
            .map_err(|_| anyhow::anyhow!("WebUSB handler is no longer running"))
    }

    async fn send_packet(
        &mut self,
        packet: AvocadoPacket,
    ) -> anyhow::Result<oneshot::Receiver<()>> {
        let tx = self.tx.as_mut().context("transport was not started")?;
        let (completion_tx, completion_rx) = oneshot::channel();
        tx.send(TransportAction::SendPacket(packet, completion_tx))
            .await
            .map_err(|_| anyhow::anyhow!("WebUSB handler is no longer running"))?;
        Ok(completion_rx)
    }
}

async fn request_pixcut_device() -> anyhow::Result<JsValue> {
    let window = web_sys::window().context("WebUSB requires a browser window")?;
    let navigator = window.navigator();
    let usb = Reflect::get(&navigator, &JsValue::from_str("usb"))
        .map_err(|error| js_error("could not access navigator.usb", error))?;
    if usb.is_null() || usb.is_undefined() {
        bail!("this browser does not provide WebUSB");
    }

    let filter = js_sys::Object::new();
    set_property(
        &filter,
        "vendorId",
        JsValue::from_f64(PIXCUT_USB_VID.into()),
    )?;
    set_property(
        &filter,
        "productId",
        JsValue::from_f64(PIXCUT_USB_PID.into()),
    )?;
    let filters = Array::new();
    filters.push(&filter);
    let options = js_sys::Object::new();
    set_property(&options, "filters", filters.into())?;

    call_promise(&usb, "requestDevice", &[options.into()])
        .await
        .context("could not request PixCut WebUSB access")
}

fn validate_pixcut_identity(device: &JsValue) -> anyhow::Result<()> {
    let vendor = required_u16(device, "vendorId")?;
    let product = required_u16(device, "productId")?;
    if (vendor, product) != (PIXCUT_USB_VID, PIXCUT_USB_PID) {
        bail!("browser returned an unexpected USB device {vendor:04X}:{product:04X}");
    }
    Ok(())
}

async fn open_and_claim(device: &JsValue) -> anyhow::Result<()> {
    let opened = Reflect::get(device, &JsValue::from_str("opened"))
        .map_err(|error| js_error("could not inspect WebUSB device", error))?
        .as_bool()
        .unwrap_or(false);
    if !opened {
        call_promise_with_timeout(device, "open", &[], CONTROL_TIMEOUT)
            .await
            .context("could not open PixCut WebUSB device")?;
    }

    let configuration_one = configuration_with_value(device, 1)?;
    validate_endpoint_layout(&configuration_one)?;

    let active_configuration = Reflect::get(device, &JsValue::from_str("configuration"))
        .map_err(|error| js_error("could not inspect WebUSB configuration", error))?;
    if active_configuration.is_null() || active_configuration.is_undefined() {
        call_promise_with_timeout(
            device,
            "selectConfiguration",
            &[JsValue::from_f64(1.0)],
            CONTROL_TIMEOUT,
        )
        .await
        .context("could not select WebUSB configuration 1")?;
    } else if numeric_property(&active_configuration, "configurationValue") != Some(1) {
        bail!("PixCut WebUSB configuration 1 is not active");
    }

    call_promise_with_timeout(
        device,
        "claimInterface",
        &[JsValue::from_f64(COMMAND_INTERFACE.into())],
        CONTROL_TIMEOUT,
    )
    .await
    .context("could not claim PixCut command interface 2")?;

    call_promise_with_timeout(
        device,
        "claimInterface",
        &[JsValue::from_f64(DATA_INTERFACE.into())],
        CONTROL_TIMEOUT,
    )
    .await
    .context("could not claim PixCut data interface 3")?;

    Ok(())
}

fn validate_endpoint_layout(configuration: &JsValue) -> anyhow::Result<()> {
    validate_bulk_interface(
        configuration,
        COMMAND_INTERFACE,
        webusb_endpoint(COMMAND_OUT_ENDPOINT),
        webusb_endpoint(COMMAND_IN_ENDPOINT),
    )?;
    validate_bulk_interface(
        configuration,
        DATA_INTERFACE,
        webusb_endpoint(DATA_OUT_ENDPOINT),
        webusb_endpoint(DATA_IN_ENDPOINT),
    )
}

fn validate_bulk_interface(
    configuration: &JsValue,
    interface_number: u8,
    out_endpoint: u8,
    in_endpoint: u8,
) -> anyhow::Result<()> {
    let interfaces = Reflect::get(configuration, &JsValue::from_str("interfaces"))
        .map_err(|error| js_error("could not inspect WebUSB interfaces", error))?;
    let interfaces = Array::from(&interfaces);
    let interface = interfaces
        .iter()
        .find(|interface| {
            numeric_property(interface, "interfaceNumber") == Some(interface_number.into())
        })
        .with_context(|| format!("PixCut WebUSB interface {interface_number} is missing"))?;
    let alternate = Reflect::get(&interface, &JsValue::from_str("alternate"))
        .map_err(|error| js_error("could not inspect WebUSB alternate interface", error))?;
    if numeric_property(&alternate, "alternateSetting") != Some(0) {
        bail!("PixCut WebUSB interface {interface_number} does not use alternate setting 0");
    }
    if numeric_property(&alternate, "interfaceClass") != Some(0xff) {
        bail!("PixCut WebUSB interface {interface_number} is not vendor-specific class 0xFF");
    }
    let endpoints = Reflect::get(&alternate, &JsValue::from_str("endpoints"))
        .map_err(|error| js_error("could not inspect WebUSB endpoints", error))?;
    let endpoints = Array::from(&endpoints);
    if endpoints.length() != 2 {
        bail!(
            "PixCut WebUSB interface {interface_number} has {} endpoints; expected exactly 2",
            endpoints.length()
        );
    }

    for (direction, number) in [("out", out_endpoint), ("in", in_endpoint)] {
        let present = endpoints.iter().any(|endpoint| {
            numeric_property(&endpoint, "endpointNumber") == Some(number.into())
                && optional_string(&endpoint, "direction").as_deref() == Some(direction)
                && optional_string(&endpoint, "type").as_deref() == Some("bulk")
        });
        if !present {
            bail!(
                "PixCut WebUSB interface {interface_number} is missing bulk {direction} endpoint {number}"
            );
        }
    }
    Ok(())
}

fn configuration_with_value(device: &JsValue, expected: u8) -> anyhow::Result<JsValue> {
    let configurations = Reflect::get(device, &JsValue::from_str("configurations"))
        .map_err(|error| js_error("could not inspect WebUSB configurations", error))?;
    let configurations = Array::from(&configurations);
    configurations
        .iter()
        .find(|configuration| {
            numeric_property(configuration, "configurationValue") == Some(expected.into())
        })
        .with_context(|| format!("PixCut WebUSB configuration {expected} is missing"))
}

async fn run_usb(
    device: JsValue,
    mut action_rx: mpsc::UnboundedReceiver<TransportAction>,
    event_tx: mpsc::UnboundedSender<TransportEvent>,
) {
    if event_tx
        .unbounded_send(TransportEvent::TransportStatus(TransportStatus::Connected))
        .is_err()
    {
        if let Err(cleanup) = close_device(&device).await {
            error!("{}", full_error(cleanup));
        }
        return;
    }

    let mut decoder = JsonResponseDecoder::default();
    let mut pending_responses = VecDeque::new();
    let mut terminal_error = None;
    while let Some(action) = action_rx.next().await {
        let result = match action {
            TransportAction::Disconnect => break,
            TransportAction::SendPacket(packet, completion) => {
                let result = if packet.content_type == ContentType::Data {
                    send_data_packet(&device, &packet).await
                } else {
                    send_json_packet(
                        &device,
                        &packet,
                        &mut decoder,
                        &mut pending_responses,
                        &event_tx,
                    )
                    .await
                    .and_then(|response| {
                        event_tx
                            .unbounded_send(TransportEvent::Packet(response))
                            .map_err(|_| anyhow::anyhow!("transport event receiver was dropped"))
                    })
                };
                result.map(|()| completion)
            }
        };

        match result {
            Ok(completion) => {
                if completion.send(()).is_err() {
                    debug!("WebUSB completion receiver was dropped");
                }
            }
            Err(error) => {
                terminal_error = Some(error);
                break;
            }
        }
    }

    let cleanup_error = close_device(&device).await.err();
    let reported_error = match (terminal_error, cleanup_error) {
        (Some(operation), Some(cleanup)) => Some(operation_and_cleanup_error(operation, cleanup)),
        (Some(operation), None) => Some(full_error(operation)),
        (None, Some(cleanup)) => Some(full_error(cleanup)),
        (None, None) => None,
    };
    if let Some(error) = reported_error {
        let _ = event_tx.unbounded_send(TransportEvent::Error(error));
    }
    let _ = event_tx.unbounded_send(TransportEvent::TransportStatus(
        TransportStatus::Disconnected,
    ));
    info!("WebUSB handler stopped");
}

async fn send_json_packet(
    device: &JsValue,
    request: &AvocadoPacket,
    decoder: &mut JsonResponseDecoder,
    pending: &mut VecDeque<serde_json::Value>,
    event_tx: &mpsc::UnboundedSender<TransportEvent>,
) -> anyhow::Result<AvocadoPacket> {
    if request.encoding_type != EncodingType::Json {
        bail!("WebUSB command packet is not JSON encoded");
    }
    let mut frame = b"cmd json\n".to_vec();
    frame.extend_from_slice(&request.data);
    write_frame(device, webusb_endpoint(COMMAND_OUT_ENDPOINT), &frame).await?;
    read_matching_json_response(device, request, decoder, pending, event_tx).await
}

async fn read_matching_json_response(
    device: &JsValue,
    request: &AvocadoPacket,
    decoder: &mut JsonResponseDecoder,
    pending: &mut VecDeque<serde_json::Value>,
    event_tx: &mpsc::UnboundedSender<TransportEvent>,
) -> anyhow::Result<AvocadoPacket> {
    // `web_time::Instant` uses the browser's monotonic clock on wasm32;
    // `std::time::Instant::now()` panics on wasm32-unknown-unknown.
    let started = Instant::now();
    let mut reads = 0usize;
    let mut unmatched = 0usize;
    loop {
        if pending.is_empty() {
            let remaining = COMMAND_RESPONSE_TIMEOUT
                .checked_sub(started.elapsed())
                .context("WebUSB command response timed out")?;
            validate_webusb_response_budget(reads, unmatched)?;
            let response = read_response_with_timeout(
                device,
                webusb_endpoint(COMMAND_IN_ENDPOINT),
                remaining.min(READ_TIMEOUT),
            )
            .await?;
            reads += 1;
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
            unmatched += 1;
            validate_webusb_response_budget(reads, unmatched)?;
            event_tx
                .unbounded_send(TransportEvent::Packet(packet))
                .map_err(|_| anyhow::anyhow!("transport event receiver was dropped"))?;
        }
        // Some USB bridges resolve empty or stale reads immediately. Yield to
        // the browser task queue so that rendering and timeout callbacks still
        // run instead of forming an unbounded microtask loop.
        TimeoutFuture::new(0).await;
    }
}

fn json_response_packet(
    request: &AvocadoPacket,
    message_id: u32,
    value: serde_json::Value,
) -> anyhow::Result<AvocadoPacket> {
    let data = serde_json::to_vec(&value).context("could not preserve WebUSB response")?;
    trace!(bytes = data.len(), "received WebUSB JSON response");
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

async fn send_data_packet(device: &JsValue, request: &AvocadoPacket) -> anyhow::Result<()> {
    if request.data.len() < 4 {
        bail!("WebUSB data packet is missing its job id");
    }
    let job_id = u32::from_le_bytes(request.data[..4].try_into().expect("four-byte slice"));
    let frame = encode_data_frame(job_id, &request.data[4..])?;
    write_data_frame(device, webusb_endpoint(DATA_OUT_ENDPOINT), &frame).await?;
    let acknowledgement = read_response(device, webusb_endpoint(DATA_IN_ENDPOINT)).await?;
    validate_data_ack(&acknowledgement).context("WebUSB data endpoint rejected the frame")?;
    trace!(
        bytes = acknowledgement.len(),
        "received WebUSB data acknowledgement"
    );
    TimeoutFuture::new(DATA_ACK_DELAY.as_millis() as u32).await;
    Ok(())
}

/// Submit a complete `cmd data` frame in one `transferOut`, matching the
/// native USB transaction boundary used by the printer firmware.
async fn write_data_frame(device: &JsValue, endpoint: u8, frame: &[u8]) -> anyhow::Result<()> {
    let bytes = Uint8Array::new_from_slice(frame);
    let result = call_promise_with_timeout(
        device,
        "transferOut",
        &[JsValue::from_f64(endpoint.into()), bytes.into()],
        WRITE_TIMEOUT,
    )
    .await
    .with_context(|| format!("WebUSB data-frame write to endpoint {endpoint} failed"))?;
    require_transfer_ok(&result, "data write")?;
    let written = Reflect::get(&result, &JsValue::from_str("bytesWritten"))
        .map_err(|error| js_error("could not inspect WebUSB data write result", error))?
        .as_f64()
        .map(|value| value as usize)
        .context("WebUSB data write result omitted bytesWritten")?;
    if written != frame.len() {
        bail!(
            "WebUSB short data write: wrote {written} of {} bytes",
            frame.len()
        );
    }
    Ok(())
}

async fn write_frame(device: &JsValue, endpoint: u8, frame: &[u8]) -> anyhow::Result<()> {
    for slice in write_slices(frame) {
        let bytes = Uint8Array::new_from_slice(slice);
        let result = call_promise_with_timeout(
            device,
            "transferOut",
            &[JsValue::from_f64(endpoint.into()), bytes.into()],
            WRITE_TIMEOUT,
        )
        .await
        .with_context(|| format!("WebUSB bulk write to endpoint {endpoint} failed"))?;
        require_transfer_ok(&result, "write")?;
        let written = Reflect::get(&result, &JsValue::from_str("bytesWritten"))
            .map_err(|error| js_error("could not inspect WebUSB write result", error))?
            .as_f64()
            .map(|value| value as usize)
            .context("WebUSB write result omitted bytesWritten")?;
        if written != slice.len() {
            bail!(
                "WebUSB short write: wrote {written} of {} bytes",
                slice.len()
            );
        }
    }
    Ok(())
}

async fn read_response(device: &JsValue, endpoint: u8) -> anyhow::Result<Vec<u8>> {
    read_response_with_timeout(device, endpoint, READ_TIMEOUT).await
}

async fn read_response_with_timeout(
    device: &JsValue,
    endpoint: u8,
    timeout: Duration,
) -> anyhow::Result<Vec<u8>> {
    let result = call_promise_with_timeout(
        device,
        "transferIn",
        &[
            JsValue::from_f64(endpoint.into()),
            JsValue::from_f64(RESPONSE_BUFFER_SIZE.into()),
        ],
        timeout,
    )
    .await
    .with_context(|| format!("WebUSB bulk read from endpoint {endpoint} failed"))?;
    require_transfer_ok(&result, "read")?;
    let data = Reflect::get(&result, &JsValue::from_str("data"))
        .map_err(|error| js_error("could not inspect WebUSB read result", error))?;
    if data.is_null() || data.is_undefined() {
        bail!("WebUSB read completed without data");
    }
    data_view_bytes(data)
}

fn data_view_bytes(data: JsValue) -> anyhow::Result<Vec<u8>> {
    let buffer = Reflect::get(&data, &JsValue::from_str("buffer"))
        .map_err(|error| js_error("could not access WebUSB read buffer", error))?;
    let offset = Reflect::get(&data, &JsValue::from_str("byteOffset"))
        .map_err(|error| js_error("could not access WebUSB read offset", error))?
        .as_f64()
        .context("WebUSB read offset was not numeric")? as u32;
    let length = Reflect::get(&data, &JsValue::from_str("byteLength"))
        .map_err(|error| js_error("could not access WebUSB read length", error))?
        .as_f64()
        .context("WebUSB read length was not numeric")? as u32;
    let bytes = Uint8Array::new(&buffer).subarray(offset, offset.saturating_add(length));
    let mut result = vec![0; bytes.length() as usize];
    bytes.copy_to(&mut result);
    Ok(result)
}

fn require_transfer_ok(result: &JsValue, operation: &str) -> anyhow::Result<()> {
    let status = Reflect::get(result, &JsValue::from_str("status"))
        .map_err(|error| js_error("could not inspect WebUSB transfer status", error))?
        .as_string()
        .context("WebUSB transfer result omitted its status")?;
    if status != "ok" {
        bail!("WebUSB {operation} completed with status {status}");
    }
    Ok(())
}

async fn close_device(device: &JsValue) -> anyhow::Result<()> {
    let mut failures = Vec::new();
    for interface in [DATA_INTERFACE, COMMAND_INTERFACE] {
        if let Err(error) = call_promise_with_timeout(
            device,
            "releaseInterface",
            &[JsValue::from_f64(interface.into())],
            CLOSE_TIMEOUT,
        )
        .await
        {
            failures.push(format!(
                "release interface {interface} failed: {}",
                error_chain(&error)
            ));
        }
    }
    if let Err(error) = call_promise_with_timeout(device, "close", &[], CLOSE_TIMEOUT).await {
        failures.push(format!("close device failed: {}", error_chain(&error)));
    }
    if failures.is_empty() {
        Ok(())
    } else {
        bail!("WebUSB cleanup failed: {}", failures.join("; "))
    }
}

async fn call_promise(
    target: &JsValue,
    name: &str,
    arguments: &[JsValue],
) -> anyhow::Result<JsValue> {
    let function = Reflect::get(target, &JsValue::from_str(name))
        .map_err(|error| js_error(&format!("could not access WebUSB {name}"), error))?
        .dyn_into::<Function>()
        .map_err(|_| anyhow::anyhow!("WebUSB object does not provide {name}()"))?;
    let args = Array::new();
    for argument in arguments {
        args.push(argument);
    }
    let result = Reflect::apply(&function, target, &args)
        .map_err(|error| js_error(&format!("WebUSB {name}() failed"), error))?;
    let promise = result
        .dyn_into::<Promise>()
        .map_err(|_| anyhow::anyhow!("WebUSB {name}() did not return a Promise"))?;
    JsFuture::from(promise)
        .await
        .map_err(|error| js_error(&format!("WebUSB {name}() was rejected"), error))
}

async fn call_promise_with_timeout(
    target: &JsValue,
    name: &str,
    arguments: &[JsValue],
    timeout: Duration,
) -> anyhow::Result<JsValue> {
    let operation = call_promise(target, name, arguments).fuse();
    let timeout_ms = timeout.as_millis().min(u32::MAX.into()) as u32;
    let timer = TimeoutFuture::new(timeout_ms).fuse();
    futures::pin_mut!(operation, timer);
    futures::select! {
        result = operation => result,
        _ = timer => bail!("WebUSB {name}() timed out after {} seconds", timeout.as_secs()),
    }
}

fn set_property(target: &JsValue, name: &str, value: JsValue) -> anyhow::Result<()> {
    Reflect::set(target, &JsValue::from_str(name), &value)
        .map_err(|error| js_error(&format!("could not set WebUSB option {name}"), error))?
        .then_some(())
        .context("browser rejected a WebUSB request option")
}

fn required_u16(target: &JsValue, name: &str) -> anyhow::Result<u16> {
    let value = Reflect::get(target, &JsValue::from_str(name))
        .map_err(|error| js_error(&format!("could not inspect WebUSB {name}"), error))?
        .as_f64()
        .with_context(|| format!("WebUSB {name} was not numeric"))?;
    u16::try_from(value as u32).with_context(|| format!("WebUSB {name} was out of range"))
}

fn optional_string(target: &JsValue, name: &str) -> Option<String> {
    Reflect::get(target, &JsValue::from_str(name))
        .ok()
        .filter(|value| !value.is_null() && !value.is_undefined())
        .and_then(|value| value.as_string())
        .filter(|value| !value.trim().is_empty())
}

fn numeric_property(target: &JsValue, name: &str) -> Option<u32> {
    Reflect::get(target, &JsValue::from_str(name))
        .ok()?
        .as_f64()
        .map(|value| value as u32)
}

const fn webusb_endpoint(address: u8) -> u8 {
    address & 0x0f
}

fn js_error(context: &str, error: JsValue) -> anyhow::Error {
    let name = optional_string(&error, "name");
    let message = Reflect::get(&error, &JsValue::from_str("message"))
        .ok()
        .and_then(|value| value.as_string())
        .unwrap_or_else(|| format!("{error:?}"));
    match name {
        Some(name) => anyhow::anyhow!("{context}: {name}: {message}"),
        None => anyhow::anyhow!("{context}: {message}"),
    }
}

fn error_chain(error: &anyhow::Error) -> String {
    format!("{error:#}")
}

fn full_error(error: anyhow::Error) -> anyhow::Error {
    anyhow::anyhow!(error_chain(&error))
}

fn operation_and_cleanup_error(operation: anyhow::Error, cleanup: anyhow::Error) -> anyhow::Error {
    anyhow::anyhow!(
        "{}; cleanup also failed: {}",
        error_chain(&operation),
        error_chain(&cleanup)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_addresses_become_webusb_endpoint_numbers() {
        assert_eq!(webusb_endpoint(COMMAND_OUT_ENDPOINT), 6);
        assert_eq!(webusb_endpoint(COMMAND_IN_ENDPOINT), 6);
        assert_eq!(webusb_endpoint(DATA_OUT_ENDPOINT), 4);
        assert_eq!(webusb_endpoint(DATA_IN_ENDPOINT), 4);
    }

    #[test]
    fn raw_usb_transport_uses_sdk_data_frame_capacity() {
        let transport = WebUsbTransport::default();
        assert_eq!(transport.max_data_size(), 4_075);
        assert_eq!(
            transport.job_command_profile(),
            JobCommandProfile::PixCutUsb
        );
        let wrapped = crate::transports::Transport::WebUsbTransport(transport);
        assert_eq!(wrapped.max_data_size(), 4_075);
        assert_eq!(wrapped.job_command_profile(), JobCommandProfile::PixCutUsb);
    }

    #[test]
    fn flattened_errors_keep_the_full_source_chain() {
        let error = anyhow::anyhow!("DOMException root").context("claim interface 3");
        assert_eq!(
            full_error(error).to_string(),
            "claim interface 3: DOMException root"
        );
    }

    #[test]
    fn startup_errors_include_operation_and_cleanup_chains() {
        let operation = anyhow::anyhow!("claim root").context("claim interface 3");
        let cleanup = anyhow::anyhow!("close root").context("close device");
        assert_eq!(
            operation_and_cleanup_error(operation, cleanup).to_string(),
            "claim interface 3: claim root; cleanup also failed: close device: close root"
        );
    }
}
