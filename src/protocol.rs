use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};
use egui::Vec2;
use lazy_static::lazy_static;
use packed_struct::prelude::*;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{instrument, trace};

const WRAPPER: u8 = 0x7E;

lazy_static! {
    pub static ref DEVICES: Vec<Device> = vec![Device {
        name: "PixCut S1".to_string(),
        model: "DHP700".to_string(),
        dpi: 300.0,
        cutter_calibration: Some(CutterCalibration {
            scale_factor: 3.38667 * 1.01333,
            offset: Vec2::new(-9.0, -13.0),
        }),
        modes: vec![
            Mode {
                mode_type: ModeType::Print,
                canvas_sizes: vec![CanvasSize {
                    name: "4x6".to_string(),
                    media_size: 5012,
                    media_type: 2010,
                    size: Vec2::new(4.0 * 300.0, 6.0 * 300.0),
                    safe_area: Vec2::new(4.0 * 300.0, 6.0 * 300.0),
                }]
            },
            Mode {
                mode_type: ModeType::PrintAndCut,
                canvas_sizes: vec![CanvasSize {
                    name: "4x7".to_string(),
                    media_size: 5013,
                    media_type: 2030,
                    size: Vec2::new(4.0 * 300.0, 7.0 * 300.0),
                    safe_area: Vec2::new(3.62 * 300.0, 6.77 * 300.0),
                }]
            }
        ]
    }];
}

#[derive(Error, Debug)]
pub enum ProtocolError {
    #[error("reader error: {0}")]
    Reader(std::io::Error),
    #[error("invalid data for field: {0}")]
    InvalidData(&'static str),
}

#[derive(Debug, Clone, Serialize)]
pub struct AvocadoPacket {
    pub version: u8,
    pub content_type: ContentType,
    pub interaction_type: InteractionType,
    pub encoding_type: EncodingType,
    pub encryption_mode: EncryptionMode,
    pub terminal_id: u32,
    pub msg_number: u32,
    pub msg_package_total: u16,
    pub msg_package_num: u16,
    pub is_subpackage: bool,
    pub data: Vec<u8>,
}

impl AvocadoPacket {
    pub fn as_json<T>(&self) -> Option<T>
    where
        T: serde::de::DeserializeOwned,
    {
        if self.content_type == ContentType::Message
            && self.encryption_mode == EncryptionMode::None
            && self.encoding_type == EncodingType::Json
        {
            serde_json::from_slice(&self.data).ok()
        } else {
            None
        }
    }
}

#[derive(Debug)]
pub struct AvocadoFlags {
    pub length: u16,
    pub is_subpackage: bool,
    pub encryption_mode: EncryptionMode,
}

impl AvocadoFlags {
    pub fn unpack(flags: u16) -> Option<Self> {
        let is_subpackage = ((flags & 0b00100000_00000000) >> 13) > 0;
        let encryption_mode = EncryptionMode::from_primitive(
            u8::try_from((flags & 0b00011100_00000000) >> 10).unwrap(),
        )?;
        let length = flags & 0b00000011_11111111;

        Some(AvocadoFlags {
            length,
            is_subpackage,
            encryption_mode,
        })
    }
}

impl AvocadoPacket {
    #[instrument(skip_all)]
    pub fn read_one<R>(reader: &mut R) -> Result<Self, ProtocolError>
    where
        R: std::io::Read,
    {
        let prefix = reader.read_u8().map_err(ProtocolError::Reader)?;
        if prefix != WRAPPER {
            return Err(ProtocolError::InvalidData("prefix"));
        }

        let version = reader.read_u8().map_err(ProtocolError::Reader)?;
        let reserved = reader.read_u8().map_err(ProtocolError::Reader)?;

        let content_type: ContentType = Self::read_enum(reader, "content_type")?;
        trace!(?content_type);
        let interaction_type: InteractionType = Self::read_enum(reader, "interaction_type")?;
        trace!(?interaction_type);
        let encoding_type: EncodingType = Self::read_enum(reader, "encoding_type")?;
        trace!(?encoding_type);

        let terminal_id = reader
            .read_u32::<LittleEndian>()
            .map_err(ProtocolError::Reader)?;
        trace!(terminal_id);
        let msg_number = reader
            .read_u32::<LittleEndian>()
            .map_err(ProtocolError::Reader)?;
        trace!(msg_number);
        let msg_package_total = reader
            .read_u16::<LittleEndian>()
            .map_err(ProtocolError::Reader)?;
        trace!(msg_package_total);
        let msg_package_num = reader
            .read_u16::<LittleEndian>()
            .map_err(ProtocolError::Reader)?;
        trace!(msg_package_num);

        let packed_flags = reader
            .read_u16::<LittleEndian>()
            .map_err(ProtocolError::Reader)?;
        trace!("flags: {packed_flags:016b}");

        let flags =
            AvocadoFlags::unpack(packed_flags).ok_or(ProtocolError::InvalidData("flags"))?;
        trace!(?flags);

        let mut data = vec![0u8; usize::from(flags.length)];
        reader
            .read_exact(&mut data)
            .map_err(ProtocolError::Reader)?;
        trace!("data: {}", hex::encode(&data));

        let checksum = reader.read_u8().map_err(ProtocolError::Reader)?;
        let mut checksum_data = Vec::with_capacity(19 + data.len());
        checksum_data.extend([
            version,
            reserved,
            content_type.to_primitive(),
            interaction_type.to_primitive(),
            encoding_type.to_primitive(),
        ]);
        checksum_data.extend(terminal_id.to_le_bytes());
        checksum_data.extend(msg_number.to_le_bytes());
        checksum_data.extend(msg_package_total.to_le_bytes());
        checksum_data.extend(msg_package_num.to_le_bytes());
        checksum_data.extend(packed_flags.to_le_bytes());
        checksum_data.extend(&data);
        if checksum != Self::checksum(&checksum_data) {
            return Err(ProtocolError::InvalidData("checksum"));
        }

        let suffix = reader.read_u8().map_err(ProtocolError::Reader)?;
        if suffix != WRAPPER {
            return Err(ProtocolError::InvalidData("suffix"));
        }

        Ok(Self {
            version,
            content_type,
            interaction_type,
            encoding_type,
            encryption_mode: flags.encryption_mode,
            terminal_id,
            msg_number,
            msg_package_total,
            msg_package_num,
            is_subpackage: flags.is_subpackage,
            data,
        })
    }

    #[instrument(skip_all)]
    pub fn encode(&self) -> Vec<u8> {
        assert!(
            self.data.len() <= 0x03ff,
            "Avocado packet payload exceeds the 10-bit length field"
        );
        let mut buf = Vec::with_capacity(self.data.len() + 22);

        buf.push(WRAPPER);
        buf.push(self.version);
        buf.push(0); // reserved
        buf.push(self.content_type.to_primitive());
        buf.push(self.interaction_type.to_primitive());
        buf.push(self.encoding_type.to_primitive());
        buf.write_u32::<LittleEndian>(self.terminal_id).unwrap();
        buf.write_u32::<LittleEndian>(self.msg_number).unwrap();
        buf.write_u16::<LittleEndian>(self.msg_package_total)
            .unwrap();
        buf.write_u16::<LittleEndian>(self.msg_package_num).unwrap();
        let mut flags = 0u16;
        if self.is_subpackage {
            flags |= 1 << 13
        }
        flags |= u16::from(self.encryption_mode.to_primitive()) << 10;
        flags |= self.data.len() as u16 & 0b00000011_11111111;
        buf.write_u16::<LittleEndian>(flags).unwrap();
        buf.extend_from_slice(&self.data);
        buf.push(Self::checksum(&buf[1..]));
        buf.push(WRAPPER);

        buf
    }

    fn checksum(data: &[u8]) -> u8 {
        data.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte))
    }

    fn read_enum<R, E>(reader: &mut R, name: &'static str) -> Result<E, ProtocolError>
    where
        R: std::io::Read,
        E: PrimitiveEnum<Primitive = u8>,
    {
        PrimitiveEnum::from_primitive(reader.read_u8().map_err(ProtocolError::Reader)?)
            .ok_or(ProtocolError::InvalidData(name))
    }
}

pub struct AvocadoPacketReader<R> {
    reader: std::io::BufReader<R>,
    finished: bool,
}

impl<R: std::io::Read> AvocadoPacketReader<R> {
    pub fn new(reader: R) -> Self {
        Self {
            reader: std::io::BufReader::new(reader),
            finished: false,
        }
    }
}

impl<R> Iterator for AvocadoPacketReader<R>
where
    R: std::io::Read,
{
    type Item = Result<AvocadoPacket, ProtocolError>;

    fn next(&mut self) -> Option<Self::Item> {
        use std::io::BufRead as _;

        if self.finished {
            return None;
        }
        match self.reader.fill_buf() {
            Ok([]) => {
                self.finished = true;
                None
            }
            Ok(_) => match AvocadoPacket::read_one(&mut self.reader) {
                Ok(packet) => Some(Ok(packet)),
                Err(err) => {
                    // Do not turn a truncated frame into a clean end-of-stream,
                    // and do not repeatedly yield the same I/O failure.
                    self.finished = true;
                    Some(Err(err))
                }
            },
            Err(err) => {
                self.finished = true;
                Some(Err(ProtocolError::Reader(err)))
            }
        }
    }
}

#[derive(PrimitiveEnum_u8, Clone, Copy, Debug, PartialEq, Hash, Serialize)]
pub enum ContentType {
    Message = 1,
    Data = 2,
}

impl std::fmt::Display for ContentType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Message => write!(f, "message"),
            Self::Data => write!(f, "data"),
        }
    }
}

#[derive(PrimitiveEnum_u8, Clone, Copy, Debug, PartialEq, Hash, Serialize)]
pub enum InteractionType {
    Request = 6,
    Response = 7,
}

impl std::fmt::Display for InteractionType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Request => write!(f, "request"),
            Self::Response => write!(f, "response"),
        }
    }
}

#[derive(PrimitiveEnum_u8, Clone, Copy, Debug, PartialEq, Hash, Serialize)]
pub enum EncodingType {
    Hexadecimal = 2,
    Json = 3,
}

impl std::fmt::Display for EncodingType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Hexadecimal => write!(f, "hexadecimal"),
            Self::Json => write!(f, "json"),
        }
    }
}

#[derive(PrimitiveEnum_u8, Clone, Copy, Debug, PartialEq, Hash, Serialize)]
pub enum EncryptionMode {
    None = 0b000,
    RC4 = 0b010,
}

impl std::fmt::Display for EncryptionMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::None => write!(f, "none"),
            Self::RC4 => write!(f, "RC4"),
        }
    }
}

#[derive(PrimitiveEnum_u8, Clone, Copy, Debug, PartialEq, Hash, Serialize)]
pub enum JobState {
    Waiting = 1,
    Start = 2,
    Processing = 3,
    ProcessingHeld = 4,
    Pending = 5,
    Terminating = 6,
    Aborted = 7,
    Cancelled = 8,
    Completed = 9,
}

#[derive(PrimitiveEnum_u16, Clone, Copy, Debug, PartialEq, Hash, Serialize)]
pub enum JobSubState {
    WaitingNone = 1000,
    StartNone = 2000,
    ProcessingNone = 3000,
    ProcessingPrintingDataDownloading = 3001,
    ProcessingPrintingDataUploading = 3002,
    ProcessingPrintingDataCloudRendering = 3003,
    ProcessingPrintingDataLocalRendering = 3004,
    ProcessingPrinting = 3005,
    ProcessingHeldNone = 4000,
    PendingNone = 5000,
    TerminatingNone = 6000,
    AbortedNone = 7000,
    CancelledNone = 8000,
    CompletedNone = 9000,
}

#[derive(PrimitiveEnum_u8, Clone, Copy, Debug, PartialEq, Hash, Serialize)]
pub enum PrinterState {
    Initializing = 10,
    Idle = 20,
    Sleep = 30,
    Processing = 40,
    Off = 50,
    Error = 60,
}

#[derive(PrimitiveEnum_u16, Clone, Copy, Debug, PartialEq, Hash, Serialize)]
pub enum PrinterSubState {
    InitNone = 1000,
    IdleNone = 2000,
    Printing = 3001,
    FileTransferring = 3002,
    Cancelling = 3006,
    Upgrading = 3007,
    Calibrating = 3008,
    SemiAutoPrinting = 3009,
    SemiAutoScanRequired = 3010,
    SemiAutoScanning = 3011,
    ScanWaiting = 3012,
    CopyWaiting = 3013,
    Rendering = 3014,
    Initializing = 3015,
    Decoding = 3016,
    LoadingPaper = 3017,
    PrintingYellow = 3018,
    PrintingMagenta = 3019,
    PrintingCyan = 3020,
    PrintingOC = 3021,
    Preheating = 3022,
    Cooldown = 3023,
    Cleaning = 3024,
    HomeFeed = 3025,
    EjectingPaper = 3026,
    SmartSheet = 3027,
    CutPick = 3028,
    CutHome = 3029,
    Cutting = 3030,
    CutEject = 3031,
    Normal = 4002,
    NotRealOff = 5002,
    ErrorNone = 6000,
}

macro_rules! impl_de_str_primitive {
    ($t:ty) => {
        impl<'de> serde::Deserialize<'de> for $t {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                #[derive(Deserialize)]
                #[serde(untagged)]
                enum StrOrPrimitive {
                    Str(String),
                    Primitive(<$t as PrimitiveEnum>::Primitive),
                }

                let val = match StrOrPrimitive::deserialize(deserializer)? {
                    StrOrPrimitive::Str(s) => s
                        .parse()
                        .map_err(|_| serde::de::Error::custom("value was not primitive"))?,
                    StrOrPrimitive::Primitive(val) => val,
                };

                <$t>::from_primitive(val)
                    .ok_or_else(|| serde::de::Error::custom("value was not valid for primitive"))
            }
        }
    };
}

impl_de_str_primitive!(JobState);
impl_de_str_primitive!(JobSubState);
impl_de_str_primitive!(PrinterState);
impl_de_str_primitive!(PrinterSubState);

#[derive(Debug, Deserialize)]
pub struct AvocadoId {
    pub id: u32,
}

#[derive(Debug, Deserialize)]
pub struct AvocadoResult<T> {
    pub result: T,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrinterIdentityInfo {
    pub model: String,
    pub serial_number: Option<String>,
    pub firmware_revision: String,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum PrinterIdentityResultShape {
    Ordered((String, Option<String>, String)),
    Named {
        model: String,
        #[serde(default, rename = "serial-number", alias = "serial_number")]
        serial_number: Option<String>,
        #[serde(rename = "firmware-revision", alias = "firmware_revision")]
        firmware_revision: String,
    },
}

/// Decode the response to a `get-prop` request whose parameters were ordered
/// as model, serial-number, and firmware-revision.
pub fn decode_printer_identity(packet: &AvocadoPacket) -> Option<PrinterIdentityInfo> {
    let result = packet.as_json::<AvocadoResult<PrinterIdentityResultShape>>()?;
    let (model, serial_number, firmware_revision) = match result.result {
        PrinterIdentityResultShape::Ordered((model, serial_number, firmware_revision)) => {
            (model, serial_number, firmware_revision)
        }
        PrinterIdentityResultShape::Named {
            model,
            serial_number,
            firmware_revision,
        } => (model, serial_number, firmware_revision),
    };
    let model = model.trim().to_owned();
    let firmware_revision = firmware_revision.trim().to_owned();
    let serial_number = serial_number
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty());
    (!model.is_empty() && !firmware_revision.is_empty()).then_some(PrinterIdentityInfo {
        model,
        serial_number,
        firmware_revision,
    })
}

/// Firmware revisions have returned `get-job-info.result` as either a status
/// object or a one-element array. Keep that wire variation out of queue logic.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum JobStatusResult {
    One(JobStatusInfo),
    Many(Vec<JobStatusInfo>),
}

impl JobStatusResult {
    pub fn into_for_job(self, job_id: u32) -> Option<JobStatusInfo> {
        match self {
            Self::One(info) => (info.job_id == job_id).then_some(info),
            Self::Many(infos) => infos.into_iter().find(|info| info.job_id == job_id),
        }
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct JobStatusInfo {
    #[serde(alias = "job_id")]
    pub job_id: u32,
    #[serde(alias = "job_state")]
    pub job_state: JobState,
    #[serde(alias = "job_sub_state")]
    pub job_sub_state: JobSubState,
    #[serde(default)]
    pub copies: Option<u8>,
    #[serde(default, alias = "printing_page_number")]
    pub printing_page_number: Option<u8>,
    #[serde(default, alias = "user_account")]
    pub user_account: Option<String>,
    #[serde(default)]
    pub channel: Option<u32>,
    #[serde(default, alias = "media_size")]
    pub media_size: Option<u32>,
    #[serde(default, alias = "media_type")]
    pub media_type: Option<u32>,
    #[serde(default, alias = "job_type")]
    pub job_type: Option<u32>,
    #[serde(default, alias = "document_format")]
    pub document_format: Option<u32>,
    #[serde(default, alias = "file_size")]
    pub file_size: Option<u32>,
    #[serde(default, alias = "transfer_status")]
    pub transfer_status: Option<u32>,
    #[serde(default, alias = "transfer_size")]
    pub transfer_size: Option<u32>,
    #[serde(default, alias = "job_state_reason")]
    pub job_state_reason: Option<serde_json::Value>,
    #[serde(default, alias = "cutting_progress")]
    pub cutting_progress: Option<serde_json::Value>,
    #[serde(default, alias = "cut_contours")]
    pub cut_contours: Option<serde_json::Value>,
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct Device {
    pub name: String,
    pub model: String,
    pub dpi: f32,
    pub cutter_calibration: Option<CutterCalibration>,
    pub modes: Vec<Mode>,
}

#[derive(Debug, Clone)]
pub struct CutterCalibration {
    pub scale_factor: f32,
    pub offset: Vec2,
}

impl Default for CutterCalibration {
    fn default() -> Self {
        Self {
            scale_factor: 1.0,
            offset: Vec2::ZERO,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum ModeType {
    Print,
    PrintAndCut,
}

impl ModeType {
    pub fn name(&self) -> &'static str {
        match self {
            ModeType::Print => "Print",
            ModeType::PrintAndCut => "Print and Cut",
        }
    }

    pub fn channel(&self) -> u16 {
        match self {
            ModeType::Print => 30784,
            ModeType::PrintAndCut => 30960,
        }
    }

    pub fn job_type(&self) -> u16 {
        match self {
            ModeType::Print => 0,
            ModeType::PrintAndCut => 600,
        }
    }

    pub fn link_type(&self) -> u16 {
        match self {
            ModeType::Print => 1000,
            ModeType::PrintAndCut => 0,
        }
    }

    pub fn has_cutting(&self) -> bool {
        matches!(self, ModeType::PrintAndCut)
    }
}

#[derive(Debug, Clone)]
pub struct Mode {
    pub mode_type: ModeType,
    pub canvas_sizes: Vec<CanvasSize>,
}

#[derive(Debug, Clone)]
pub struct CanvasSize {
    pub name: String,
    pub media_size: u16,
    pub media_type: u16,
    pub size: Vec2,
    pub safe_area: Vec2,
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    const JSON_REQUEST_DATA: &[u8] = &[
        0x7E, 0x64, 0x00, 0x01, 0x06, 0x03, 0x74, 0x02, 0x00, 0x00, 0x74, 0x02, 0x00, 0x00, 0x01,
        0x00, 0x01, 0x00, 0x69, 0x00, 0x7B, 0x0A, 0x20, 0x20, 0x22, 0x69, 0x64, 0x22, 0x20, 0x3A,
        0x20, 0x36, 0x32, 0x38, 0x2C, 0x0A, 0x20, 0x20, 0x22, 0x6D, 0x65, 0x74, 0x68, 0x6F, 0x64,
        0x22, 0x20, 0x3A, 0x20, 0x22, 0x67, 0x65, 0x74, 0x2D, 0x70, 0x72, 0x6F, 0x70, 0x22, 0x2C,
        0x0A, 0x20, 0x20, 0x22, 0x70, 0x61, 0x72, 0x61, 0x6D, 0x73, 0x22, 0x20, 0x3A, 0x20, 0x5B,
        0x0A, 0x20, 0x20, 0x20, 0x20, 0x22, 0x66, 0x69, 0x72, 0x6D, 0x77, 0x61, 0x72, 0x65, 0x2D,
        0x72, 0x65, 0x76, 0x69, 0x73, 0x69, 0x6F, 0x6E, 0x22, 0x2C, 0x0A, 0x20, 0x20, 0x20, 0x20,
        0x22, 0x62, 0x74, 0x2D, 0x70, 0x68, 0x6F, 0x6E, 0x65, 0x2D, 0x6D, 0x61, 0x63, 0x22, 0x0A,
        0x20, 0x20, 0x5D, 0x0A, 0x7D, 0x59, 0x7E,
    ];

    #[test]
    fn test_read_one() {
        let mut cursor = Cursor::new(JSON_REQUEST_DATA);

        let packet = AvocadoPacket::read_one(&mut cursor);
        assert!(packet.is_ok());
    }

    #[test]
    fn read_one_rejects_corrupted_checksum() {
        let mut data = JSON_REQUEST_DATA.to_vec();
        let checksum = data.len() - 2;
        data[checksum] ^= 0x01;
        let error = AvocadoPacket::read_one(&mut Cursor::new(data)).unwrap_err();
        assert!(matches!(error, ProtocolError::InvalidData("checksum")));
    }

    #[test]
    fn packet_reader_reports_a_truncated_frame_once() {
        let truncated = &JSON_REQUEST_DATA[..JSON_REQUEST_DATA.len() - 3];
        let mut packets = AvocadoPacketReader::new(Cursor::new(truncated));
        assert!(matches!(
            packets.next(),
            Some(Err(ProtocolError::Reader(error)))
                if error.kind() == std::io::ErrorKind::UnexpectedEof
        ));
        assert!(packets.next().is_none());
        assert!(
            AvocadoPacketReader::new(Cursor::new(Vec::<u8>::new()))
                .next()
                .is_none()
        );
    }

    #[test]
    fn job_status_accepts_snake_case_and_missing_optional_diagnostics() {
        let status: JobStatusInfo = serde_json::from_value(serde_json::json!({
            "job_id": 7,
            "job_state": 9,
            "job_sub_state": 9000,
            "cutting_progress": 75
        }))
        .unwrap();
        assert_eq!(status.job_id, 7);
        assert_eq!(status.cutting_progress, Some(serde_json::json!(75)));
        assert_eq!(status.file_size, None);
    }

    #[test]
    fn job_status_result_accepts_object_and_array_firmware_shapes() {
        for value in [
            serde_json::json!({
                "result": {
                    "job-id": 7,
                    "job-state": "9",
                    "job-sub-state": "9000"
                }
            }),
            serde_json::json!({
                "result": [{
                    "job_id": 8,
                    "job_state": 9,
                    "job_sub_state": 9000,
                    "printing_page_number": 2,
                    "media_size": 5013,
                    "document_format": 1,
                    "transfer_status": 100,
                    "transfer_size": 2048
                }]
            }),
        ] {
            let response: AvocadoResult<JobStatusResult> =
                serde_json::from_value(value).expect("valid firmware response shape");
            let expected_id = match &response.result {
                JobStatusResult::One(_) => 7,
                JobStatusResult::Many(_) => 8,
            };
            let status = response
                .result
                .into_for_job(expected_id)
                .expect("requested status");
            assert_eq!(status.job_state, JobState::Completed);
            assert_eq!(status.job_sub_state, JobSubState::CompletedNone);
            if status.job_id == 8 {
                assert_eq!(status.printing_page_number, Some(2));
                assert_eq!(status.media_size, Some(5013));
                assert_eq!(status.document_format, Some(1));
                assert_eq!(status.transfer_status, Some(100));
                assert_eq!(status.transfer_size, Some(2048));
            }
        }

        let response: AvocadoResult<JobStatusResult> =
            serde_json::from_value(serde_json::json!({ "result": [] }))
                .expect("empty arrays are a valid, temporarily missing result");
        assert!(response.result.into_for_job(9).is_none());
    }

    #[test]
    fn printer_identity_accepts_ordered_and_named_firmware_shapes() {
        let packet = |result: serde_json::Value| AvocadoPacket {
            version: 100,
            content_type: ContentType::Message,
            interaction_type: InteractionType::Response,
            encoding_type: EncodingType::Json,
            encryption_mode: EncryptionMode::None,
            terminal_id: 7,
            msg_number: 7,
            msg_package_total: 1,
            msg_package_num: 1,
            is_subpackage: false,
            data: serde_json::to_vec(&serde_json::json!({ "id": 7, "result": result })).unwrap(),
        };

        assert_eq!(
            decode_printer_identity(&packet(serde_json::json!(["DHP700", "SN-1", "1.2.3"]))),
            Some(PrinterIdentityInfo {
                model: "DHP700".into(),
                serial_number: Some("SN-1".into()),
                firmware_revision: "1.2.3".into(),
            })
        );
        assert_eq!(
            decode_printer_identity(&packet(serde_json::json!({
                "model": "DHP700",
                "serial-number": "SN-2",
                "firmware-revision": "2.0"
            }))),
            Some(PrinterIdentityInfo {
                model: "DHP700".into(),
                serial_number: Some("SN-2".into()),
                firmware_revision: "2.0".into(),
            })
        );
    }

    #[test]
    fn printer_identity_requires_model_and_firmware_but_allows_missing_serial() {
        let make = |result: serde_json::Value| AvocadoPacket {
            version: 100,
            content_type: ContentType::Message,
            interaction_type: InteractionType::Response,
            encoding_type: EncodingType::Json,
            encryption_mode: EncryptionMode::None,
            terminal_id: 8,
            msg_number: 8,
            msg_package_total: 1,
            msg_package_num: 1,
            is_subpackage: false,
            data: serde_json::to_vec(&serde_json::json!({ "id": 8, "result": result })).unwrap(),
        };
        assert_eq!(
            decode_printer_identity(&make(serde_json::json!(["DHP700", null, "2.0"]))),
            Some(PrinterIdentityInfo {
                model: "DHP700".into(),
                serial_number: None,
                firmware_revision: "2.0".into(),
            })
        );
        assert!(decode_printer_identity(&make(serde_json::json!(["", "SN", "2.0"]))).is_none());
    }

    #[test]
    fn test_encode() {
        let packet = AvocadoPacket {
            version: 100,
            content_type: ContentType::Message,
            interaction_type: InteractionType::Request,
            encoding_type: EncodingType::Json,
            encryption_mode: EncryptionMode::None,
            terminal_id: 628,
            msg_number: 628,
            msg_package_total: 1,
            msg_package_num: 1,
            is_subpackage: false,
            data: vec![
                0x7B, 0x0A, 0x20, 0x20, 0x22, 0x69, 0x64, 0x22, 0x20, 0x3A, 0x20, 0x36, 0x32, 0x38,
                0x2C, 0x0A, 0x20, 0x20, 0x22, 0x6D, 0x65, 0x74, 0x68, 0x6F, 0x64, 0x22, 0x20, 0x3A,
                0x20, 0x22, 0x67, 0x65, 0x74, 0x2D, 0x70, 0x72, 0x6F, 0x70, 0x22, 0x2C, 0x0A, 0x20,
                0x20, 0x22, 0x70, 0x61, 0x72, 0x61, 0x6D, 0x73, 0x22, 0x20, 0x3A, 0x20, 0x5B, 0x0A,
                0x20, 0x20, 0x20, 0x20, 0x22, 0x66, 0x69, 0x72, 0x6D, 0x77, 0x61, 0x72, 0x65, 0x2D,
                0x72, 0x65, 0x76, 0x69, 0x73, 0x69, 0x6F, 0x6E, 0x22, 0x2C, 0x0A, 0x20, 0x20, 0x20,
                0x20, 0x22, 0x62, 0x74, 0x2D, 0x70, 0x68, 0x6F, 0x6E, 0x65, 0x2D, 0x6D, 0x61, 0x63,
                0x22, 0x0A, 0x20, 0x20, 0x5D, 0x0A, 0x7D,
            ],
        };
        assert_eq!(
            packet.encode(),
            [
                0x7E, 0x64, 0x00, 0x01, 0x06, 0x03, 0x74, 0x02, 0x00, 0x00, 0x74, 0x02, 0x00, 0x00,
                0x01, 0x00, 0x01, 0x00, 0x69, 0x00, 0x7B, 0x0A, 0x20, 0x20, 0x22, 0x69, 0x64, 0x22,
                0x20, 0x3A, 0x20, 0x36, 0x32, 0x38, 0x2C, 0x0A, 0x20, 0x20, 0x22, 0x6D, 0x65, 0x74,
                0x68, 0x6F, 0x64, 0x22, 0x20, 0x3A, 0x20, 0x22, 0x67, 0x65, 0x74, 0x2D, 0x70, 0x72,
                0x6F, 0x70, 0x22, 0x2C, 0x0A, 0x20, 0x20, 0x22, 0x70, 0x61, 0x72, 0x61, 0x6D, 0x73,
                0x22, 0x20, 0x3A, 0x20, 0x5B, 0x0A, 0x20, 0x20, 0x20, 0x20, 0x22, 0x66, 0x69, 0x72,
                0x6D, 0x77, 0x61, 0x72, 0x65, 0x2D, 0x72, 0x65, 0x76, 0x69, 0x73, 0x69, 0x6F, 0x6E,
                0x22, 0x2C, 0x0A, 0x20, 0x20, 0x20, 0x20, 0x22, 0x62, 0x74, 0x2D, 0x70, 0x68, 0x6F,
                0x6E, 0x65, 0x2D, 0x6D, 0x61, 0x63, 0x22, 0x0A, 0x20, 0x20, 0x5D, 0x0A, 0x7D, 0x59,
                0x7E,
            ]
        );
    }

    #[test]
    fn encode_preserves_version_and_round_trips_packet() {
        let packet = AvocadoPacket {
            version: 101,
            content_type: ContentType::Data,
            interaction_type: InteractionType::Response,
            encoding_type: EncodingType::Hexadecimal,
            encryption_mode: EncryptionMode::RC4,
            terminal_id: 0x0102_0304,
            msg_number: 99,
            msg_package_total: 3,
            msg_package_num: 2,
            is_subpackage: true,
            data: vec![0x00, 0x7e, 0xff],
        };

        let encoded = packet.encode();
        let decoded = AvocadoPacket::read_one(&mut Cursor::new(encoded)).unwrap();
        assert_eq!(decoded.version, 101);
        assert_eq!(decoded.content_type, ContentType::Data);
        assert_eq!(decoded.interaction_type, InteractionType::Response);
        assert_eq!(decoded.encoding_type, EncodingType::Hexadecimal);
        assert_eq!(decoded.encryption_mode, EncryptionMode::RC4);
        assert_eq!(decoded.terminal_id, 0x0102_0304);
        assert_eq!(decoded.msg_number, 99);
        assert_eq!(decoded.msg_package_total, 3);
        assert_eq!(decoded.msg_package_num, 2);
        assert!(decoded.is_subpackage);
        assert_eq!(decoded.data, vec![0x00, 0x7e, 0xff]);
    }

    #[test]
    #[should_panic(expected = "payload exceeds the 10-bit length field")]
    fn encode_rejects_payload_that_cannot_be_represented() {
        let packet = AvocadoPacket {
            version: 100,
            content_type: ContentType::Data,
            interaction_type: InteractionType::Request,
            encoding_type: EncodingType::Hexadecimal,
            encryption_mode: EncryptionMode::None,
            terminal_id: 1,
            msg_number: 1,
            msg_package_total: 1,
            msg_package_num: 1,
            is_subpackage: false,
            data: vec![0; 1024],
        };
        let _ = packet.encode();
    }

    #[derive(Clone, Copy, Debug)]
    struct FixturePoint {
        x: f64,
        y: f64,
    }

    fn parse_plt_paths(plt: &str) -> Vec<Vec<FixturePoint>> {
        let mut paths = Vec::<Vec<FixturePoint>>::new();
        let mut current = None;
        for token in plt.split_ascii_whitespace() {
            let Some(kind) = token.as_bytes().first().copied() else {
                continue;
            };
            if !matches!(kind, b'U' | b'D') {
                continue;
            }
            let Some((y, x)) = token[1..].split_once(',') else {
                continue;
            };
            let point = FixturePoint {
                x: x.parse().unwrap(),
                y: y.parse().unwrap(),
            };
            if kind == b'U' {
                current = Some(paths.len());
                paths.push(vec![point]);
            } else if let Some(index) = current {
                let path = &mut paths[index];
                let repeats_seating_point =
                    path.len() == 1 && path[0].x == point.x && path[0].y == point.y;
                if !repeats_seating_point {
                    path.push(point);
                }
            }
        }
        // A final blade-up-only group is the park position, not a cut path.
        if paths.last().is_some_and(|path| path.len() == 1) {
            paths.pop();
        }
        paths
    }

    fn point_segment_distance(point: FixturePoint, a: FixturePoint, b: FixturePoint) -> f64 {
        let dx = b.x - a.x;
        let dy = b.y - a.y;
        let length_squared = dx * dx + dy * dy;
        if length_squared == 0.0 {
            return (point.x - a.x).hypot(point.y - a.y);
        }
        let projection =
            (((point.x - a.x) * dx + (point.y - a.y) * dy) / length_squared).clamp(0.0, 1.0);
        (point.x - (a.x + projection * dx)).hypot(point.y - (a.y + projection * dy))
    }

    fn directed_path_deviation(from: &[Vec<FixturePoint>], to: &[Vec<FixturePoint>]) -> f64 {
        let segments: Vec<_> = to
            .iter()
            .flat_map(|path| path.windows(2).map(|pair| (pair[0], pair[1])))
            .collect();
        from.iter()
            .flatten()
            .map(|point| {
                segments
                    .iter()
                    .map(|(a, b)| point_segment_distance(*point, *a, *b))
                    .fold(f64::INFINITY, f64::min)
            })
            .fold(0.0, f64::max)
    }

    fn symmetric_hausdorff(a: &[Vec<FixturePoint>], b: &[Vec<FixturePoint>]) -> f64 {
        directed_path_deviation(a, b).max(directed_path_deviation(b, a))
    }

    #[test]
    fn oracle_plt_fixture_and_hausdorff_thresholds_are_repeatable() {
        // Captured fixture provenance and its MIT license are recorded in
        // THIRD_PARTY_NOTICES.md and beside this fixture.
        const SQUARE: &str = include_str!("../tests/fixtures/honeymaro-pixcut/square-exact.plt");
        const OVERCUT: &str = include_str!("../tests/fixtures/honeymaro-pixcut/square-overcut.plt");
        assert_eq!(
            SQUARE,
            "IN VER0.1.0 KP1 U2029,1016 D2029,1016 D5077,1016 D5077,3048 D2029,3048 D2029,1016 U6476,0  @ "
        );

        let reference = parse_plt_paths(SQUARE);
        let overcut = parse_plt_paths(OVERCUT);
        assert_eq!(reference.len(), 1);
        assert_eq!(reference[0].len(), 5);
        assert_eq!(overcut.len(), 1);
        assert!(overcut[0].len() > reference[0].len());

        // The oracle suite uses 12 units (~0.3 mm) for approximate matches and
        // 24 units (~0.6 mm) for loose matches. Exercise both sides of those
        // deterministic quantitative gates without depending on vertex counts.
        let shifted_near: Vec<_> = reference
            .iter()
            .map(|path| {
                path.iter()
                    .map(|point| FixturePoint {
                        x: point.x + 8.0,
                        y: point.y + 8.0,
                    })
                    .collect()
            })
            .collect();
        let shifted_far: Vec<_> = reference
            .iter()
            .map(|path| {
                path.iter()
                    .map(|point| FixturePoint {
                        x: point.x + 18.0,
                        y: point.y + 18.0,
                    })
                    .collect()
            })
            .collect();
        assert!(symmetric_hausdorff(&reference, &shifted_near) <= 12.0);
        assert!(symmetric_hausdorff(&reference, &shifted_far) > 24.0);
    }
}
