use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet, VecDeque, hash_map::DefaultHasher},
    hash::{Hash, Hasher},
    io::Write,
    sync::{Arc, mpsc},
};

use egui::{Color32, Id, KeyboardShortcut, Modal, Modifiers, Pos2, Vec2};
use futures::{StreamExt, lock::Mutex};
use geo::{BoundingRect, Contains, Coord, Intersects, LineString, Rect as GeoRect};
use image::{EncodableLayout, GenericImageView, ImageEncoder};
use serde::{Deserialize, Serialize};
use sha1::Digest;
use strum::IntoEnumIterator;
use tracing::{debug, error, info, trace};
use uuid::Uuid;

use crate::{
    Rc,
    calibration::{
        CalibrationActivationPolicy, CalibrationCutMode, CalibrationMethod, CalibrationObservation,
        CalibrationPayloadHashes, CalibrationPlotterCommand, CalibrationPlotterCommandKind,
        CalibrationPolicy, CalibrationProfile, CalibrationRun, CalibrationRunState,
        CalibrationSolution, CalibrationStore, CalibrationTargetManifest, CalibrationWizard,
        CanvasToPlotter, CutPathDirection, JobSlot as CalibrationJobSlot, ManifestIdentity,
        PrinterCalibrationKey, ScanAnalysisConfig, ScanAnalysisReport, ScanSlot,
        StablePrinterIdentity, TargetCutMode, TargetManifest, TransformBounds, ValidationMetrics,
        analyze_flatbed_scan, flatbed_calibration, flatbed_validation, manual_calibration,
        manual_validation, observation_error_metrics, render_print_raster, solve_calibration,
    },
    cut::{CutAction, CutGenerator, CutImage, CutTuning, OvercutSettings, apply_overcut},
    export::{ToolpathStats, cut_svg, jpeg_pdf, toolpath_debug_svg},
    jobs::{JobQueue, JobSpec, JobStatus as QueueJobStatus, Printer as QueuePrinter},
    path_edit::{smooth_path, union_paths},
    peel_tab::{PeelTab, peel_tabs as build_peel_tabs},
    protocol::*,
    shapes::{self, ProceduralShape},
    spawn, spawn_blocking,
    studio::{
        self, CutlineOwner, DocumentKind, DocumentSettings, ImageAdjustments, MaterialProfile,
        PackItem, PlaceholderFit, SavedImage, StudioDocument, TemplatePlaceholder,
    },
    theme,
    toolpath::{CutMode, CutPhase, effective_cut_modes, plan_cut_phases},
    transports::*,
    views,
};

mod calibration_ui;

const MATERIAL_PROFILES_STORAGE_KEY: &str = "sapodilla.material-profiles.v1";
const LIBRARY_FOLDERS_STORAGE_KEY: &str = "sapodilla.library-folders.v1";
const LIBRARY_CYCLE_STORAGE_KEY: &str = "sapodilla.library-cycle.v1";
const LIBRARY_CONSUMED_STORAGE_KEY: &str = "sapodilla.library-consumed-ahead.v1";
const CANVAS_VIEW_STORAGE_KEY: &str = "sapodilla.canvas-view.v1";
const APPEARANCE_STORAGE_KEY: &str = "sapodilla.appearance.v1";
const CALIBRATION_STORAGE_KEY: &str = "sapodilla.calibration.v1";
const CALIBRATION_SESSION_STORAGE_KEY: &str = "sapodilla.calibration-session.v1";
const PRINTER_FALLBACK_NAMES_STORAGE_KEY: &str = "sapodilla.printer-fallback-names.v1";
const APPEARANCE_VERSION: u8 = 1;
#[cfg(any(not(target_arch = "wasm32"), test))]
const LIBRARY_PAGE_SIZE: usize = 100;
const MAX_LIBRARY_FILL_ATTEMPTS: usize = 512;
const NEW_ARTWORK_MIN_FRACTION: f32 = 0.22;
const NEW_ARTWORK_MAX_FRACTION: f32 = 0.72;

#[derive(Clone, Debug)]
pub struct CalibrationScanImport {
    report: ScanAnalysisReport,
    /// Runtime-only, bounded PNG used to let the operator visually verify the
    /// detected cut centers. The original full-resolution scan is not kept.
    preview_png: Arc<[u8]>,
    preview_sha1: String,
}

#[derive(derive_more::Debug)]
pub enum Action {
    Error(anyhow::Error),
    DiscoveredDevices {
        transport_index: usize,
        result: anyhow::Result<Vec<DiscoveredDevice>>,
    },
    #[cfg(not(target_arch = "wasm32"))]
    TransportReady {
        printer_id: String,
        #[debug(skip)]
        result: anyhow::Result<Rc<TransportManager>>,
    },
    TransportEvent {
        printer_id: String,
        event: TransportEvent,
    },
    PrinterIdentityLoaded {
        printer_id: String,
        result: anyhow::Result<PrinterIdentityInfo>,
    },
    LoadedAvocadoPackets(Result<Vec<AvocadoPacket>, ProtocolError>),
    LoadedImage(#[debug(skip)] anyhow::Result<LoadedImage>),
    ReplacedImage {
        image_id: String,
        #[debug(skip)]
        result: anyhow::Result<LoadedImage>,
    },
    #[cfg(all(feature = "background-ml", not(target_arch = "wasm32")))]
    BackgroundModelSelected(anyhow::Result<std::path::PathBuf>),
    BackgroundRemoved {
        image_id: String,
        #[debug(skip)]
        result: anyhow::Result<image::RgbaImage>,
    },
    ImageAdjusted {
        image_id: String,
        source_revision: u64,
        adjustments: ImageAdjustments,
        #[debug(skip)]
        image: image::RgbaImage,
    },
    EdgeBackgroundRemoved {
        image_id: String,
        source_revision: u64,
        #[debug(skip)]
        image: image::RgbaImage,
    },
    LoadedLibraryImages(#[debug(skip)] Vec<anyhow::Result<LoadedImage>>),
    #[cfg(not(target_arch = "wasm32"))]
    LoadedLibraryFolder {
        path: String,
    },
    LoadedDocument(#[debug(skip)] anyhow::Result<(StudioDocument, Vec<LoadedImage>)>),
    LoadedCutPaths(anyhow::Result<Vec<LineString<f32>>>),
    SendProgress {
        job_id: u64,
        progress: f32,
    },
    PrinterJobError {
        job_id: u64,
        error: anyhow::Error,
    },
    Cut {
        generation_id: u64,
        source_geometry: CutGeometrySnapshot,
        action: CutAction,
    },
    PrintPrepared {
        capabilities: Vec<&'static str>,
        #[debug(skip)]
        job: PendingPrintJob,
    },
    PrintRouteEncoded {
        printer_id: String,
        job_id: u64,
        #[debug(skip)]
        payload: EncodedPrintJob,
    },
    CalibrationJobPrepared {
        run_id: String,
        validation_generation: u32,
        physical_sheet_attempt: u32,
        slot: CalibrationJobSlot,
        spec: JobSpec,
        #[debug(skip)]
        result: anyhow::Result<PendingPrintJob>,
    },
    CalibrationScanAnalyzed {
        run_id: String,
        validation_generation: u32,
        physical_sheet_attempt: u32,
        scan_request_generation: u32,
        slot: ScanSlot,
        file_name: String,
        #[debug(skip)]
        result: Result<CalibrationScanImport, String>,
    },
    CalibrationCandidateSolved {
        run_id: String,
        validation_generation: u32,
        #[debug(skip)]
        result: Result<CalibrationSolution, String>,
    },
    CalibrationStoreImported {
        #[debug(skip)]
        result: Result<CalibrationStore, String>,
    },
    CalibrationDeviceJobStarted {
        queue_id: u64,
        device_job_id: u32,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CutGeometrySnapshot {
    device: usize,
    mode: usize,
    canvas_size: usize,
    tuning: CutTuningSnapshot,
    images: Vec<CutImageGeometry>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CutTuningSnapshot {
    buffer: u32,
    minimum_length: u32,
    smoothing: usize,
    simplify: u32,
    internal: bool,
    white_transparent: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CutImageGeometry {
    id: String,
    offset: [u32; 2],
    scale: [u32; 2],
    rotation_degrees: u32,
    content_revision: u64,
    visible: bool,
    enable_cutting: bool,
}

fn should_accept_cut_action(
    active_generation: Option<u64>,
    generation_id: u64,
    source: &CutGeometrySnapshot,
    current: &CutGeometrySnapshot,
) -> bool {
    active_generation == Some(generation_id) && source == current
}

fn update_cut_geometry_snapshot(
    previous: &mut Option<CutGeometrySnapshot>,
    current: CutGeometrySnapshot,
) -> bool {
    if previous.as_ref() == Some(&current) {
        false
    } else {
        *previous = Some(current);
        true
    }
}

pub struct SapodillaApp {
    pub tx: ContextSender<Action>,
    pub rx: mpsc::Receiver<Action>,

    pub transports: Vec<Rc<Mutex<Transport>>>,
    pub transport_names: Vec<Cow<'static, str>>,
    pub transport_supports_discovery: Vec<bool>,
    pub selected_transport_index: usize,
    pub discovered_devices: Vec<DiscoveredDevice>,
    pub selected_transport_device: Option<String>,
    pub discovering_devices: bool,

    pub transport_manager: Option<Rc<TransportManager>>,
    pub printer_connections: BTreeMap<String, Rc<TransportManager>>,
    pub printer_statuses: BTreeMap<String, TransportStatus>,
    pub printer_identities: BTreeMap<String, PrinterIdentityInfo>,
    pub printer_identity_errors: BTreeMap<String, String>,
    printer_identity_loading: BTreeSet<String>,
    printer_fallback_names: BTreeMap<String, String>,
    printer_fallback_drafts: BTreeMap<String, String>,
    pub transport_status: TransportStatus,

    pub selected_device: usize,
    pub selected_mode: usize,
    pub selected_canvas_size: usize,
    pub previous_canvas_size: Vec2,
    pub copies: usize,

    pub device_status: Option<(PrinterState, PrinterSubState, String)>,
    pub job_status: Option<JobStatusInfo>,
    pub job_queue: JobQueue,
    pub active_queue_job: Option<u64>,
    pub active_queue_jobs: BTreeMap<String, u64>,
    pending_print_jobs: BTreeMap<u64, PendingPrintJob>,
    print_preparing: bool,
    pub send_progress: Option<f32>,

    pub packets: VecDeque<AvocadoPacket>,
    pub viewing_packet: Option<AvocadoPacket>,
    pub cut_tuning: CutTuning,
    pub cut_shapes: Vec<LineString<f32>>,
    pub manual_cut_shapes: Vec<LineString<f32>>,
    pub cut_modes: Vec<CutMode>,
    pub auto_cut_count: usize,
    cut_geometry_snapshot: Option<CutGeometrySnapshot>,
    cut_validation_snapshot: Option<CutValidationSnapshot>,
    next_cut_generation_id: u64,
    active_cut_generation: Option<u64>,
    pub has_intersections: bool,
    pub off_canvas: bool,
    pub cut_progress: Option<(usize, usize)>,
    pub(crate) cut_preview_cache_key: Option<u64>,
    pub(crate) cut_preview_cache: Vec<CutPhase>,
    pub(crate) cut_preview_tabs: Vec<(usize, PeelTab)>,
    pub(crate) cut_preview_stats: ToolpathStats,

    pub showing_packet_log: bool,
    pub showing_avocado_packet_debug: bool,
    pub avocado_debug_packets: Option<Result<Vec<AvocadoPacket>, ProtocolError>>,

    pub canvas_rect: egui::Rect,
    pub loaded_images: Vec<LoadedImage>,
    pub document_kind: DocumentKind,
    pub template_placeholders: Vec<TemplatePlaceholder>,
    pub cutline_owners: Vec<Option<CutlineOwner>>,
    pub cutline_locked: Vec<bool>,
    pub(crate) peel_tab_positions: Vec<Option<f32>>,
    pub library: Vec<LoadedImage>,
    pub library_folders: Vec<String>,
    pub library_disk_paths: Vec<std::path::PathBuf>,
    pub library_page: usize,
    pub library_has_more: bool,
    pub selected_images: Vec<usize>,
    pub(crate) canvas_transform_gesture: Option<views::TransformGesture>,
    pub background_color: [u8; 3],
    pub material_profiles: Vec<MaterialProfile>,
    pub calibration_store: CalibrationStore,
    calibration_session: Option<CalibrationSession>,
    calibration_ui_state: calibration_ui::CalibrationUiState,
    show_calibration_profiles: bool,
    calibration_message: Option<String>,
    pub selected_material: usize,
    pub perf_cut: bool,
    pub perf_dash_mm: f32,
    pub perf_gap_mm: f32,
    pub peel_tabs: bool,
    pub overcut: OvercutSettings,
    pub pack_gap_mm: f32,
    pub pack_allow_rotation: bool,
    pub show_cutlines: bool,
    pub show_safe_area: bool,
    pub show_grid: bool,
    pub show_rulers: bool,
    pub(crate) ruler_unit: CanvasUnit,
    pub grid_spacing_mm: f32,
    pub snap_to_guides: bool,
    pub edit_cutlines: bool,
    pub selected_cut_path: Option<usize>,
    pub selected_cut_node: Option<usize>,
    pub canvas_fit_requested: bool,
    pub pack_cycle: usize,
    pub library_consumed_ahead: BTreeSet<usize>,
    pub pack_overflow: usize,
    pub background_tolerance: u16,
    pub background_feather: u16,
    pub selected_procedural_shape: usize,
    pub shape_width_mm: f32,
    pub shape_height_mm: f32,
    pub background_model_path: Option<std::path::PathBuf>,
    pub background_ml_running: bool,
    image_processing: BTreeSet<String>,

    pub error: Option<anyhow::Error>,
    confirm_new_sheet: bool,
    show_settings: bool,
    show_library_panel: bool,
    show_inspector_panel: bool,
    compact_layout: bool,
    appearance: AppearancePreferences,
    custom_accent_rgb: [u8; 3],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct AppearancePreferences {
    version: u8,
    accent: theme::AccentChoice,
    #[serde(default)]
    theme: egui::ThemePreference,
}

impl Default for AppearancePreferences {
    fn default() -> Self {
        Self {
            version: APPEARANCE_VERSION,
            accent: theme::AccentChoice::default(),
            theme: egui::ThemePreference::System,
        }
    }
}

fn sanitize_appearance(preferences: AppearancePreferences) -> AppearancePreferences {
    if preferences.version == APPEARANCE_VERSION {
        preferences
    } else {
        AppearancePreferences::default()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum CanvasUnit {
    Px,
    Pt,
    #[default]
    Mm,
    Cm,
    In,
}

impl CanvasUnit {
    pub(crate) const ALL: [Self; 5] = [Self::Px, Self::Pt, Self::Mm, Self::Cm, Self::In];

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Px => "px",
            Self::Pt => "pt",
            Self::Mm => "mm",
            Self::Cm => "cm",
            Self::In => "in",
        }
    }

    pub(crate) fn from_mm(self, millimetres: f32, dpi: f32) -> f32 {
        millimetres * self.units_per_mm(dpi)
    }

    pub(crate) fn to_mm(self, value: f32, dpi: f32) -> f32 {
        value / self.units_per_mm(dpi)
    }

    pub(crate) fn pixels_per_unit(self, dpi: f32) -> f32 {
        (dpi / 25.4) / self.units_per_mm(dpi)
    }

    fn units_per_mm(self, dpi: f32) -> f32 {
        match self {
            Self::Px => dpi / 25.4,
            Self::Pt => 72.0 / 25.4,
            Self::Mm => 1.0,
            Self::Cm => 0.1,
            Self::In => 1.0 / 25.4,
        }
    }
}

fn canvas_measurement_controls(
    ui: &mut egui::Ui,
    unit: &mut CanvasUnit,
    grid_spacing_mm: &mut f32,
    dpi: f32,
) {
    ui.horizontal(|ui| {
        ui.label("Units");
        egui::ComboBox::from_id_salt("canvas-ruler-unit")
            .selected_text(unit.label())
            .show_ui(ui, |ui| {
                for option in CanvasUnit::ALL {
                    ui.selectable_value(unit, option, option.label());
                }
            });
    });

    let mut displayed_spacing = unit.from_mm(*grid_spacing_mm, dpi);
    let minimum = unit.from_mm(0.5, dpi);
    let maximum = unit.from_mm(100.0, dpi);
    if ui
        .add(
            egui::Slider::new(&mut displayed_spacing, minimum..=maximum)
                .logarithmic(true)
                .suffix(format!(" {}", unit.label()))
                .text("Grid spacing"),
        )
        .changed()
    {
        *grid_spacing_mm = sanitize_grid_spacing_mm(unit.to_mm(displayed_spacing, dpi));
    }
}

fn sanitize_grid_spacing_mm(value: f32) -> f32 {
    if value.is_finite() {
        value.clamp(0.5, 100.0)
    } else {
        CanvasViewPreferences::default().grid_spacing_mm
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
struct CanvasViewPreferences {
    show_grid: bool,
    show_rulers: bool,
    #[serde(default)]
    ruler_unit: CanvasUnit,
    grid_spacing_mm: f32,
}

impl Default for CanvasViewPreferences {
    fn default() -> Self {
        Self {
            show_grid: true,
            show_rulers: true,
            ruler_unit: CanvasUnit::Mm,
            grid_spacing_mm: 10.0,
        }
    }
}

pub struct ContextSender<A> {
    tx: mpsc::Sender<A>,
    ctx: egui::Context,
}

impl<A> Clone for ContextSender<A> {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
            ctx: self.ctx.clone(),
        }
    }
}

impl<A> ContextSender<A> {
    pub fn new(tx: mpsc::Sender<A>, ctx: egui::Context) -> Self {
        Self { tx, ctx }
    }

    pub fn send(&self, action: A) -> Result<(), mpsc::SendError<A>> {
        self.tx.send(action)?;
        self.ctx.request_repaint();
        Ok(())
    }
}

#[derive(Clone)]
pub struct LoadedImage {
    /// Stable document-local identity. Keeping this on the image makes layer
    /// reorder, duplication, and deletion unable to desynchronise template
    /// relationships from their artwork.
    pub id: String,
    pub name: String,
    pub image: image::RgbaImage,
    pub original_image: image::RgbaImage,
    pub sized_texture: egui::load::SizedTexture,

    pub offset: Pos2,
    pub scale: Vec2,
    pub scale_locked: bool,
    pub enable_cutting: bool,
    pub rotation_degrees: f32,
    pub locked: bool,
    pub visible: bool,
    pub template_fit: PlaceholderFit,
    pub adjustments: ImageAdjustments,
    pub content_revision: u64,

    // We need this handle so egui doesn't drop the texture.
    #[allow(dead_code)]
    handle: egui::TextureHandle,
}

#[derive(Clone)]
pub struct PendingPrintJob {
    encoded_image: Vec<u8>,
    encoded_image_len: usize,
    image_hash: String,
    created_at: u64,
    copies: usize,
    device_index: usize,
    mode_index: usize,
    canvas_index: usize,
    cut_shapes: Vec<LineString<f32>>,
    cut_modes: Vec<CutMode>,
    material: MaterialProfile,
    perf_cut: bool,
    perf_dash: f32,
    perf_gap: f32,
    peel_tabs: bool,
    peel_tab_positions: Vec<Option<f32>>,
    overcut: OvercutSettings,
    /// Calibration targets already contain explicit blade-up bridge segments;
    /// these phases bypass production perforation dashing.
    calibration_phases: Option<Vec<CutPhase>>,
    /// Validation must use the candidate without activating it globally.
    mapping_override: Option<CanvasToPlotter>,
}

#[derive(Clone)]
pub struct EncodedPrintJob {
    source: PendingPrintJob,
    plt: Vec<u8>,
    packet_data: Vec<u8>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct CalibrationSession {
    printer_id: String,
    printer_key: PrinterCalibrationKey,
    wizard: CalibrationWizard,
    baseline_profile_id: Option<String>,
    #[serde(default = "default_calibration_profile_version")]
    baseline_profile_version: u16,
    baseline_mapping: CanvasToPlotter,
    material: MaterialProfile,
    candidate: Option<CalibrationSolution>,
    candidate_mapping: Option<CanvasToPlotter>,
    validation_metrics: Option<ValidationMetrics>,
    #[serde(default)]
    training_scan_report: Option<ScanAnalysisReport>,
    #[serde(default)]
    validation_scan_report: Option<ScanAnalysisReport>,
    #[serde(skip)]
    training_scan_preview_png: Option<Arc<[u8]>>,
    #[serde(skip)]
    validation_scan_preview_png: Option<Arc<[u8]>>,
    #[serde(skip)]
    training_scan_preview_sha1: Option<String>,
    #[serde(skip)]
    validation_scan_preview_sha1: Option<String>,
    primary_queue_job: Option<u64>,
    second_queue_job: Option<u64>,
    validation_queue_job: Option<u64>,
    /// Persisted audit IDs are separate from live queue handles, which are
    /// process-local and must never be resumed after a restart.
    #[serde(default)]
    historical_queue_job_ids: [Option<u64>; 3],
    #[serde(default)]
    image_sha1: [Option<String>; 3],
    #[serde(default)]
    plotter_sha1: [Option<String>; 3],
    #[serde(default)]
    plotter_commands: [Vec<CalibrationPlotterCommand>; 3],
    #[serde(default)]
    validation_generation: u32,
    #[serde(default)]
    device_job_ids: Vec<u32>,
    #[serde(default)]
    validation_device_job_ids: Vec<u32>,
    #[serde(default)]
    device_job_ids_by_slot: [Vec<u32>; 3],
    /// Incremented whenever a completed or ambiguous physical sheet is
    /// reprinted. It becomes part of the manifest run ID so an older scan can
    /// never bind to the replacement sheet.
    #[serde(default)]
    physical_sheet_attempts: [u32; 3],
    #[serde(default)]
    scan_request_generations: [u32; 2],
}

impl CalibrationSession {
    fn manifest_identity(&self, run_id: impl Into<String>) -> ManifestIdentity {
        ManifestIdentity {
            run_id: run_id.into(),
            baseline_mapping_id: self
                .baseline_profile_id
                .clone()
                .unwrap_or_else(|| "pixcut-s1-stock-v1".into()),
            profile_version: self.baseline_profile_version,
            candidate_generation: 0,
        }
    }

    fn validation_manifest_identity(&self) -> ManifestIdentity {
        let mut identity = self.manifest_identity_for_slot(CalibrationJobSlot::Validation);
        identity.candidate_generation = self.wizard.validation_generation;
        identity
    }

    fn manifest_identity_for_slot(&self, slot: CalibrationJobSlot) -> ManifestIdentity {
        let attempt = self.physical_sheet_attempts[Self::slot_index(slot)];
        let base_run_id = if attempt == 0 {
            self.wizard.run_id.clone()
        } else {
            format!("{}-attempt-{attempt}", self.wizard.run_id)
        };
        let run_id = calibration_slot_run_id(&base_run_id, slot);
        self.manifest_identity(run_id)
    }

    fn accepts_async_result(&self, run_id: &str, validation_generation: u32) -> bool {
        self.wizard.run_id == run_id && self.wizard.validation_generation == validation_generation
    }

    fn accepts_job_result(
        &self,
        run_id: &str,
        validation_generation: u32,
        slot: CalibrationJobSlot,
        physical_sheet_attempt: u32,
    ) -> bool {
        self.accepts_async_result(run_id, validation_generation)
            && self.physical_sheet_attempts[Self::slot_index(slot)] == physical_sheet_attempt
    }

    const fn scan_slot_index(slot: ScanSlot) -> usize {
        match slot {
            ScanSlot::Training => 0,
            ScanSlot::Validation => 1,
        }
    }

    fn accepts_scan_result(
        &self,
        run_id: &str,
        validation_generation: u32,
        slot: ScanSlot,
        physical_sheet_attempt: u32,
        scan_request_generation: u32,
    ) -> bool {
        let job_slot = match slot {
            ScanSlot::Training => CalibrationJobSlot::Primary,
            ScanSlot::Validation => CalibrationJobSlot::Validation,
        };
        self.accepts_job_result(
            run_id,
            validation_generation,
            job_slot,
            physical_sheet_attempt,
        ) && self.scan_request_generations[Self::scan_slot_index(slot)] == scan_request_generation
    }

    fn clear_stale_candidate_evidence(&mut self) -> Option<u64> {
        if self.validation_generation == self.wizard.validation_generation {
            return None;
        }
        self.validation_generation = self.wizard.validation_generation;
        self.candidate = None;
        self.candidate_mapping = None;
        self.validation_metrics = None;
        self.validation_scan_report = None;
        self.validation_scan_preview_png = None;
        self.validation_scan_preview_sha1 = None;
        self.image_sha1[2] = None;
        self.plotter_sha1[2] = None;
        self.plotter_commands[2].clear();
        let mut stale_device_ids = std::mem::take(
            &mut self.device_job_ids_by_slot[Self::slot_index(CalibrationJobSlot::Validation)],
        );
        stale_device_ids.append(&mut self.validation_device_job_ids);
        self.device_job_ids
            .retain(|id| !stale_device_ids.contains(id));
        self.validation_device_job_ids.clear();
        self.historical_queue_job_ids[Self::slot_index(CalibrationJobSlot::Validation)] = None;
        self.validation_queue_job.take()
    }

    fn is_resumable(&self) -> bool {
        if self.printer_id.trim().is_empty()
            || self.printer_key.model.trim().is_empty()
            || self.material.passes == 0
            || self.material.passes > 4
            || self.material.blade_pressure > 100
            || self.material.perf_pressure > 100
            || self.device_job_ids.len() > 32
            || self.device_job_ids_by_slot.iter().any(|ids| ids.len() > 32)
            || self
                .device_job_ids_by_slot
                .iter()
                .flatten()
                .any(|id| !self.device_job_ids.contains(id))
            || self
                .physical_sheet_attempts
                .into_iter()
                .any(|attempt| attempt > 1_000_000)
            || self
                .scan_request_generations
                .into_iter()
                .any(|generation| generation > 1_000_000)
            || self
                .baseline_mapping
                .validate(TransformBounds::default())
                .is_err()
            || self
                .candidate_mapping
                .is_some_and(|mapping| mapping.validate(TransformBounds::default()).is_err())
        {
            return false;
        }
        let Ok(json) = serde_json::to_string(&self.wizard) else {
            return false;
        };
        CalibrationWizard::resume_json(&json).is_ok()
    }

    fn queue_job(&self, slot: CalibrationJobSlot) -> Option<u64> {
        match slot {
            CalibrationJobSlot::Primary => self.primary_queue_job,
            CalibrationJobSlot::Second => self.second_queue_job,
            CalibrationJobSlot::Validation => self.validation_queue_job,
        }
    }

    fn set_queue_job(&mut self, slot: CalibrationJobSlot, job_id: u64) {
        self.historical_queue_job_ids[Self::slot_index(slot)] = Some(job_id);
        *match slot {
            CalibrationJobSlot::Primary => &mut self.primary_queue_job,
            CalibrationJobSlot::Second => &mut self.second_queue_job,
            CalibrationJobSlot::Validation => &mut self.validation_queue_job,
        } = Some(job_id);
    }

    fn take_queue_job(&mut self, slot: CalibrationJobSlot) -> Option<u64> {
        match slot {
            CalibrationJobSlot::Primary => self.primary_queue_job.take(),
            CalibrationJobSlot::Second => self.second_queue_job.take(),
            CalibrationJobSlot::Validation => self.validation_queue_job.take(),
        }
    }

    fn slot_index(slot: CalibrationJobSlot) -> usize {
        match slot {
            CalibrationJobSlot::Primary => 0,
            CalibrationJobSlot::Second => 1,
            CalibrationJobSlot::Validation => 2,
        }
    }

    fn sanitize_after_load(mut self) -> Option<Self> {
        if !self.is_resumable() {
            return None;
        }
        let now = self.wizard.updated_at;
        for slot in [
            CalibrationJobSlot::Primary,
            CalibrationJobSlot::Second,
            CalibrationJobSlot::Validation,
        ] {
            let status = match slot {
                CalibrationJobSlot::Primary => self.wizard.primary_job,
                CalibrationJobSlot::Second => self.wizard.second_job,
                CalibrationJobSlot::Validation => self.wizard.validation_job,
            };
            let slot_index = Self::slot_index(slot);
            if status == crate::calibration::JobStatus::Completed
                && self.historical_queue_job_ids[slot_index].is_none()
            {
                // Migrate sessions saved before audit IDs were split from
                // process-local queue handles.
                self.historical_queue_job_ids[slot_index] = self.queue_job(slot);
            }
            if matches!(
                status,
                crate::calibration::JobStatus::Queued | crate::calibration::JobStatus::InProgress
            ) {
                // Queue state is deliberately not persisted. Treat an
                // interrupted dispatch as ambiguous rather than claiming the
                // printer did or did not produce the sheet.
                self.wizard
                    .set_job_status(slot, crate::calibration::JobStatus::Failed, now);
            }
        }
        self.primary_queue_job = None;
        self.second_queue_job = None;
        self.validation_queue_job = None;
        Some(self)
    }
}

const fn default_calibration_profile_version() -> u16 {
    crate::calibration::CALIBRATION_SCHEMA_VERSION as u16
}

#[derive(Clone)]
struct RenderLayer {
    image: image::RgbaImage,
    size: Vec2,
    rotation_degrees: f32,
    visual_offset: Pos2,
}

#[derive(Clone)]
struct RenderSnapshot {
    canvas: Vec2,
    background: [u8; 4],
    layers: Vec<RenderLayer>,
}

impl RenderSnapshot {
    fn render(&self) -> image::DynamicImage {
        let mut buffer = image::ImageBuffer::from_pixel(
            self.canvas.x as u32,
            self.canvas.y as u32,
            image::Rgba(self.background),
        );
        for layer in &self.layers {
            composite_layer(&mut buffer, layer);
        }
        buffer.into()
    }
}

impl LoadedImage {
    pub fn new(ctx: &egui::Context, data: &[u8], offset: Option<Pos2>) -> anyhow::Result<Self> {
        let im = image::load_from_memory(data)?;
        trace!("loaded image");

        let (width, height) = im.dimensions();
        trace!(width, height, "got image size");

        let im = im.to_rgba8();
        let color_image = egui::ColorImage::from_rgba_unmultiplied(
            [width as usize, height as usize],
            im.as_bytes(),
        );

        let handle = ctx.load_texture(Uuid::new_v4(), color_image, egui::TextureOptions::LINEAR);
        let sized_texture =
            egui::load::SizedTexture::new(handle.id(), Vec2::new(width as f32, height as f32));
        trace!(id = ?handle.id(), "finished loading texture");

        Ok(LoadedImage {
            id: format!("image-{}", Uuid::new_v4()),
            name: "Untitled sticker".into(),
            original_image: im.clone(),
            image: im,
            sized_texture,
            offset: offset.unwrap_or(Pos2::ZERO),
            scale: Vec2::splat(1.0),
            scale_locked: true,
            enable_cutting: true,
            rotation_degrees: 0.0,
            locked: false,
            visible: true,
            template_fit: PlaceholderFit::Cover,
            adjustments: ImageAdjustments::default(),
            content_revision: 0,
            handle,
        })
    }

    pub fn size(&self) -> Vec2 {
        self.sized_texture.size * self.scale
    }

    pub fn rescale(&mut self, new_scale: Vec2) {
        if self.scale == new_scale {
            return;
        }

        let current_size = self.size();
        self.scale = new_scale;
        let new_size = self.size();

        let change = (new_size - current_size) / 2.0;
        self.offset -= change;
    }

    pub fn rotated_size(&self) -> Vec2 {
        let radians = self.rotation_degrees.to_radians();
        let (sin, cos) = radians.sin_cos();
        let size = self.size();
        Vec2::new(
            size.x * cos.abs() + size.y * sin.abs(),
            size.x * sin.abs() + size.y * cos.abs(),
        )
    }

    pub fn visual_offset(&self) -> Pos2 {
        self.offset + (self.size() - self.rotated_size()) / 2.0
    }

    pub fn refresh_texture(&mut self) {
        let color_image = egui::ColorImage::from_rgba_unmultiplied(
            [self.image.width() as usize, self.image.height() as usize],
            self.image.as_bytes(),
        );
        self.handle.set(color_image, egui::TextureOptions::LINEAR);
    }

    pub fn apply_adjustments(&mut self) {
        self.image = studio::adjust_image(&self.original_image, self.adjustments);
        self.content_revision = self.content_revision.wrapping_add(1);
        self.refresh_texture();
    }

    pub fn remove_background(&mut self, tolerance: u16, feather: u16) {
        self.image = studio::remove_background(&self.image, tolerance, feather);
        self.original_image = self.image.clone();
        self.adjustments = ImageAdjustments::default();
        self.content_revision = self.content_revision.wrapping_add(1);
        self.refresh_texture();
    }

    pub fn flip_horizontal(&mut self) {
        image::imageops::flip_horizontal_in_place(&mut self.image);
        image::imageops::flip_horizontal_in_place(&mut self.original_image);
        self.content_revision = self.content_revision.wrapping_add(1);
        self.refresh_texture();
    }

    pub fn flip_vertical(&mut self) {
        image::imageops::flip_vertical_in_place(&mut self.image);
        image::imageops::flip_vertical_in_place(&mut self.original_image);
        self.content_revision = self.content_revision.wrapping_add(1);
        self.refresh_texture();
    }
}

/// Give newly placed artwork a predictable, usable initial footprint while
/// preserving its aspect ratio. Document loads and replacements deliberately
/// bypass this: their saved/user-authored transforms are authoritative.
fn normalize_new_artwork(image: &mut LoadedImage, canvas: &CanvasSize) {
    let size = image.size();
    if !size.x.is_finite() || !size.y.is_finite() || size.x <= 0.0 || size.y <= 0.0 {
        return;
    }

    let min_size = canvas.safe_area * NEW_ARTWORK_MIN_FRACTION;
    let max_size = canvas.safe_area * NEW_ARTWORK_MAX_FRACTION;
    let grow = (min_size.x / size.x).max(min_size.y / size.y);
    let fit = (max_size.x / size.x).min(max_size.y / size.y);
    let factor = if size.x > max_size.x || size.y > max_size.y {
        fit
    } else if size.x < min_size.x || size.y < min_size.y {
        grow.min(fit)
    } else {
        1.0
    };

    if factor.is_finite() && factor > 0.0 {
        image.scale *= factor;
    }
    image.offset = ((canvas.size - image.rotated_size()) / 2.0).to_pos2();
}

impl SapodillaApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        crate::icons::install(&cc.egui_ctx);
        egui_extras::install_image_loaders(&cc.egui_ctx);

        let (tx, rx) = mpsc::channel();
        let tx = ContextSender::new(tx, cc.egui_ctx.clone());
        let job_queue = JobQueue::new();

        let material_profiles = cc
            .storage
            .and_then(|storage| {
                eframe::get_value::<Vec<MaterialProfile>>(storage, MATERIAL_PROFILES_STORAGE_KEY)
            })
            .map(sanitize_material_profiles)
            .filter(|profiles| !profiles.is_empty())
            .unwrap_or_else(MaterialProfile::built_ins);
        let selected_material = usize::from(material_profiles.len() > 1);
        let calibration_store = cc
            .storage
            .and_then(|storage| {
                eframe::get_value::<CalibrationStore>(storage, CALIBRATION_STORAGE_KEY)
            })
            .and_then(|store| store.sanitize().ok())
            .unwrap_or_default();
        let calibration_session = cc
            .storage
            .and_then(|storage| {
                eframe::get_value::<CalibrationSession>(storage, CALIBRATION_SESSION_STORAGE_KEY)
            })
            .and_then(CalibrationSession::sanitize_after_load);
        let printer_fallback_names = cc
            .storage
            .and_then(|storage| {
                eframe::get_value::<BTreeMap<String, String>>(
                    storage,
                    PRINTER_FALLBACK_NAMES_STORAGE_KEY,
                )
            })
            .map(sanitize_printer_fallback_names)
            .unwrap_or_default();
        let library_folders = cc
            .storage
            .and_then(|storage| {
                eframe::get_value::<Vec<String>>(storage, LIBRARY_FOLDERS_STORAGE_KEY)
            })
            .unwrap_or_default();
        let library_cycle = cc
            .storage
            .and_then(|storage| eframe::get_value::<usize>(storage, LIBRARY_CYCLE_STORAGE_KEY))
            .unwrap_or_default();
        let library_consumed_ahead = cc
            .storage
            .and_then(|storage| {
                eframe::get_value::<BTreeSet<usize>>(storage, LIBRARY_CONSUMED_STORAGE_KEY)
            })
            .unwrap_or_default();
        let canvas_view = cc
            .storage
            .and_then(|storage| {
                eframe::get_value::<CanvasViewPreferences>(storage, CANVAS_VIEW_STORAGE_KEY)
            })
            .map(|mut view| {
                view.grid_spacing_mm = sanitize_grid_spacing_mm(view.grid_spacing_mm);
                view
            })
            .unwrap_or_default();
        let appearance = cc
            .storage
            .and_then(|storage| {
                eframe::get_value::<AppearancePreferences>(storage, APPEARANCE_STORAGE_KEY)
            })
            .map(sanitize_appearance)
            .unwrap_or_default();
        let custom_accent_rgb = appearance.accent.rgb();
        cc.egui_ctx.set_theme(appearance.theme);
        let library = Vec::new();
        #[cfg(not(target_arch = "wasm32"))]
        let (library_disk_paths, library_has_more) =
            scan_library_page(&library_folders, 0, LIBRARY_PAGE_SIZE);
        #[cfg(target_arch = "wasm32")]
        let (library_disk_paths, library_has_more) = (Vec::new(), false);

        Self {
            tx,
            rx,

            transports: Transport::iter()
                .map(|transport| Rc::new(Mutex::new(transport)))
                .collect(),
            transport_names: Transport::iter()
                .map(|transport| transport.name())
                .collect(),
            transport_supports_discovery: Transport::iter()
                .map(|transport| transport.supports_discovery())
                .collect(),
            selected_transport_index: 0,
            discovered_devices: Vec::new(),
            selected_transport_device: None,
            discovering_devices: false,

            transport_status: TransportStatus::Disconnected,
            transport_manager: None,
            printer_connections: BTreeMap::new(),
            printer_statuses: BTreeMap::new(),
            printer_identities: BTreeMap::new(),
            printer_identity_errors: BTreeMap::new(),
            printer_identity_loading: BTreeSet::new(),
            printer_fallback_names,
            printer_fallback_drafts: BTreeMap::new(),

            selected_device: 0,
            selected_mode: 0,
            selected_canvas_size: 0,
            previous_canvas_size: Vec2::ZERO,
            copies: 1,

            device_status: None,
            job_status: None,
            job_queue,
            active_queue_job: None,
            active_queue_jobs: BTreeMap::new(),
            pending_print_jobs: BTreeMap::new(),
            print_preparing: false,
            send_progress: None,

            packets: Default::default(),
            viewing_packet: None,
            cut_tuning: Default::default(),
            cut_shapes: Vec::new(),
            manual_cut_shapes: Vec::new(),
            cut_modes: Vec::new(),
            auto_cut_count: 0,
            cut_geometry_snapshot: None,
            cut_validation_snapshot: None,
            next_cut_generation_id: 1,
            active_cut_generation: None,
            has_intersections: false,
            off_canvas: false,
            cut_progress: None,
            cut_preview_cache_key: None,
            cut_preview_cache: Vec::new(),
            cut_preview_tabs: Vec::new(),
            cut_preview_stats: ToolpathStats::default(),

            showing_packet_log: false,
            showing_avocado_packet_debug: false,
            avocado_debug_packets: Default::default(),

            canvas_rect: egui::Rect::ZERO,
            loaded_images: Default::default(),
            document_kind: DocumentKind::Sheet,
            template_placeholders: Vec::new(),
            cutline_owners: Vec::new(),
            cutline_locked: Vec::new(),
            peel_tab_positions: Vec::new(),
            library,
            library_folders,
            library_disk_paths,
            library_page: 0,
            library_has_more,
            selected_images: Default::default(),
            canvas_transform_gesture: None,
            background_color: [255, 255, 255],
            material_profiles,
            calibration_store,
            calibration_session,
            calibration_ui_state: calibration_ui::CalibrationUiState::default(),
            show_calibration_profiles: false,
            calibration_message: None,
            selected_material,
            perf_cut: false,
            perf_dash_mm: 1.5,
            perf_gap_mm: 0.5,
            peel_tabs: false,
            overcut: OvercutSettings {
                enabled: false,
                ..OvercutSettings::default()
            },
            pack_gap_mm: 2.0,
            pack_allow_rotation: true,
            show_cutlines: true,
            show_safe_area: true,
            show_grid: canvas_view.show_grid,
            show_rulers: canvas_view.show_rulers,
            ruler_unit: canvas_view.ruler_unit,
            grid_spacing_mm: canvas_view.grid_spacing_mm,
            snap_to_guides: true,
            edit_cutlines: false,
            selected_cut_path: None,
            selected_cut_node: None,
            canvas_fit_requested: false,
            pack_cycle: library_cycle,
            library_consumed_ahead,
            pack_overflow: 0,
            background_tolerance: 36,
            background_feather: 12,
            selected_procedural_shape: 0,
            shape_width_mm: 50.0,
            shape_height_mm: 50.0,
            background_model_path: crate::background_ml::default_model_path()
                .ok()
                .filter(|path| crate::background_ml::inspect_model_file(path).is_ok()),
            background_ml_running: false,
            image_processing: BTreeSet::new(),

            error: None,
            confirm_new_sheet: false,
            show_settings: false,
            show_library_panel: true,
            show_inspector_panel: true,
            compact_layout: false,
            appearance,
            custom_accent_rgb,
        }
    }

    fn current_cut_geometry(&self) -> CutGeometrySnapshot {
        CutGeometrySnapshot {
            device: self.selected_device,
            mode: self.selected_mode,
            canvas_size: self.selected_canvas_size,
            tuning: CutTuningSnapshot {
                buffer: self.cut_tuning.buffer.to_bits(),
                minimum_length: self.cut_tuning.minimum_length.to_bits(),
                smoothing: self.cut_tuning.smoothing,
                simplify: self.cut_tuning.simplify.to_bits(),
                internal: self.cut_tuning.internal,
                white_transparent: self.cut_tuning.white_transparent,
            },
            images: self
                .loaded_images
                .iter()
                .map(|image| CutImageGeometry {
                    id: image.id.clone(),
                    offset: [image.offset.x.to_bits(), image.offset.y.to_bits()],
                    scale: [image.scale.x.to_bits(), image.scale.y.to_bits()],
                    rotation_degrees: image.rotation_degrees.to_bits(),
                    content_revision: image.content_revision,
                    visible: image.visible,
                    enable_cutting: image.enable_cutting,
                })
                .collect(),
        }
    }

    pub(crate) fn synchronize_cut_geometry(&mut self) {
        let current = self.current_cut_geometry();
        if !update_cut_geometry_snapshot(&mut self.cut_geometry_snapshot, current) {
            return;
        }
        self.cut_progress = None;
        self.active_cut_generation = None;
        self.invalidate_auto_cutlines();
    }

    fn invalidate_auto_cutlines(&mut self) {
        let count = self.auto_cut_count.min(self.cut_shapes.len());
        if count == 0 {
            return;
        }
        self.cut_shapes.drain(..count);
        self.cut_modes.drain(..count.min(self.cut_modes.len()));
        self.cutline_owners
            .drain(..count.min(self.cutline_owners.len()));
        self.cutline_locked
            .drain(..count.min(self.cutline_locked.len()));
        self.peel_tab_positions
            .drain(..count.min(self.peel_tab_positions.len()));
        self.auto_cut_count = 0;
        self.selected_cut_path = None;
        self.selected_cut_node = None;
    }

    fn get_transport(&self) -> Rc<Mutex<Transport>> {
        self.transports
            .get(self.selected_transport_index)
            .cloned()
            .unwrap()
    }

    fn selected_transport_supports_discovery(&self) -> bool {
        self.transport_supports_discovery
            .get(self.selected_transport_index)
            .copied()
            .unwrap_or(false)
    }

    fn refresh_transport_devices(&mut self) {
        if self.discovering_devices || !self.selected_transport_supports_discovery() {
            return;
        }

        self.discovering_devices = true;
        let transport_index = self.selected_transport_index;
        let transport = self.get_transport();
        let tx = self.tx.clone();
        spawn(async move {
            let result = transport.lock().await.discover_devices().await;
            if let Err(error) = tx.send(Action::DiscoveredDevices {
                transport_index,
                result,
            }) {
                error!("could not send serial discovery result: {error}");
            }
        });
    }

    fn connect_transport(&mut self) {
        let selected_device = self.selected_transport_device.clone();
        let printer_id = format!(
            "{}:{}",
            self.selected_transport_index,
            selected_device.as_deref().unwrap_or("default")
        );
        if self.printer_connections.contains_key(&printer_id) {
            self.error = Some(anyhow::anyhow!("that printer is already connected"));
            return;
        }
        let printer_name = selected_device
            .as_ref()
            .and_then(|id| {
                self.discovered_devices
                    .iter()
                    .find(|device| &device.id == id)
            })
            .map(|device| device.name.clone())
            .unwrap_or_else(|| self.transport_names[self.selected_transport_index].to_string());
        if self.job_queue.printer(&printer_id).is_none() {
            self.job_queue
                .add_printer(
                    QueuePrinter::new(&printer_id, printer_name)
                        .with_capabilities(["print", "cut"]),
                )
                .expect("new connection identifier is unique");
            let _ = self
                .job_queue
                .set_printer_offline(&printer_id, "connecting");
        }
        let transport = Rc::new(Mutex::new(
            Transport::iter()
                .nth(self.selected_transport_index)
                .expect("selected transport index remains valid"),
        ));
        let event_tx = self.tx.clone();
        self.transport_status = TransportStatus::Connecting;
        self.printer_statuses
            .insert(printer_id.clone(), TransportStatus::Connecting);

        #[cfg(target_arch = "wasm32")]
        {
            let event_printer_id = printer_id.clone();
            let manager = TransportManager::new(transport, move |event| {
                if let Err(error) = event_tx.send(Action::TransportEvent {
                    printer_id: event_printer_id.clone(),
                    event,
                }) {
                    error!("could not send transport event: {error}");
                }
            });
            self.transport_manager = Some(manager.clone());
            self.printer_connections.insert(printer_id, manager);
        }

        #[cfg(not(target_arch = "wasm32"))]
        {
            let requires_selection = self.selected_transport_supports_discovery();
            let tx = self.tx.clone();
            let ready_printer_id = printer_id.clone();
            spawn(async move {
                let result = async {
                    if requires_selection {
                        let selected_device = selected_device
                            .as_deref()
                            .ok_or_else(|| anyhow::anyhow!("select a device before connecting"))?;
                        transport.lock().await.select_device(selected_device)?;
                    }

                    let event_printer_id = printer_id.clone();
                    Ok(TransportManager::new(transport, move |event| {
                        if let Err(error) = event_tx.send(Action::TransportEvent {
                            printer_id: event_printer_id.clone(),
                            event,
                        }) {
                            error!("could not send transport event: {error}");
                        }
                    }))
                }
                .await;

                if let Err(error) = tx.send(Action::TransportReady {
                    printer_id: ready_printer_id,
                    result,
                }) {
                    error!("could not send transport manager: {error}");
                }
            });
        }
    }

    fn request_printer_identity(&self, printer_id: String, manager: Rc<TransportManager>) {
        let tx = self.tx.clone();
        spawn(async move {
            let result = async {
                let id = manager.next_message_id();
                let response = manager
                    .wait_for_response(AvocadoPacket {
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
                            "method": "get-prop",
                            "params": ["model", "serial-number", "firmware-revision"]
                        }))?,
                    })
                    .await?;
                decode_printer_identity(&response)
                    .ok_or_else(|| anyhow::anyhow!("printer returned an invalid identity response"))
            }
            .await;
            let _ = tx.send(Action::PrinterIdentityLoaded { printer_id, result });
        });
    }

    fn upload_image(&self, ctx: &egui::Context) {
        let ctx = ctx.clone();
        let tx = self.tx.clone();

        spawn(async move {
            let file = rfd::AsyncFileDialog::new()
                .add_filter("image", &["jpg", "jpeg", "png"])
                .pick_file()
                .await;

            if let Some(file) = file {
                let data = file.read().await;

                let action = match LoadedImage::new(&ctx, &data, None) {
                    Ok(image) => Action::LoadedImage(Ok(image)),
                    Err(err) => Action::LoadedImage(Err(err)),
                };

                tx.send(action).unwrap();
            }
        });
    }

    fn replace_image(&self, ctx: &egui::Context, index: usize) {
        let Some(image_id) = self.loaded_images.get(index).map(|image| image.id.clone()) else {
            return;
        };
        let ctx = ctx.clone();
        let tx = self.tx.clone();
        spawn(async move {
            let Some(file) = rfd::AsyncFileDialog::new()
                .add_filter("image", &["jpg", "jpeg", "png"])
                .pick_file()
                .await
            else {
                return;
            };
            let result = LoadedImage::new(&ctx, &file.read().await, None);
            let _ = tx.send(Action::ReplacedImage { image_id, result });
        });
    }

    #[cfg(all(feature = "background-ml", not(target_arch = "wasm32")))]
    fn choose_background_model(&self) {
        let tx = self.tx.clone();
        spawn(async move {
            let Some(file) = rfd::AsyncFileDialog::new()
                .add_filter("BiRefNet ONNX model", &["onnx"])
                .pick_file()
                .await
            else {
                return;
            };
            let path = file.path().to_path_buf();
            let result = crate::background_ml::inspect_model_file(&path).map(|info| info.path);
            let _ = tx.send(Action::BackgroundModelSelected(result));
        });
    }

    #[cfg(all(feature = "background-ml", not(target_arch = "wasm32")))]
    fn remove_background_ml(&mut self, index: usize) {
        let model_path = self.background_model_path.clone();
        let Some((image_id, image)) = self
            .loaded_images
            .get(index)
            .map(|image| (image.id.clone(), image.image.clone()))
        else {
            return;
        };
        self.background_ml_running = true;
        let tx = self.tx.clone();
        crate::spawn_blocking(move || {
            let result = (|| {
                let model_path = match model_path {
                    Some(path) if crate::background_ml::inspect_model_file(&path).is_ok() => path,
                    None => crate::background_ml::ensure_model_available()?,
                    Some(_) => crate::background_ml::ensure_model_available()?,
                };
                crate::background_ml::remove_background_with_cached_model(&image, &model_path)
            })();
            let _ = tx.send(Action::BackgroundRemoved { image_id, result });
        });
    }

    fn import_library_images(&self, ctx: &egui::Context) {
        let ctx = ctx.clone();
        let tx = self.tx.clone();
        spawn(async move {
            let files = rfd::AsyncFileDialog::new()
                .add_filter("images", &["jpg", "jpeg", "png"])
                .pick_files()
                .await
                .unwrap_or_default();
            let mut images = Vec::with_capacity(files.len());
            for file in files {
                let name = file.file_name();
                let data = file.read().await;
                images.push(LoadedImage::new(&ctx, &data, None).map(|mut image| {
                    image.name = name;
                    image
                }));
            }
            let _ = tx.send(Action::LoadedLibraryImages(images));
        });
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn import_library_folder(&self, _ctx: &egui::Context) {
        let tx = self.tx.clone();
        spawn(async move {
            let Some(folder) = rfd::AsyncFileDialog::new().pick_folder().await else {
                return;
            };
            let root = folder.path().to_path_buf();
            let _ = tx.send(Action::LoadedLibraryFolder {
                path: root.to_string_lossy().into_owned(),
            });
        });
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn add_disk_library_image(&self, ctx: &egui::Context, path: std::path::PathBuf) {
        let ctx = ctx.clone();
        let tx = self.tx.clone();
        spawn(async move {
            let result = std::fs::read(&path)
                .map_err(anyhow::Error::from)
                .and_then(|data| {
                    LoadedImage::new(&ctx, &data, None).map(|mut image| {
                        image.name = path
                            .file_stem()
                            .and_then(|name| name.to_str())
                            .unwrap_or("Untitled sticker")
                            .to_owned();
                        image
                    })
                });
            let _ = tx.send(Action::LoadedImage(result));
        });
    }

    fn import_svg(&self) {
        let tx = self.tx.clone();
        spawn(async move {
            let Some(file) = rfd::AsyncFileDialog::new()
                .add_filter("Scalable Vector Graphics", &["svg"])
                .pick_file()
                .await
            else {
                return;
            };
            let data = file.read().await;
            let result = String::from_utf8(data)
                .map_err(anyhow::Error::from)
                .and_then(|svg| studio::parse_svg(&svg, 12));
            let _ = tx.send(Action::LoadedCutPaths(result));
        });
    }

    fn export_png(&self) {
        let snapshot = self.render_snapshot();
        let tx = self.tx.clone();
        spawn_blocking(move || {
            let image = snapshot.render();
            let mut bytes = Vec::new();
            if let Err(error) = image::codecs::png::PngEncoder::new(&mut bytes).write_image(
                image.as_bytes(),
                image.width(),
                image.height(),
                image::ExtendedColorType::Rgba8,
            ) {
                let _ = tx.send(Action::Error(error.into()));
                return;
            }
            save_export(tx, "sapodilla-sheet.png", "PNG image", &["png"], bytes);
        });
    }

    fn export_cut_svg(&self) {
        save_export(
            self.tx.clone(),
            "sapodilla-cutlines.svg",
            "Scalable Vector Graphics",
            &["svg"],
            cut_svg(&self.cut_shapes, self.get_canvas().size).into_bytes(),
        );
    }

    fn export_pdf(&self) {
        let snapshot = self.render_snapshot();
        let dpi = DEVICES[self.selected_device].dpi;
        let tx = self.tx.clone();
        spawn_blocking(move || {
            let image = snapshot.render();
            match jpeg_pdf(&encode_image(&image), image.width(), image.height(), dpi) {
                Ok(bytes) => {
                    save_export(tx, "sapodilla-sheet.pdf", "PDF document", &["pdf"], bytes)
                }
                Err(error) => {
                    let _ = tx.send(Action::Error(error));
                }
            }
        });
    }

    fn export_toolpath_debug_svg(&self) {
        save_export(
            self.tx.clone(),
            "sapodilla-toolpath-debug.svg",
            "Scalable Vector Graphics",
            &["svg"],
            toolpath_debug_svg(&self.prepared_toolpaths(), self.get_canvas().size).into_bytes(),
        );
    }

    fn prepared_toolpaths(&self) -> Vec<LineString<f32>> {
        let modes = effective_cut_modes(self.cut_shapes.len(), &self.cut_modes, self.perf_cut);
        let paths = self
            .cut_shapes
            .iter()
            .zip(&modes)
            .map(|(path, mode)| {
                if *mode == CutMode::Kiss && self.overcut.enabled && path.0.first() == path.0.last()
                {
                    apply_overcut(path, self.overcut)
                } else {
                    path.clone()
                }
            })
            .collect::<Vec<_>>();
        let material = &self.material_profiles[self.selected_material];
        let dpi = DEVICES[self.selected_device].dpi;
        let mut prepared = plan_cut_phases(
            &paths,
            &modes,
            material.blade_pressure,
            material.perf_pressure,
            self.perf_dash_mm * dpi / 25.4,
            self.perf_gap_mm * dpi / 25.4,
        )
        .into_iter()
        .flat_map(|phase| phase.paths)
        .collect::<Vec<_>>();
        if self.peel_tabs {
            let enabled = modes
                .iter()
                .map(|mode| *mode != CutMode::Disabled)
                .collect::<Vec<_>>();
            prepared.extend(
                build_peel_tabs(&self.cut_shapes, &enabled, &self.peel_tab_positions)
                    .into_iter()
                    .map(|(_, tab)| tab.path),
            );
        }
        prepared
    }

    fn export_plt(&self) {
        let mode = &DEVICES[self.selected_device].modes[self.selected_mode];
        let canvas_size = &mode.canvas_sizes[self.selected_canvas_size];
        let bytes = encode_plt(
            &self.cut_shapes,
            &self.cut_modes,
            stock_plotter_mapping(self.selected_device, canvas_size),
            canvas_size,
            &self.material_profiles[self.selected_material],
            self.perf_cut,
            self.perf_dash_mm * DEVICES[self.selected_device].dpi / 25.4,
            self.perf_gap_mm * DEVICES[self.selected_device].dpi / 25.4,
            self.peel_tabs,
            &self.peel_tab_positions,
            self.overcut,
        );
        save_export(
            self.tx.clone(),
            "sapodilla-toolpath.plt",
            "HPGL/PLT toolpath",
            &["plt"],
            bytes,
        );
    }

    fn document(&self, kind: DocumentKind) -> anyhow::Result<StudioDocument> {
        let mut document = StudioDocument::new(kind, self.get_canvas().size, self.background_color);
        document.material = self.material_profiles[self.selected_material].clone();
        document.settings = DocumentSettings {
            selected_device: self.selected_device,
            selected_mode: self.selected_mode,
            selected_canvas_size: self.selected_canvas_size,
            copies: self.copies,
            cut_buffer: self.cut_tuning.buffer,
            cut_minimum_length: self.cut_tuning.minimum_length,
            cut_smoothing: self.cut_tuning.smoothing,
            cut_simplify: self.cut_tuning.simplify,
            cut_internal: self.cut_tuning.internal,
            cut_white_transparent: self.cut_tuning.white_transparent,
            perf_cut: self.perf_cut,
            perf_dash_mm: self.perf_dash_mm,
            perf_gap_mm: self.perf_gap_mm,
            peel_tabs: self.peel_tabs,
            pack_gap_mm: self.pack_gap_mm,
            pack_allow_rotation: self.pack_allow_rotation,
            overcut_enabled: self.overcut.enabled,
            overcut_steps: self.overcut.steps,
            overcut_maximum_angle_degrees: self.overcut.maximum_angle_degrees,
            overcut_reach_mm: self.overcut.reach_pixels * 25.4 / DEVICES[self.selected_device].dpi,
            overcut_snap_to_pixels: self.overcut.snap_to_pixels,
        };
        let sticker_selection = (kind == DocumentKind::Sticker && self.selected_images.len() == 1)
            .then_some(self.selected_images[0]);
        for (index, image) in self.loaded_images.iter().enumerate() {
            if sticker_selection.is_some_and(|selected| selected != index) {
                continue;
            }
            let mut png = Vec::new();
            image::codecs::png::PngEncoder::new(&mut png).write_image(
                image.image.as_bytes(),
                image.image.width(),
                image.image.height(),
                image::ExtendedColorType::Rgba8,
            )?;
            let mut saved = SavedImage::from_png(
                &image.name,
                &png,
                image.offset,
                image.scale,
                image.rotation_degrees,
                image.enable_cutting,
                image.locked || kind == DocumentKind::Template,
                image.visible,
            );
            saved.template_fit = image.template_fit;
            saved.id.clone_from(&image.id);
            document.images.push(saved);
        }
        document.ensure_object_ids();
        let saved_image_sources = self
            .loaded_images
            .iter()
            .enumerate()
            .filter(|(index, _)| sticker_selection.is_none_or(|selected| selected == *index))
            .zip(document.images.iter())
            .map(|((source_index, _), saved)| (source_index, saved.id.clone()))
            .collect::<Vec<_>>();
        if kind == DocumentKind::Template {
            document.template_placeholders = if self.document_kind == DocumentKind::Template
                && !self.template_placeholders.is_empty()
            {
                reconcile_template_placeholders(
                    self.template_placeholders.clone(),
                    document.images.iter().map(|image| image.id.as_str()),
                )
            } else {
                saved_image_sources
                    .iter()
                    .map(|(source_index, image_id)| {
                        let image = &self.loaded_images[*source_index];
                        let size = image.rotated_size();
                        TemplatePlaceholder {
                            id: format!("placeholder-{image_id}"),
                            name: image.name.clone(),
                            bounds: [
                                image.visual_offset().x,
                                image.visual_offset().y,
                                size.x,
                                size.y,
                            ],
                            rotation_degrees: image.rotation_degrees,
                            fit: image.template_fit,
                            assigned_image_id: Some(image_id.clone()),
                        }
                    })
                    .collect()
            };
        }
        let cut_sources = self
            .cut_shapes
            .iter()
            .enumerate()
            .filter(|path| {
                let path = path.1;
                let Some(selected) = sticker_selection else {
                    return true;
                };
                let Some(bounds) = path.bounding_rect() else {
                    return false;
                };
                let image = &self.loaded_images[selected];
                egui::Rect::from_min_size(image.visual_offset(), image.rotated_size()).contains(
                    Pos2::new(
                        (bounds.min().x + bounds.max().x) / 2.0,
                        (bounds.min().y + bounds.max().y) / 2.0,
                    ),
                )
            })
            .map(|(source_index, path)| {
                (
                    source_index,
                    path.0.iter().map(|point| [point.x, point.y]).collect(),
                )
            })
            .collect::<Vec<(usize, Vec<[f32; 2]>)>>();
        document.cut_paths = cut_sources.iter().map(|(_, path)| path.clone()).collect();
        for (cut_path_index, (source_cut_path_index, path)) in cut_sources.iter().enumerate() {
            let Some((min_x, max_x, min_y, max_y)) =
                path.iter()
                    .fold(None::<(f32, f32, f32, f32)>, |bounds, point| {
                        let [x, y] = *point;
                        Some(match bounds {
                            None => (x, x, y, y),
                            Some((min_x, max_x, min_y, max_y)) => {
                                (min_x.min(x), max_x.max(x), min_y.min(y), max_y.max(y))
                            }
                        })
                    })
            else {
                continue;
            };
            let center = Pos2::new((min_x + max_x) / 2.0, (min_y + max_y) / 2.0);
            let stored_owner = self
                .cutline_owners
                .get(*source_cut_path_index)
                .cloned()
                .flatten();
            let owner_is_valid = |owner: &CutlineOwner| match owner {
                CutlineOwner::Image(id) => document.images.iter().any(|image| image.id == *id),
                CutlineOwner::TemplatePlaceholder(id) => document
                    .template_placeholders
                    .iter()
                    .any(|placeholder| placeholder.id == *id),
            };
            let owner = match stored_owner {
                Some(owner) if owner_is_valid(&owner) => Some(owner),
                // A deleted object's relationship must not be silently
                // transferred to whatever now occupies the same geometry.
                Some(_) => None,
                None => saved_image_sources
                    .iter()
                    .find_map(|(source_index, image_id)| {
                        let image = &self.loaded_images[*source_index];
                        egui::Rect::from_min_size(image.visual_offset(), image.rotated_size())
                            .contains(center)
                            .then(|| {
                                if kind == DocumentKind::Template {
                                    CutlineOwner::TemplatePlaceholder(format!(
                                        "placeholder-{image_id}"
                                    ))
                                } else {
                                    CutlineOwner::Image(image_id.clone())
                                }
                            })
                    }),
            };
            document.set_cutline_owner(cut_path_index, owner)?;
            if let Some(metadata) = document
                .cutline_metadata
                .iter_mut()
                .find(|metadata| metadata.cut_path_index == cut_path_index)
            {
                metadata.cut_mode = self
                    .cut_modes
                    .get(*source_cut_path_index)
                    .copied()
                    .unwrap_or_default();
                metadata.locked = kind == DocumentKind::Template
                    || self
                        .cutline_locked
                        .get(*source_cut_path_index)
                        .copied()
                        .unwrap_or(false);
                metadata.peel_tab_position = self
                    .peel_tab_positions
                    .get(*source_cut_path_index)
                    .copied()
                    .flatten();
            }
        }
        Ok(document)
    }

    fn save_document(&mut self, kind: DocumentKind) {
        let document = match self.document(kind) {
            Ok(document) => document,
            Err(error) => {
                self.error = Some(error);
                return;
            }
        };
        let tx = self.tx.clone();
        spawn(async move {
            let extension = StudioDocument::extension();
            let document_name = match kind {
                DocumentKind::Sticker => "sticker",
                DocumentKind::Sheet => "sheet",
                DocumentKind::Template => "template",
            };
            let Some(file) = rfd::AsyncFileDialog::new()
                .add_filter("Sapodilla project", &[extension])
                .set_file_name(format!("untitled-{document_name}.{extension}"))
                .save_file()
                .await
            else {
                return;
            };
            let result = match document.to_json() {
                Ok(data) => file.write(&data).await.map_err(anyhow::Error::from),
                Err(error) => Err(error),
            };
            if let Err(error) = result {
                let _ = tx.send(Action::Error(error));
            }
        });
    }

    fn open_document(&self, ctx: &egui::Context) {
        let ctx = ctx.clone();
        let tx = self.tx.clone();
        spawn(async move {
            let Some(file) = rfd::AsyncFileDialog::new()
                .add_filter("Sapodilla projects", &[StudioDocument::extension()])
                .pick_file()
                .await
            else {
                return;
            };
            let result = async {
                let document = StudioDocument::from_json(&file.read().await)?;
                let mut images = Vec::with_capacity(document.images.len());
                for saved in &document.images {
                    let mut image = LoadedImage::new(
                        &ctx,
                        &saved.png_bytes()?,
                        Some(Pos2::from(saved.offset)),
                    )?;
                    image.id.clone_from(&saved.id);
                    image.name.clone_from(&saved.name);
                    image.scale = Vec2::from(saved.scale);
                    image.rotation_degrees = saved.rotation_degrees;
                    image.enable_cutting = saved.cutting_enabled;
                    image.locked = saved.locked;
                    image.visible = saved.visible;
                    image.template_fit = document
                        .template_placeholders
                        .iter()
                        .find(|placeholder| {
                            placeholder.assigned_image_id.as_ref() == Some(&saved.id)
                        })
                        .map(|placeholder| placeholder.fit)
                        .unwrap_or(saved.template_fit);
                    images.push(image);
                }
                Ok::<_, anyhow::Error>((document, images))
            }
            .await;
            let _ = tx.send(Action::LoadedDocument(result));
        });
    }

    fn auto_pack(&mut self) -> Vec<usize> {
        let canvas = self.get_canvas();
        let safe_offset = (canvas.size - canvas.safe_area) / 2.0;
        let gap = self.pack_gap_mm * DEVICES[self.selected_device].dpi / 25.4;
        let items = self
            .loaded_images
            .iter()
            .enumerate()
            .filter(|(_, image)| image.visible && !image.locked)
            .map(|(index, image)| PackItem {
                index,
                size: image.rotated_size(),
            })
            .collect::<Vec<_>>();
        let placements = studio::auto_pack(&items, canvas.safe_area, gap, self.pack_allow_rotation);
        let locked_obstacles = self
            .loaded_images
            .iter()
            .filter(|image| image.visible && image.locked)
            .map(|image| egui::Rect::from_min_size(image.visual_offset(), image.rotated_size()))
            .collect::<Vec<_>>();
        let mut rejected = placements
            .iter()
            .filter(|placement| {
                let mut size = self.loaded_images[placement.index].rotated_size();
                if placement.rotated {
                    size = Vec2::new(size.y, size.x);
                }
                let target = egui::Rect::from_min_size(placement.offset + safe_offset, size);
                locked_obstacles
                    .iter()
                    .any(|obstacle| obstacle.intersects(target))
            })
            .map(|placement| placement.index)
            .collect::<Vec<_>>();
        loop {
            let old_obstacles = rejected
                .iter()
                .map(|index| {
                    let image = &self.loaded_images[*index];
                    egui::Rect::from_min_size(image.visual_offset(), image.rotated_size())
                })
                .collect::<Vec<_>>();
            let before = rejected.len();
            for placement in &placements {
                let mut size = self.loaded_images[placement.index].rotated_size();
                if placement.rotated {
                    size = Vec2::new(size.y, size.x);
                }
                let target = egui::Rect::from_min_size(placement.offset + safe_offset, size);
                if !rejected.contains(&placement.index)
                    && old_obstacles
                        .iter()
                        .any(|obstacle| obstacle.intersects(target))
                {
                    rejected.push(placement.index);
                }
            }
            if rejected.len() == before {
                break;
            }
        }
        let packed = placements
            .iter()
            .filter(|placement| !rejected.contains(&placement.index))
            .map(|placement| placement.index)
            .collect::<Vec<_>>();
        self.pack_overflow = items.len().saturating_sub(packed.len());
        for placement in placements
            .into_iter()
            .filter(|placement| packed.contains(&placement.index))
        {
            let image = &mut self.loaded_images[placement.index];
            if placement.rotated {
                image.rotation_degrees = (image.rotation_degrees + 90.0).rem_euclid(360.0);
            }
            image.offset =
                placement.offset + safe_offset + (image.rotated_size() - image.size()) / 2.0;
        }
        packed
    }

    fn add_new_artwork(&mut self, mut image: LoadedImage) {
        let was_empty = self.loaded_images.is_empty();
        normalize_new_artwork(&mut image, self.get_canvas());
        self.loaded_images.push(image);
        self.selected_images = vec![self.loaded_images.len() - 1];
        self.canvas_fit_requested = true;

        // The asset drawer has served its purpose once the first item is on
        // the sheet. Keep the inspector available for immediate refinement and
        // return horizontal space to the workspace; Library remains one click away.
        if was_empty {
            self.show_library_panel = false;
        }
    }

    fn apply_artwork_menu_action(&mut self, action: views::ArtworkMenuAction) {
        let target_ids = action.image_ids.into_iter().collect::<BTreeSet<_>>();
        if target_ids.is_empty() {
            return;
        }
        let any_locked = self
            .loaded_images
            .iter()
            .any(|image| target_ids.contains(&image.id) && image.locked);

        match action.command {
            views::ArtworkMenuCommand::Duplicate => {
                let originals = self
                    .loaded_images
                    .iter()
                    .filter(|image| target_ids.contains(&image.id))
                    .cloned()
                    .collect::<Vec<_>>();
                let mut duplicate_ids = BTreeSet::new();
                for mut duplicate in originals {
                    duplicate.id = format!("image-{}", Uuid::new_v4());
                    duplicate.name = format!("{} copy", duplicate.name);
                    duplicate.offset += Vec2::splat(20.0);
                    duplicate_ids.insert(duplicate.id.clone());
                    self.loaded_images.push(duplicate);
                }
                self.selected_images = self
                    .loaded_images
                    .iter()
                    .enumerate()
                    .filter_map(|(index, image)| duplicate_ids.contains(&image.id).then_some(index))
                    .collect();
            }
            views::ArtworkMenuCommand::BringToFront if !any_locked => {
                let (selected, mut remaining): (Vec<_>, Vec<_>) = self
                    .loaded_images
                    .drain(..)
                    .partition(|image| target_ids.contains(&image.id));
                remaining.extend(selected);
                self.loaded_images = remaining;
            }
            views::ArtworkMenuCommand::SendToBack if !any_locked => {
                let (mut selected, remaining): (Vec<_>, Vec<_>) = self
                    .loaded_images
                    .drain(..)
                    .partition(|image| target_ids.contains(&image.id));
                selected.extend(remaining);
                self.loaded_images = selected;
            }
            views::ArtworkMenuCommand::BringForward if !any_locked => {
                for index in (0..self.loaded_images.len().saturating_sub(1)).rev() {
                    let current = target_ids.contains(&self.loaded_images[index].id);
                    let next = target_ids.contains(&self.loaded_images[index + 1].id);
                    if current && !next {
                        self.loaded_images.swap(index, index + 1);
                    }
                }
            }
            views::ArtworkMenuCommand::SendBackward if !any_locked => {
                for index in 1..self.loaded_images.len() {
                    let current = target_ids.contains(&self.loaded_images[index].id);
                    let previous = target_ids.contains(&self.loaded_images[index - 1].id);
                    if current && !previous {
                        self.loaded_images.swap(index, index - 1);
                    }
                }
            }
            views::ArtworkMenuCommand::RotateClockwise if !any_locked => {
                for image in &mut self.loaded_images {
                    if target_ids.contains(&image.id) {
                        image.rotation_degrees =
                            (image.rotation_degrees + 270.0).rem_euclid(360.0) - 180.0;
                    }
                }
            }
            views::ArtworkMenuCommand::RotateCounterclockwise if !any_locked => {
                for image in &mut self.loaded_images {
                    if target_ids.contains(&image.id) {
                        image.rotation_degrees =
                            (image.rotation_degrees + 90.0).rem_euclid(360.0) - 180.0;
                    }
                }
            }
            views::ArtworkMenuCommand::FlipHorizontal if !any_locked => {
                for image in &mut self.loaded_images {
                    if target_ids.contains(&image.id) {
                        image.flip_horizontal();
                    }
                }
            }
            views::ArtworkMenuCommand::FlipVertical if !any_locked => {
                for image in &mut self.loaded_images {
                    if target_ids.contains(&image.id) {
                        image.flip_vertical();
                    }
                }
            }
            views::ArtworkMenuCommand::SetVisible(visible) => {
                for image in &mut self.loaded_images {
                    if target_ids.contains(&image.id) {
                        image.visible = visible;
                    }
                }
            }
            views::ArtworkMenuCommand::SetLocked(locked) => {
                for image in &mut self.loaded_images {
                    if target_ids.contains(&image.id) {
                        image.locked = locked;
                    }
                }
            }
            views::ArtworkMenuCommand::SetCutting(cutting) => {
                for image in &mut self.loaded_images {
                    if target_ids.contains(&image.id) {
                        image.enable_cutting = cutting;
                    }
                }
            }
            views::ArtworkMenuCommand::Remove if !any_locked => {
                self.loaded_images
                    .retain(|image| !target_ids.contains(&image.id));
                for placeholder in &mut self.template_placeholders {
                    if placeholder
                        .assigned_image_id
                        .as_ref()
                        .is_some_and(|id| target_ids.contains(id))
                    {
                        placeholder.assigned_image_id = None;
                    }
                }
                self.image_processing
                    .retain(|image_id| !target_ids.contains(image_id));
                self.selected_images.clear();
            }
            _ => return,
        }

        if !matches!(
            action.command,
            views::ArtworkMenuCommand::Duplicate | views::ArtworkMenuCommand::Remove
        ) {
            self.selected_images = self
                .loaded_images
                .iter()
                .enumerate()
                .filter_map(|(index, image)| target_ids.contains(&image.id).then_some(index))
                .collect();
        }
        self.canvas_transform_gesture = None;
        self.edit_cutlines = false;
        self.selected_cut_path = None;
        self.selected_cut_node = None;
        self.synchronize_cut_geometry();

        if matches!(action.command, views::ArtworkMenuCommand::Remove) {
            for index in (0..self.cutline_owners.len()).rev() {
                let remove = matches!(
                    self.cutline_owners[index].as_ref(),
                    Some(CutlineOwner::Image(image_id)) if target_ids.contains(image_id)
                );
                if remove {
                    if index >= self.auto_cut_count {
                        let manual_index = index - self.auto_cut_count;
                        if manual_index < self.manual_cut_shapes.len() {
                            self.manual_cut_shapes.remove(manual_index);
                        }
                    } else {
                        self.auto_cut_count = self.auto_cut_count.saturating_sub(1);
                    }
                    if index < self.cut_shapes.len() {
                        self.cut_shapes.remove(index);
                    }
                    if index < self.cut_modes.len() {
                        self.cut_modes.remove(index);
                    }
                    self.cutline_owners.remove(index);
                    if index < self.cutline_locked.len() {
                        self.cutline_locked.remove(index);
                    }
                    if index < self.peel_tab_positions.len() {
                        self.peel_tab_positions.remove(index);
                    }
                }
            }
        }
        self.cut_preview_cache_key = None;
        self.cut_validation_snapshot = None;
    }

    fn add_library_to_sheet(&mut self, shuffle: bool) {
        #[cfg(not(target_arch = "wasm32"))]
        let disk_paths = collect_library_paths(&self.library_folders);
        #[cfg(target_arch = "wasm32")]
        let disk_paths = Vec::<std::path::PathBuf>::new();
        let asset_count = self.library.len() + disk_paths.len();
        if asset_count == 0 {
            return;
        }
        let texture_context = self.tx.ctx.clone();

        // Probe one complete production cycle, retaining printable candidates
        // that do not fit as holes for the next sheet while allowing smaller
        // later entries to consume the current sheet's remaining gaps.
        let mut attempts = 0;
        let mut probe_cursor = self.pack_cycle;
        let mut cycle_end = probe_cursor.saturating_add(asset_count);
        let mut deferred_in_cycle = false;
        while attempts < MAX_LIBRARY_FILL_ATTEMPTS {
            probe_cursor = probe_cursor.max(self.pack_cycle);
            if probe_cursor >= cycle_end {
                if deferred_in_cycle {
                    break;
                }
                cycle_end = cycle_end.saturating_add(asset_count);
            }
            if self.library_consumed_ahead.contains(&probe_cursor) {
                probe_cursor = probe_cursor.wrapping_add(1);
                continue;
            }
            let production_position = probe_cursor;
            probe_cursor = probe_cursor.wrapping_add(1);
            let library_index = library_cycle_index(asset_count, production_position, shuffle);
            let previous_transforms = self
                .loaded_images
                .iter()
                .map(|image| (image.offset, image.rotation_degrees))
                .collect::<Vec<_>>();
            let mut image = if let Some(image) = self.library.get(library_index) {
                image.clone()
            } else {
                let disk_index = library_index - self.library.len();
                let Some(path) = disk_paths.get(disk_index) else {
                    break;
                };
                let Ok(data) = std::fs::read(path) else {
                    commit_library_position(
                        &mut self.pack_cycle,
                        &mut self.library_consumed_ahead,
                        production_position,
                    );
                    attempts += 1;
                    continue;
                };
                let Ok(mut image) = LoadedImage::new(&texture_context, &data, None) else {
                    commit_library_position(
                        &mut self.pack_cycle,
                        &mut self.library_consumed_ahead,
                        production_position,
                    );
                    attempts += 1;
                    continue;
                };
                image.name = path
                    .file_stem()
                    .and_then(|name| name.to_str())
                    .unwrap_or("Untitled sticker")
                    .to_owned();
                image
            };
            image.id = format!("image-{}", Uuid::new_v4());
            image.offset = Pos2::ZERO;
            self.loaded_images.push(image);
            let candidate_index = self.loaded_images.len() - 1;
            let required_count = self
                .loaded_images
                .iter()
                .filter(|image| image.visible && !image.locked)
                .count();
            let packed = self.auto_pack();
            if fill_trial_succeeded(&packed, candidate_index, required_count) {
                commit_library_position(
                    &mut self.pack_cycle,
                    &mut self.library_consumed_ahead,
                    production_position,
                );
            } else {
                let candidate_size = self.loaded_images[candidate_index].rotated_size();
                let intrinsically_oversized = !can_fit_empty_sheet(
                    candidate_size,
                    self.get_canvas().safe_area,
                    self.pack_allow_rotation,
                );
                self.loaded_images.remove(candidate_index);
                for (image, (offset, rotation)) in
                    self.loaded_images.iter_mut().zip(previous_transforms)
                {
                    image.offset = offset;
                    image.rotation_degrees = rotation;
                }
                if intrinsically_oversized {
                    commit_library_position(
                        &mut self.pack_cycle,
                        &mut self.library_consumed_ahead,
                        production_position,
                    );
                } else {
                    deferred_in_cycle = true;
                }
            }
            attempts += 1;
        }
        self.pack_overflow = 0;
    }

    fn reset_library_cycle(&mut self) {
        self.pack_cycle = 0;
        self.library_consumed_ahead.clear();
    }

    pub fn background_color32(&self) -> Color32 {
        Color32::from_rgb(
            self.background_color[0],
            self.background_color[1],
            self.background_color[2],
        )
    }

    fn render_image(&self) -> image::DynamicImage {
        let canvas = self.get_canvas().size;

        let mut buf = image::ImageBuffer::from_pixel(
            canvas.x as u32,
            canvas.y as u32,
            image::Rgba(self.background_color32().to_array()),
        );

        for loaded_image in self.loaded_images.iter().filter(|image| image.visible) {
            let image_size = loaded_image.size();

            let resized_image = if loaded_image.scale == Vec2::ONE {
                Cow::Borrowed(&loaded_image.image)
            } else {
                Cow::Owned(image::imageops::resize(
                    &loaded_image.image,
                    image_size.x as u32,
                    image_size.y as u32,
                    image::imageops::FilterType::Lanczos3,
                ))
            };

            let resized_image = if loaded_image.rotation_degrees.abs() > 0.001 {
                Cow::Owned(studio::rotate_image(
                    &resized_image,
                    loaded_image.rotation_degrees,
                ))
            } else {
                resized_image
            };
            let visual_offset = loaded_image.visual_offset();
            let offset_x = visual_offset.x as i32;
            let offset_y = visual_offset.y as i32;

            let size_x = resized_image.width() as i32;
            let size_y = resized_image.height() as i32;

            let start_x = -offset_x.min(0);
            let start_y = -offset_y.min(0);

            let end_x = offset_x.max(0);
            let end_y = offset_y.max(0);

            let width_limit = (size_x - start_x).min(buf.width() as i32 - end_x);
            let height_limit = (size_y - start_y).min(buf.height() as i32 - end_y);

            if width_limit <= 0 || height_limit <= 0 {
                continue;
            }

            debug!(
                offset_x,
                offset_y,
                size_x,
                size_y,
                start_x,
                start_y,
                width_limit,
                height_limit,
                "calculated image position"
            );

            image::imageops::overlay(
                &mut buf,
                resized_image.as_ref(),
                i64::from(offset_x),
                i64::from(offset_y),
            );
        }

        buf.into()
    }

    fn render_snapshot(&self) -> RenderSnapshot {
        RenderSnapshot {
            canvas: self.get_canvas().size,
            background: self.background_color32().to_array(),
            layers: self
                .loaded_images
                .iter()
                .filter(|image| image.visible)
                .map(|image| RenderLayer {
                    image: image.image.clone(),
                    size: image.size(),
                    rotation_degrees: image.rotation_degrees,
                    visual_offset: image.visual_offset(),
                })
                .collect(),
        }
    }

    pub fn get_canvas(&self) -> &'static CanvasSize {
        &DEVICES[self.selected_device].modes[self.selected_mode].canvas_sizes
            [self.selected_canvas_size]
    }

    fn apply_actions(&mut self) {
        while let Ok(action) = self.rx.try_recv() {
            info!("got action: {action:?}");

            match action {
                Action::Error(err) => {
                    if let Some(job_id) = self.active_queue_job.take() {
                        let _ = self.job_queue.fail(job_id, err.to_string());
                    }
                    self.error = Some(err);

                    if let Some(manager) = self.transport_manager.take() {
                        spawn(async move {
                            if let Err(err) = manager.disconnect().await {
                                error!("could not disconnect from transport after error: {err}");
                            }
                        });
                    }
                }
                Action::DiscoveredDevices {
                    transport_index,
                    result,
                } => {
                    if transport_index == self.selected_transport_index {
                        self.discovering_devices = false;
                        match result {
                            Ok(devices) => {
                                if self
                                    .selected_transport_device
                                    .as_ref()
                                    .is_some_and(|selected| {
                                        !devices.iter().any(|device| &device.id == selected)
                                    })
                                {
                                    self.selected_transport_device = None;
                                }
                                self.discovered_devices = devices;
                            }
                            Err(error) => self.error = Some(error),
                        }
                    }
                }
                #[cfg(not(target_arch = "wasm32"))]
                Action::TransportReady { printer_id, result } => match result {
                    Ok(manager) => {
                        self.transport_manager = Some(manager.clone());
                        self.printer_connections
                            .insert(printer_id.clone(), manager.clone());
                        if self.printer_statuses.get(&printer_id)
                            == Some(&TransportStatus::Connected)
                            && self.printer_identity_loading.insert(printer_id.clone())
                        {
                            self.request_printer_identity(printer_id, manager);
                        }
                    }
                    Err(error) => {
                        self.transport_status = TransportStatus::Disconnected;
                        self.printer_statuses
                            .insert(printer_id.clone(), TransportStatus::Disconnected);
                        let _ = self
                            .job_queue
                            .set_printer_offline(&printer_id, error.to_string());
                        self.error = Some(error);
                    }
                },
                Action::TransportEvent { printer_id, event } => match event {
                    TransportEvent::Packet(packet) => {
                        if self.packets.len() >= 999 {
                            self.packets.pop_back();
                        }

                        self.packets.push_front(packet);
                    }
                    TransportEvent::TransportStatus(status) => {
                        self.transport_status = status;
                        self.printer_statuses.insert(printer_id.clone(), status);
                        if status == TransportStatus::Connected {
                            if let Some(manager) = self.printer_connections.get(&printer_id)
                                && self.printer_identity_loading.insert(printer_id.clone())
                            {
                                self.request_printer_identity(printer_id.clone(), manager.clone());
                            }
                        } else if status == TransportStatus::Disconnected
                            && self
                                .job_queue
                                .set_printer_offline(&printer_id, "printer disconnected")
                                .ok()
                                .flatten()
                                .is_some()
                        {
                            self.active_queue_jobs.remove(&printer_id);
                            self.active_queue_job = None;
                        }

                        if status == TransportStatus::Disconnecting {
                            self.device_status = None;
                        }
                        if status == TransportStatus::Disconnected
                            && let Some(manager) = self.printer_connections.remove(&printer_id)
                        {
                            self.printer_identities.remove(&printer_id);
                            self.printer_identity_errors.remove(&printer_id);
                            self.printer_identity_loading.remove(&printer_id);
                            self.printer_fallback_drafts.remove(&printer_id);
                            if self
                                .transport_manager
                                .as_ref()
                                .is_some_and(|current| Rc::ptr_eq(current, &manager))
                            {
                                self.transport_manager = None;
                            }
                            spawn(async move {
                                manager.reset_after_loss().await;
                            });
                        }
                    }
                    TransportEvent::DeviceStatus(status) => {
                        self.device_status = Some(status);
                    }
                    TransportEvent::JobStatus(status) => {
                        if let Some(job_id) = self.active_queue_jobs.get(&printer_id).copied() {
                            if matches!(status.job_state, JobState::Completed) {
                                let _ = self.job_queue.complete(job_id);
                                self.pending_print_jobs.remove(&job_id);
                                self.active_queue_jobs.remove(&printer_id);
                                self.active_queue_job = None;
                            } else if matches!(
                                status.job_state,
                                JobState::Aborted | JobState::Cancelled
                            ) {
                                let _ = self.job_queue.cancel(job_id);
                                self.active_queue_jobs.remove(&printer_id);
                                self.active_queue_job = None;
                            }
                        }
                        self.job_status = Some(status);
                    }
                    TransportEvent::Error(err) => {
                        if let Some(job_id) = self.active_queue_jobs.remove(&printer_id) {
                            let _ = self.job_queue.fail(job_id, err.to_string());
                        }
                        self.error = Some(err);
                    }
                },

                Action::PrinterIdentityLoaded { printer_id, result } => match result {
                    Ok(identity) => {
                        self.printer_identity_loading.remove(&printer_id);
                        self.printer_identity_errors.remove(&printer_id);
                        if identity.serial_number.is_some() {
                            self.printer_identities.insert(printer_id.clone(), identity);
                            let _ = self.job_queue.set_printer_online(&printer_id);
                        } else {
                            if self
                                .printer_fallback_names
                                .get(&printer_id)
                                .is_some_and(|name| !name.trim().is_empty())
                            {
                                let _ = self.job_queue.set_printer_online(&printer_id);
                            } else {
                                if let Some(profile_name) = unique_named_fallback_for_identity(
                                    &self.calibration_store,
                                    &identity,
                                ) {
                                    // A changed transport route cannot prove this is the same
                                    // physical serial-less printer. Suggest, but require the
                                    // operator to confirm, the only plausible saved identity.
                                    self.printer_fallback_drafts
                                        .insert(printer_id.clone(), profile_name);
                                }
                                self.printer_identity_errors.insert(
                                    printer_id.clone(),
                                    "printer did not provide a serial number; select a named calibration fallback"
                                        .into(),
                                );
                            }
                            self.printer_identities.insert(printer_id.clone(), identity);
                        }
                    }
                    Err(error) => {
                        self.printer_identity_loading.remove(&printer_id);
                        self.printer_identities.remove(&printer_id);
                        self.printer_identity_errors
                            .insert(printer_id, error.to_string());
                    }
                },

                Action::LoadedAvocadoPackets(packets) => self.avocado_debug_packets = Some(packets),
                Action::LoadedImage(res) => match res {
                    Ok(image) => {
                        self.add_new_artwork(image);
                    }
                    Err(err) => self.error = Some(err),
                },
                Action::ReplacedImage { image_id, result } => match result {
                    Ok(mut replacement) => {
                        let Some(index) = image_index_by_id(&self.loaded_images, &image_id) else {
                            continue;
                        };
                        let existing = &self.loaded_images[index];
                        let existing_id = existing.id.clone();
                        let template_slot = self
                            .template_placeholders
                            .iter()
                            .find(|placeholder| {
                                placeholder.assigned_image_id.as_ref() == Some(&existing_id)
                            })
                            .cloned();
                        let target_size = existing.rotated_size();
                        replacement.id = existing_id;
                        replacement.content_revision = existing.content_revision.wrapping_add(1);
                        replacement.name.clone_from(&existing.name);
                        replacement.offset = existing.offset;
                        replacement.rotation_degrees = existing.rotation_degrees;
                        replacement.locked = existing.locked;
                        replacement.visible = existing.visible;
                        replacement.enable_cutting = existing.enable_cutting;
                        replacement.template_fit = template_slot
                            .as_ref()
                            .map(|slot| slot.fit)
                            .unwrap_or(existing.template_fit);
                        if let Some(slot) = template_slot {
                            place_image_in_placeholder(&mut replacement, &slot);
                        } else {
                            let replacement_size = replacement.rotated_size();
                            replacement.scale = Vec2::new(
                                target_size.x / replacement_size.x.max(1.0),
                                target_size.y / replacement_size.y.max(1.0),
                            );
                        }
                        self.loaded_images[index] = replacement;
                    }
                    Err(error) => self.error = Some(error),
                },
                #[cfg(all(feature = "background-ml", not(target_arch = "wasm32")))]
                Action::BackgroundModelSelected(result) => match result {
                    Ok(path) => self.background_model_path = Some(path),
                    Err(error) => self.error = Some(error),
                },
                Action::BackgroundRemoved { image_id, result } => {
                    self.background_ml_running = false;
                    match result {
                        Ok(image) => {
                            let Some(index) = image_index_by_id(&self.loaded_images, &image_id)
                            else {
                                continue;
                            };
                            let loaded = &mut self.loaded_images[index];
                            loaded.image = image;
                            loaded.original_image = loaded.image.clone();
                            loaded.adjustments = ImageAdjustments::default();
                            loaded.content_revision = loaded.content_revision.wrapping_add(1);
                            loaded.refresh_texture();
                        }
                        Err(error) => self.error = Some(error),
                    }
                }
                Action::ImageAdjusted {
                    image_id,
                    source_revision,
                    adjustments,
                    image,
                } => {
                    self.image_processing.remove(&image_id);
                    if let Some(loaded) = self
                        .loaded_images
                        .iter_mut()
                        .find(|loaded| loaded.id == image_id)
                        .filter(|loaded| {
                            loaded.content_revision == source_revision
                                && loaded.adjustments == adjustments
                        })
                    {
                        loaded.image = image;
                        loaded.adjustments = adjustments;
                        loaded.content_revision = loaded.content_revision.wrapping_add(1);
                        loaded.refresh_texture();
                    }
                }
                Action::EdgeBackgroundRemoved {
                    image_id,
                    source_revision,
                    image,
                } => {
                    self.image_processing.remove(&image_id);
                    if let Some(loaded) = self
                        .loaded_images
                        .iter_mut()
                        .find(|loaded| loaded.id == image_id)
                        .filter(|loaded| loaded.content_revision == source_revision)
                    {
                        loaded.image = image;
                        loaded.original_image = loaded.image.clone();
                        loaded.adjustments = ImageAdjustments::default();
                        loaded.content_revision = loaded.content_revision.wrapping_add(1);
                        loaded.refresh_texture();
                    }
                }
                Action::LoadedLibraryImages(results) => {
                    let mut added = false;
                    for result in results {
                        match result {
                            Ok(image) => {
                                self.library.push(image);
                                added = true;
                            }
                            Err(error) => self.error = Some(error),
                        }
                    }
                    if added {
                        self.reset_library_cycle();
                    }
                }
                #[cfg(not(target_arch = "wasm32"))]
                Action::LoadedLibraryFolder { path } => {
                    let mut added = false;
                    if !self.library_folders.contains(&path) {
                        self.library_folders.push(path);
                        self.library_folders.sort();
                        added = true;
                    }
                    if added {
                        self.reset_library_cycle();
                    }
                    self.library_page = 0;
                    (self.library_disk_paths, self.library_has_more) = scan_library_page(
                        &self.library_folders,
                        self.library_page,
                        LIBRARY_PAGE_SIZE,
                    );
                }
                Action::LoadedDocument(result) => match result {
                    Ok((document, images)) => {
                        let settings = document.settings.clone();
                        let cutline_metadata = document.cutline_metadata.clone();
                        self.document_kind = document.kind;
                        self.template_placeholders = document.template_placeholders.clone();
                        self.selected_device = settings.selected_device.min(DEVICES.len() - 1);
                        self.selected_mode = settings
                            .selected_mode
                            .min(DEVICES[self.selected_device].modes.len() - 1);
                        self.selected_canvas_size = settings.selected_canvas_size.min(
                            DEVICES[self.selected_device].modes[self.selected_mode]
                                .canvas_sizes
                                .len()
                                - 1,
                        );
                        // Older documents may not have stored selection indexes.
                        for (device_index, device) in DEVICES.iter().enumerate() {
                            for (mode_index, mode) in device.modes.iter().enumerate() {
                                if let Some(canvas_index) =
                                    mode.canvas_sizes.iter().position(|canvas| {
                                        (canvas.size.x - document.canvas_size[0]).abs() < 0.5
                                            && (canvas.size.y - document.canvas_size[1]).abs() < 0.5
                                    })
                                {
                                    self.selected_device = device_index;
                                    self.selected_mode = mode_index;
                                    self.selected_canvas_size = canvas_index;
                                }
                            }
                        }
                        self.copies = settings.copies.clamp(1, 10);
                        self.cut_tuning.buffer = settings.cut_buffer;
                        self.cut_tuning.minimum_length = settings.cut_minimum_length;
                        self.cut_tuning.smoothing = settings.cut_smoothing;
                        self.cut_tuning.simplify = settings.cut_simplify;
                        self.cut_tuning.internal = settings.cut_internal;
                        self.cut_tuning.white_transparent = settings.cut_white_transparent;
                        self.perf_cut = settings.perf_cut;
                        self.perf_dash_mm = settings.perf_dash_mm;
                        self.perf_gap_mm = settings.perf_gap_mm;
                        self.peel_tabs = settings.peel_tabs;
                        self.pack_gap_mm = settings.pack_gap_mm;
                        self.pack_allow_rotation = settings.pack_allow_rotation;
                        self.overcut = OvercutSettings {
                            enabled: settings.overcut_enabled,
                            steps: settings.overcut_steps.clamp(1, 12),
                            maximum_angle_degrees: settings
                                .overcut_maximum_angle_degrees
                                .clamp(0.0, 90.0),
                            reach_pixels: settings.overcut_reach_mm.clamp(0.0, 10.0)
                                * DEVICES[self.selected_device].dpi
                                / 25.4,
                            snap_to_pixels: settings.overcut_snap_to_pixels,
                        };
                        self.background_color = document.background;
                        self.loaded_images = images;
                        self.cut_shapes = document
                            .cut_paths
                            .into_iter()
                            .map(|path| {
                                LineString::from(
                                    path.into_iter()
                                        .map(|point| (point[0], point[1]))
                                        .collect::<Vec<_>>(),
                                )
                            })
                            .collect();
                        self.cut_modes = vec![CutMode::Kiss; self.cut_shapes.len()];
                        self.cutline_owners = vec![None; self.cut_shapes.len()];
                        self.cutline_locked = vec![false; self.cut_shapes.len()];
                        self.peel_tab_positions = vec![None; self.cut_shapes.len()];
                        for metadata in cutline_metadata {
                            if let Some(mode) = self.cut_modes.get_mut(metadata.cut_path_index) {
                                *mode = metadata.cut_mode;
                            }
                            if let Some(owner) =
                                self.cutline_owners.get_mut(metadata.cut_path_index)
                            {
                                *owner = metadata.owner;
                            }
                            if let Some(locked) =
                                self.cutline_locked.get_mut(metadata.cut_path_index)
                            {
                                *locked = metadata.locked;
                            }
                            if let Some(position) =
                                self.peel_tab_positions.get_mut(metadata.cut_path_index)
                            {
                                *position = metadata.peel_tab_position;
                            }
                        }
                        self.manual_cut_shapes = self.cut_shapes.clone();
                        self.auto_cut_count = 0;
                        if let Some(index) = self
                            .material_profiles
                            .iter()
                            .position(|profile| profile.name == document.material.name)
                        {
                            self.material_profiles[index] = document.material;
                            self.selected_material = index;
                        } else {
                            self.material_profiles.push(document.material);
                            self.selected_material = self.material_profiles.len() - 1;
                        }
                        self.selected_images.clear();
                    }
                    Err(error) => self.error = Some(error),
                },
                Action::LoadedCutPaths(result) => {
                    match result {
                        Ok(mut paths) => {
                            let bounds = paths.iter().filter_map(LineString::bounding_rect).reduce(
                                |a, b| {
                                    GeoRect::new(
                                        Coord {
                                            x: a.min().x.min(b.min().x),
                                            y: a.min().y.min(b.min().y),
                                        },
                                        Coord {
                                            x: a.max().x.max(b.max().x),
                                            y: a.max().y.max(b.max().y),
                                        },
                                    )
                                },
                            );
                            if let Some(bounds) = bounds {
                                let canvas = self.get_canvas();
                                let source = Vec2::new(bounds.width(), bounds.height());
                                let scale_x = if source.x > f32::EPSILON {
                                    canvas.safe_area.x / source.x
                                } else {
                                    f32::INFINITY
                                };
                                let scale_y = if source.y > f32::EPSILON {
                                    canvas.safe_area.y / source.y
                                } else {
                                    f32::INFINITY
                                };
                                let scale = scale_x.min(scale_y).min(1.0);
                                if !scale.is_finite() {
                                    self.error =
                                        Some(anyhow::anyhow!("SVG paths have no measurable size"));
                                    continue;
                                }
                                let target_min = ((canvas.size - source * scale) / 2.0).to_pos2();
                                for path in &mut paths {
                                    for point in &mut path.0 {
                                        point.x = (point.x - bounds.min().x) * scale + target_min.x;
                                        point.y = (point.y - bounds.min().y) * scale + target_min.y;
                                    }
                                }
                            }
                            let added = paths.len();
                            self.manual_cut_shapes.extend(paths.clone());
                            self.cut_shapes.extend(paths);
                            self.cut_modes
                                .extend(std::iter::repeat_n(CutMode::Kiss, added));
                            self.cutline_owners.extend(std::iter::repeat_n(None, added));
                            self.cutline_locked
                                .extend(std::iter::repeat_n(false, added));
                            self.peel_tab_positions
                                .extend(std::iter::repeat_n(None, added));
                        }
                        Err(error) => self.error = Some(error),
                    }
                }
                Action::SendProgress { job_id, progress } => {
                    self.send_progress = Some(progress);
                    let _ = self
                        .job_queue
                        .update_progress(job_id, (progress * 100.0).round().clamp(0.0, 99.0) as u8);
                }
                Action::PrinterJobError { job_id, error } => {
                    let _ = self.job_queue.fail(job_id, error.to_string());
                    self.active_queue_jobs.retain(|_, active| *active != job_id);
                    self.error = Some(error);
                }
                Action::Cut {
                    generation_id,
                    source_geometry,
                    action,
                } => {
                    if !should_accept_cut_action(
                        self.active_cut_generation,
                        generation_id,
                        &source_geometry,
                        &self.current_cut_geometry(),
                    ) {
                        continue;
                    }
                    match action {
                        CutAction::Progress { completed, total } => {
                            self.cut_progress = Some((completed, total));
                        }
                        CutAction::Done(result) => {
                            self.active_cut_generation = None;
                            let manual_owners = (0..self.manual_cut_shapes.len())
                                .map(|index| {
                                    self.cutline_owners
                                        .get(self.auto_cut_count + index)
                                        .cloned()
                                        .flatten()
                                })
                                .collect::<Vec<_>>();
                            let manual_locks = (0..self.manual_cut_shapes.len())
                                .map(|index| {
                                    self.cutline_locked
                                        .get(self.auto_cut_count + index)
                                        .copied()
                                        .unwrap_or(false)
                                })
                                .collect::<Vec<_>>();
                            let manual_tab_positions = (0..self.manual_cut_shapes.len())
                                .map(|index| {
                                    self.peel_tab_positions
                                        .get(self.auto_cut_count + index)
                                        .copied()
                                        .flatten()
                                })
                                .collect::<Vec<_>>();
                            let next_modes = modes_after_regeneration(
                                &self.cut_modes,
                                self.auto_cut_count,
                                result.line_strings.len(),
                                self.manual_cut_shapes.len(),
                            );
                            self.has_intersections = result.has_intersections;
                            self.auto_cut_count = result.line_strings.len();
                            self.cut_shapes = result.line_strings;
                            self.cut_modes = next_modes;
                            self.cutline_owners = vec![None; self.auto_cut_count];
                            self.cutline_locked = vec![false; self.auto_cut_count];
                            self.peel_tab_positions = vec![None; self.auto_cut_count];
                            self.cut_shapes.extend(self.manual_cut_shapes.clone());
                            self.cutline_owners.extend(manual_owners);
                            self.cutline_locked.extend(manual_locks);
                            self.peel_tab_positions.extend(manual_tab_positions);
                            self.cut_progress = None;
                            self.off_canvas = result.off_canvas;
                            self.cut_validation_snapshot = None;
                        }
                    }
                }
                Action::PrintPrepared { capabilities, job } => {
                    self.print_preparing = false;
                    // Preparation is intentionally off the UI thread; only a
                    // fully encoded payload becomes eligible for dispatch.
                    let queue_id = self
                        .job_queue
                        .enqueue(JobSpec::named("Sapodilla sheet").requiring(capabilities));
                    self.pending_print_jobs.insert(queue_id, job);
                }
                Action::PrintRouteEncoded {
                    printer_id,
                    job_id,
                    payload,
                } => {
                    if let Some(session) = self.calibration_session.as_mut()
                        && let Some(slot) = [
                            CalibrationJobSlot::Primary,
                            CalibrationJobSlot::Second,
                            CalibrationJobSlot::Validation,
                        ]
                        .into_iter()
                        .find(|slot| session.queue_job(*slot) == Some(job_id))
                    {
                        session.plotter_sha1[CalibrationSession::slot_index(slot)] =
                            Some(hex::encode(sha1::Sha1::digest(&payload.plt)));
                        session.plotter_commands[CalibrationSession::slot_index(slot)] =
                            calibration_plotter_commands_from_plt(&payload.plt);
                    }
                    self.send_encoded_print_job(printer_id, job_id, payload)
                }
                Action::CalibrationJobPrepared {
                    run_id,
                    validation_generation,
                    physical_sheet_attempt,
                    slot,
                    spec,
                    result,
                } => {
                    let Some(session) = self.calibration_session.as_mut().filter(|session| {
                        session.accepts_job_result(
                            &run_id,
                            validation_generation,
                            slot,
                            physical_sheet_attempt,
                        )
                    }) else {
                        continue;
                    };
                    match result {
                        Ok(job) => {
                            session.image_sha1[CalibrationSession::slot_index(slot)] =
                                Some(job.image_hash.clone());
                            let queue_id = self.job_queue.enqueue(spec);
                            self.pending_print_jobs.insert(queue_id, job);
                            session.set_queue_job(slot, queue_id);
                            session.wizard.set_job_status(
                                slot,
                                crate::calibration::JobStatus::Queued,
                                current_timestamp_millis(),
                            );
                        }
                        Err(error) => {
                            session.wizard.set_job_status(
                                slot,
                                crate::calibration::JobStatus::Failed,
                                current_timestamp_millis(),
                            );
                            self.calibration_message = Some(error.to_string());
                        }
                    }
                }
                Action::CalibrationScanAnalyzed {
                    run_id,
                    validation_generation,
                    physical_sheet_attempt,
                    scan_request_generation,
                    slot,
                    file_name,
                    result,
                } => {
                    let Some(session) = self.calibration_session.as_mut().filter(|session| {
                        session.accepts_scan_result(
                            &run_id,
                            validation_generation,
                            slot,
                            physical_sheet_attempt,
                            scan_request_generation,
                        )
                    }) else {
                        continue;
                    };
                    let now = current_timestamp_millis();
                    match result {
                        Ok(import) => {
                            let report = import.report;
                            let observations = scan_report_observations(
                                &report,
                                match slot {
                                    ScanSlot::Training => &session.wizard.run_id,
                                    ScanSlot::Validation => "validation",
                                },
                            );
                            let all_quadrants = observations_cover_all_quadrants(&observations);
                            session.wizard.complete_scan_import(
                                slot,
                                &file_name,
                                all_quadrants,
                                observations,
                                now,
                            );
                            match slot {
                                ScanSlot::Training => {
                                    session.training_scan_report = Some(report);
                                    session.training_scan_preview_png = Some(import.preview_png);
                                    session.training_scan_preview_sha1 = Some(import.preview_sha1);
                                }
                                ScanSlot::Validation => {
                                    session.validation_scan_report = Some(report);
                                    session.validation_scan_preview_png = Some(import.preview_png);
                                    session.validation_scan_preview_sha1 =
                                        Some(import.preview_sha1);
                                }
                            }
                        }
                        Err(message) => {
                            session.wizard.fail_scan_import(slot, &message, now);
                            self.calibration_message = Some(message);
                        }
                    }
                }
                Action::CalibrationCandidateSolved {
                    run_id,
                    validation_generation,
                    result,
                } => {
                    let Some(session) = self.calibration_session.as_mut().filter(|session| {
                        session.accepts_async_result(&run_id, validation_generation)
                    }) else {
                        continue;
                    };
                    let now = current_timestamp_millis();
                    match result {
                        Ok(solution) => match session.baseline_mapping.compensated_for_mm_response(
                            solution.selected.forward_response,
                            f64::from(DEVICES[0].dpi) / 25.4,
                        ) {
                            Ok(mapping) if mapping.validate(TransformBounds::default()).is_ok() => {
                                session.wizard.mark_candidate_ready(
                                    &format!("{:?}", solution.selected.model),
                                    now,
                                );
                                session.candidate_mapping = Some(mapping);
                                session.candidate = Some(solution);
                            }
                            Ok(_) | Err(_) => {
                                session.wizard.mark_candidate_failed(
                                    "candidate produced an invalid plotter transform",
                                    now,
                                );
                            }
                        },
                        Err(message) => {
                            session.wizard.mark_candidate_failed(&message, now);
                            self.calibration_message = Some(message);
                        }
                    }
                }
                Action::CalibrationStoreImported { result } => match result {
                    Ok(store) => {
                        self.calibration_store = store;
                        self.calibration_message = Some("Calibration profiles imported.".into());
                    }
                    Err(message) => self.calibration_message = Some(message),
                },
                Action::CalibrationDeviceJobStarted {
                    queue_id,
                    device_job_id,
                } => {
                    if let Some(session) = self.calibration_session.as_mut()
                        && let Some(slot) = [
                            session.primary_queue_job,
                            session.second_queue_job,
                            session.validation_queue_job,
                        ]
                        .into_iter()
                        .position(|job_id| job_id == Some(queue_id))
                    {
                        if !session.device_job_ids.contains(&device_job_id) {
                            session.device_job_ids.push(device_job_id);
                        }
                        if !session.device_job_ids_by_slot[slot].contains(&device_job_id) {
                            session.device_job_ids_by_slot[slot].push(device_job_id);
                        }
                        if slot == CalibrationSession::slot_index(CalibrationJobSlot::Validation)
                            && !session.validation_device_job_ids.contains(&device_job_id)
                        {
                            session.validation_device_job_ids.push(device_job_id);
                        }
                    }
                }
            }
        }
        self.synchronize_calibration_jobs();
        self.dispatch_queued_jobs();
    }

    fn start_calibration(&mut self, printer_id: String) {
        let material = self.material_profiles[self.selected_material].clone();
        if material.blade_pressure == 0
            || material.blade_pressure > 100
            || material.perf_pressure == 0
            || material.perf_pressure > 100
            || !(1..=4).contains(&material.passes)
            || !(1..=10).contains(&material.speed)
        {
            self.calibration_message = Some(
                "Configure valid kiss-cut and through-cut pressures, 1–4 passes, and speed 1–10 for the selected material before calibrating."
                    .into(),
            );
            return;
        }
        let Some(printer_key) = calibration_key_for_printer(
            &self.printer_identities,
            &self.printer_fallback_names,
            &printer_id,
            &DEVICES[0].modes[1].canvas_sizes[0],
        ) else {
            self.calibration_message =
                Some("Wait for printer identity, or choose a stable named fallback first.".into());
            return;
        };
        let canvas = &DEVICES[0].modes[1].canvas_sizes[0];
        let baseline = resolve_routed_canvas_to_plotter(
            &self.calibration_store,
            &self.printer_identities,
            &self.printer_fallback_names,
            &printer_id,
            0,
            canvas,
        )
        .direct;
        let baseline_profile = self.calibration_store.active_profile(&printer_key);
        let baseline_profile_id = baseline_profile.map(|profile| profile.profile_id.clone());
        let baseline_profile_version = baseline_profile
            .map(|profile| u16::from(profile.version))
            .unwrap_or_else(default_calibration_profile_version);
        let now = current_timestamp_millis();
        let run_id = format!("cal-{}-{}", now, Uuid::new_v4().simple());
        let Ok(wizard) = CalibrationWizard::new(run_id, now) else {
            self.calibration_message = Some("Could not create a calibration run.".into());
            return;
        };
        let printer_label = calibration_printer_label(&printer_key);
        let media_label = calibration_media_label(&material);
        self.calibration_session = Some(CalibrationSession {
            printer_id: printer_id.clone(),
            printer_key,
            wizard,
            baseline_profile_id,
            baseline_profile_version,
            baseline_mapping: baseline,
            material,
            candidate: None,
            candidate_mapping: None,
            validation_metrics: None,
            training_scan_report: None,
            validation_scan_report: None,
            training_scan_preview_png: None,
            validation_scan_preview_png: None,
            training_scan_preview_sha1: None,
            validation_scan_preview_sha1: None,
            primary_queue_job: None,
            second_queue_job: None,
            validation_queue_job: None,
            historical_queue_job_ids: [None; 3],
            image_sha1: [None, None, None],
            plotter_sha1: [None, None, None],
            plotter_commands: std::array::from_fn(|_| Vec::new()),
            validation_generation: 0,
            device_job_ids: Vec::new(),
            validation_device_job_ids: Vec::new(),
            device_job_ids_by_slot: std::array::from_fn(|_| Vec::new()),
            physical_sheet_attempts: [0; 3],
            scan_request_generations: [0; 2],
        });
        self.calibration_ui_state = calibration_ui::CalibrationUiState::default();
        self.calibration_ui_state.selected_printer = printer_label;
        self.calibration_ui_state.selected_media = media_label;
        self.calibration_message = None;
    }

    fn synchronize_calibration_jobs(&mut self) {
        let stale_validation_job = self
            .calibration_session
            .as_mut()
            .and_then(CalibrationSession::clear_stale_candidate_evidence);
        if let Some(job_id) = stale_validation_job {
            let _ = self.job_queue.cancel(job_id);
            self.pending_print_jobs.remove(&job_id);
            self.active_queue_jobs.retain(|_, active| *active != job_id);
            if self.active_queue_job == Some(job_id) {
                self.active_queue_job = None;
            }
        }
        let Some(session) = self.calibration_session.as_mut() else {
            return;
        };
        let now = current_timestamp_millis();
        for slot in [
            CalibrationJobSlot::Primary,
            CalibrationJobSlot::Second,
            CalibrationJobSlot::Validation,
        ] {
            let Some(job_id) = session.queue_job(slot) else {
                continue;
            };
            let Some(job) = self.job_queue.job(job_id) else {
                continue;
            };
            let status = match job.status {
                QueueJobStatus::Queued => crate::calibration::JobStatus::Queued,
                QueueJobStatus::Running => crate::calibration::JobStatus::InProgress,
                QueueJobStatus::Done => crate::calibration::JobStatus::Completed,
                QueueJobStatus::Error | QueueJobStatus::Cancelled => {
                    crate::calibration::JobStatus::Failed
                }
            };
            let current = match slot {
                CalibrationJobSlot::Primary => session.wizard.primary_job,
                CalibrationJobSlot::Second => session.wizard.second_job,
                CalibrationJobSlot::Validation => session.wizard.validation_job,
            };
            if current != status {
                session.wizard.set_job_status(slot, status, now);
            }
        }
    }

    fn prepare_calibration_job(&mut self, slot: CalibrationJobSlot) {
        let Some(session) = self.calibration_session.as_ref() else {
            return;
        };
        if session.queue_job(slot).is_some_and(|job_id| {
            self.job_queue.job(job_id).is_some_and(|job| {
                matches!(job.status, QueueJobStatus::Queued | QueueJobStatus::Running)
            })
        }) {
            return;
        }
        if slot == CalibrationJobSlot::Validation && session.candidate_mapping.is_none() {
            self.calibration_message = Some("Compute a valid candidate before validation.".into());
            return;
        }
        if session.wizard.method.is_none() {
            self.calibration_message = Some("Choose a calibration method first.".into());
            return;
        }

        let now = current_timestamp_millis();
        let session = self
            .calibration_session
            .as_mut()
            .expect("calibration session was checked above");
        let previous_status = match slot {
            CalibrationJobSlot::Primary => session.wizard.primary_job,
            CalibrationJobSlot::Second => session.wizard.second_job,
            CalibrationJobSlot::Validation => session.wizard.validation_job,
        };
        let previous_second_status = session.wizard.second_job;
        let index = CalibrationSession::slot_index(slot);
        if previous_status != crate::calibration::JobStatus::NotStarted {
            session.physical_sheet_attempts[index] =
                session.physical_sheet_attempts[index].saturating_add(1);
        }
        if let Err(error) = session.wizard.begin_print_job(slot, now) {
            self.calibration_message = Some(error.to_string());
            return;
        }
        let mut reset_slots = vec![slot];
        if slot == CalibrationJobSlot::Primary {
            // A new primary sheet starts a new training dataset. Evidence from
            // an optional second sheet belongs to the old dataset too.
            reset_slots.push(CalibrationJobSlot::Second);
            if previous_second_status != crate::calibration::JobStatus::NotStarted {
                let second = CalibrationSession::slot_index(CalibrationJobSlot::Second);
                session.physical_sheet_attempts[second] =
                    session.physical_sheet_attempts[second].saturating_add(1);
            }
        }
        let mut stale_queue_jobs = Vec::new();
        for reset_slot in reset_slots {
            let reset_index = CalibrationSession::slot_index(reset_slot);
            stale_queue_jobs.extend(session.take_queue_job(reset_slot));
            session.historical_queue_job_ids[reset_index] = None;
            session.image_sha1[reset_index] = None;
            session.plotter_sha1[reset_index] = None;
            session.plotter_commands[reset_index].clear();
            let stale_device_ids = std::mem::take(&mut session.device_job_ids_by_slot[reset_index]);
            session
                .device_job_ids
                .retain(|id| !stale_device_ids.contains(id));
        }
        if slot == CalibrationJobSlot::Validation {
            session.validation_device_job_ids.clear();
            session.validation_scan_report = None;
            session.validation_scan_preview_png = None;
            session.validation_scan_preview_sha1 = None;
        } else if slot == CalibrationJobSlot::Primary {
            session.training_scan_report = None;
            session.training_scan_preview_png = None;
            session.training_scan_preview_sha1 = None;
        }

        let run_id = session.wizard.run_id.clone();
        let validation_generation = session.wizard.validation_generation;
        let physical_sheet_attempt = session.physical_sheet_attempts[index];
        let manifest_identity = if slot == CalibrationJobSlot::Validation {
            session.validation_manifest_identity()
        } else {
            session.manifest_identity_for_slot(slot)
        };
        let printer_id = session.printer_id.clone();
        let method = session.wizard.method.expect("method was checked above");
        let material = session.material.clone();
        let mapping_override = match slot {
            CalibrationJobSlot::Primary | CalibrationJobSlot::Second => {
                Some(session.baseline_mapping)
            }
            CalibrationJobSlot::Validation => session.candidate_mapping,
        };
        for job_id in stale_queue_jobs {
            let _ = self.job_queue.cancel(job_id);
            self.pending_print_jobs.remove(&job_id);
            self.active_queue_jobs
                .retain(|_, active_job_id| *active_job_id != job_id);
            if self.active_queue_job == Some(job_id) {
                self.active_queue_job = None;
            }
        }
        let tx = self.tx.clone();
        spawn_blocking(move || {
            let result = build_calibration_print_job(
                manifest_identity,
                method,
                slot,
                material,
                mapping_override,
            );
            let name = match slot {
                CalibrationJobSlot::Primary => "Calibration sheet",
                CalibrationJobSlot::Second => "Calibration sheet 2",
                CalibrationJobSlot::Validation => "Calibration validation sheet",
            };
            let spec = calibration_job_spec(name, printer_id);
            let _ = tx.send(Action::CalibrationJobPrepared {
                run_id,
                validation_generation,
                physical_sheet_attempt,
                slot,
                spec,
                result,
            });
        });
    }

    fn import_calibration_scan(&mut self, slot: ScanSlot) {
        let Some(session) = self.calibration_session.as_mut() else {
            return;
        };
        let run_id = session.wizard.run_id.clone();
        let manifest_identity = if slot == ScanSlot::Validation {
            session.validation_manifest_identity()
        } else {
            session.manifest_identity_for_slot(CalibrationJobSlot::Primary)
        };
        let Some(method) = session.wizard.method else {
            return;
        };
        if method != CalibrationMethod::FlatbedScanner {
            return;
        }
        session
            .wizard
            .begin_scan_import(slot, "choosing file…", current_timestamp_millis());
        let validation_generation = session.wizard.validation_generation;
        let scan_index = CalibrationSession::scan_slot_index(slot);
        session.scan_request_generations[scan_index] =
            session.scan_request_generations[scan_index].saturating_add(1);
        let scan_request_generation = session.scan_request_generations[scan_index];
        let physical_sheet_attempt = session.physical_sheet_attempts
            [CalibrationSession::slot_index(match slot {
                ScanSlot::Training => CalibrationJobSlot::Primary,
                ScanSlot::Validation => CalibrationJobSlot::Validation,
            })];
        match slot {
            ScanSlot::Training => {
                session.training_scan_report = None;
                session.training_scan_preview_png = None;
                session.training_scan_preview_sha1 = None;
            }
            ScanSlot::Validation => {
                session.validation_scan_report = None;
                session.validation_scan_preview_png = None;
                session.validation_scan_preview_sha1 = None;
            }
        }
        let tx = self.tx.clone();
        spawn(async move {
            let Some(file) = rfd::AsyncFileDialog::new()
                .add_filter("600 DPI color scan", &["png", "jpg", "jpeg"])
                .pick_file()
                .await
            else {
                let _ = tx.send(Action::CalibrationScanAnalyzed {
                    run_id,
                    validation_generation,
                    physical_sheet_attempt,
                    scan_request_generation,
                    slot,
                    file_name: "scan".into(),
                    result: Err("Scan import was cancelled.".into()),
                });
                return;
            };
            let file_name = file.file_name();
            let bytes = file.read().await;
            let manifest =
                calibration_manifest(manifest_identity, method, slot == ScanSlot::Validation);
            let result = manifest
                .map_err(|error| error.to_string())
                .and_then(|manifest| {
                    analyze_flatbed_scan(&bytes, &manifest, ScanAnalysisConfig::default())
                        .map_err(|error| error.to_string())
                })
                .and_then(|report| {
                    calibration_scan_preview(&bytes)
                        .map(|preview_png| CalibrationScanImport {
                            report,
                            preview_sha1: hex::encode(sha1::Sha1::digest(&preview_png)),
                            preview_png: preview_png.into(),
                        })
                        .map_err(|error| error.to_string())
                });
            let _ = tx.send(Action::CalibrationScanAnalyzed {
                run_id,
                validation_generation,
                physical_sheet_attempt,
                scan_request_generation,
                slot,
                file_name,
                result,
            });
        });
    }

    fn compute_calibration_candidate(&mut self) {
        let Some(session) = self.calibration_session.as_mut() else {
            return;
        };
        let Some(method) = session.wizard.method else {
            return;
        };
        let observations = session.wizard.training_observations();
        let run_id = session.wizard.run_id.clone();
        session.candidate = None;
        session.candidate_mapping = None;
        session
            .wizard
            .mark_candidate_computing(current_timestamp_millis());
        let validation_generation = session.wizard.validation_generation;
        let tx = self.tx.clone();
        spawn_blocking(move || {
            let result =
                solve_calibration(method, &observations, CalibrationPolicy::pixcut_s1_4x7())
                    .map_err(|error| error.to_string());
            let _ = tx.send(Action::CalibrationCandidateSolved {
                run_id,
                validation_generation,
                result,
            });
        });
    }

    fn evaluate_calibration_validation(&mut self) {
        let Some(session) = self.calibration_session.as_mut() else {
            return;
        };
        let Some(method) = session.wizard.method else {
            return;
        };
        let before = observation_error_metrics(&session.wizard.training_observations());
        let validation = session.wizard.validation_observations_for_evaluation();
        let after = observation_error_metrics(&validation);
        let (Some(before), Some(after)) = (before, after) else {
            session.wizard.set_validation_result(
                false,
                "not enough independent validation measurements",
                current_timestamp_millis(),
            );
            return;
        };
        let required_coverage_passed = validation_coverage_passed(method, &validation);
        let policy = CalibrationActivationPolicy::pixcut_s1_4x7();
        let maximum_error_passed = after.maximum_mm
            <= before.maximum_mm * policy.maximum_error_relative_factor
                + policy.maximum_error_absolute_slack_mm;
        let metrics = ValidationMetrics {
            before,
            after,
            required_coverage_passed,
            maximum_error_passed,
            normal_kiss_cut_passed: None,
        };
        let improvement = metrics.before.rms_mm > f64::EPSILON
            && (metrics.before.rms_mm - metrics.after.rms_mm) / metrics.before.rms_mm
                >= policy.minimum_rms_improvement;
        let p95_passed = metrics.after.p95_mm <= policy.maximum_p95_mm;
        let passed = metrics.is_valid()
            && required_coverage_passed
            && maximum_error_passed
            && improvement
            && p95_passed;
        let message = if passed {
            "Independent validation passed"
        } else if !improvement {
            "validation did not improve RMS error by at least 25%"
        } else if !required_coverage_passed {
            "validation targets do not cover the sheet"
        } else if !p95_passed {
            "validation improved, but remains outside the 0.50 mm p95 goal"
        } else {
            "validation maximum error materially worsened"
        };
        session.validation_metrics = Some(metrics);
        session
            .wizard
            .set_validation_result(passed, message, current_timestamp_millis());
    }

    fn activate_calibration_profile(&mut self) {
        let Some(session) = self.calibration_session.as_mut() else {
            return;
        };
        let (Some(solution), Some(mapping), Some(mut validation), Some(method)) = (
            session.candidate.as_ref(),
            session.candidate_mapping,
            session.validation_metrics.clone(),
            session.wizard.method,
        ) else {
            self.calibration_message = Some("Calibration result is incomplete.".into());
            return;
        };
        validation.normal_kiss_cut_passed = session.wizard.normal_kiss_cut_passed;
        let settings = calibration_cut_settings(&session.material, method, false);
        let validation_settings = calibration_cut_settings(&session.material, method, true);
        let profile = CalibrationProfile {
            version: crate::calibration::CALIBRATION_SCHEMA_VERSION,
            profile_id: format!("profile-{}", session.wizard.run_id),
            key: session.printer_key.clone(),
            method,
            canvas_to_plotter: mapping,
            baseline_mapping_id: session
                .baseline_profile_id
                .clone()
                .unwrap_or_else(|| crate::calibration::LEGACY_PIXCUT_S1_MAPPING_ID.into()),
            created_at: current_timestamp_millis(),
            validation: validation.clone(),
            measurement_settings: settings,
            validation_settings,
            selected_model: solution.selected.model,
            previous_profile_id: session.baseline_profile_id.clone(),
        };
        let mut run = match persisted_calibration_run(session, solution, validation.clone()) {
            Ok(run) => run,
            Err(error) => {
                self.calibration_message = Some(format!(
                    "Calibration evidence is incomplete; profile was not activated: {error}"
                ));
                return;
            }
        };
        if let Err(error) = run.validate_and_sanitize() {
            self.calibration_message = Some(format!(
                "Calibration evidence is invalid; profile was not activated: {error}"
            ));
            return;
        }
        match self.calibration_store.add_and_activate(profile) {
            Ok(_) => {
                self.calibration_store.runs.push(run);
                self.calibration_store
                    .runs
                    .sort_by_key(|run| std::cmp::Reverse(run.updated_at));
                self.calibration_store
                    .runs
                    .truncate(crate::calibration::MAX_CALIBRATION_RUNS);
                self.calibration_message = Some("Calibration profile activated.".into());
                self.calibration_session = None;
            }
            Err(error) => self.calibration_message = Some(error.to_string()),
        }
    }

    fn print_canvas(&mut self) {
        if self.print_preparing {
            return;
        }
        self.print_preparing = true;
        self.synchronize_cut_geometry();
        let mode_type = DEVICES[self.selected_device].modes[self.selected_mode].mode_type;
        let capabilities = if mode_type.has_cutting() {
            vec!["print", "cut"]
        } else {
            vec!["print"]
        };
        let snapshot = self.render_snapshot();
        let cut_shapes = self.cut_shapes.clone();
        let cut_modes = self.cut_modes.clone();
        let material = self.material_profiles[self.selected_material].clone();
        let perf_cut = self.perf_cut;
        let dpi = DEVICES[self.selected_device].dpi;
        let perf_dash = self.perf_dash_mm * dpi / 25.4;
        let perf_gap = self.perf_gap_mm * dpi / 25.4;
        let peel_tabs = self.peel_tabs;
        let peel_tab_positions = self.peel_tab_positions.clone();
        let overcut = self.overcut;
        let copies = self.copies;
        let device_index = self.selected_device;
        let mode_index = self.selected_mode;
        let canvas_index = self.selected_canvas_size;
        let tx = self.tx.clone();
        spawn_blocking(move || {
            let image = encode_image(&snapshot.render());
            let job = PendingPrintJob {
                encoded_image_len: image.len(),
                image_hash: hex::encode(sha1::Sha1::digest(&image)),
                encoded_image: image,
                created_at: current_timestamp_millis(),
                copies,
                device_index,
                mode_index,
                canvas_index,
                cut_shapes,
                cut_modes,
                material,
                perf_cut,
                perf_dash,
                perf_gap,
                peel_tabs,
                peel_tab_positions,
                overcut,
                calibration_phases: None,
                mapping_override: None,
            };
            let _ = tx.send(Action::PrintPrepared { capabilities, job });
        });
    }

    fn dispatch_queued_jobs(&mut self) {
        while let Some(route) = self.job_queue.route_next() {
            let Some(payload) = self.pending_print_jobs.get(&route.job_id).cloned() else {
                let _ = self
                    .job_queue
                    .fail(route.job_id, "queued print payload is unavailable");
                continue;
            };
            if !self.printer_connections.contains_key(&route.printer_id) {
                let _ = self
                    .job_queue
                    .set_printer_offline(&route.printer_id, "connection unavailable");
                continue;
            }
            let mode = &DEVICES[payload.device_index].modes[payload.mode_index];
            let canvas_size = mode.canvas_sizes[payload.canvas_index].clone();
            let plotter_mapping = payload
                .mapping_override
                .map(PlotterMapping::direct)
                .unwrap_or_else(|| {
                    self.routed_canvas_to_plotter(&route.printer_id, &payload, &canvas_size)
                });
            let has_cutting = mode.mode_type.has_cutting();
            self.active_queue_job = Some(route.job_id);
            self.active_queue_jobs
                .insert(route.printer_id.clone(), route.job_id);
            self.send_progress = None;
            let tx = self.tx.clone();
            let queue_id = route.job_id;
            let printer_id = route.printer_id;
            spawn_blocking(move || {
                let plt = if has_cutting {
                    if let Some(phases) = payload.calibration_phases.as_ref() {
                        encode_calibration_plt(phases, plotter_mapping, payload.material.passes)
                    } else {
                        encode_plt(
                            &payload.cut_shapes,
                            &payload.cut_modes,
                            plotter_mapping,
                            &canvas_size,
                            &payload.material,
                            payload.perf_cut,
                            payload.perf_dash,
                            payload.perf_gap,
                            payload.peel_tabs,
                            &payload.peel_tab_positions,
                            payload.overcut,
                        )
                    }
                } else {
                    Vec::new()
                };
                let mut packet_data = Vec::with_capacity(payload.encoded_image.len() + plt.len());
                packet_data.extend_from_slice(&plt);
                packet_data.extend_from_slice(&payload.encoded_image);
                let _ = tx.send(Action::PrintRouteEncoded {
                    printer_id,
                    job_id: queue_id,
                    payload: EncodedPrintJob {
                        source: payload,
                        plt,
                        packet_data,
                    },
                });
            });
        }
    }

    fn send_encoded_print_job(
        &mut self,
        printer_id: String,
        queue_id: u64,
        payload: EncodedPrintJob,
    ) {
        if self.active_queue_jobs.get(&printer_id) != Some(&queue_id) {
            return;
        }
        let Some(manager) = self.printer_connections.get(&printer_id).cloned() else {
            let _ = self
                .job_queue
                .set_printer_offline(&printer_id, "connection unavailable after encoding");
            self.active_queue_jobs.remove(&printer_id);
            self.active_queue_job = None;
            return;
        };
        let tx = self.tx.clone();
        spawn(async move {
            let result = async {
                    let source = payload.source;
                    let mode = &DEVICES[source.device_index].modes[source.mode_index];
                    let canvas_size = &mode.canvas_sizes[source.canvas_index];
                    let id = manager.next_message_id();
                    let data = if mode.mode_type.has_cutting() {
                        serde_json::json!({
                            "id": id, "method": "combo-job", "params": [
                                { "method": "print-job", "params": {
                                    "media-size": canvas_size.media_size, "media-type": canvas_size.media_type,
                                    "job-type": mode.mode_type.job_type(), "channel": mode.mode_type.channel(),
                                    "file-size": source.encoded_image_len, "document-format": 9,
                                    "document-name": format!("{}.jpeg", source.created_at),
                                    "hash-method": 1, "hash-value": source.image_hash,
                                    "user-account": "000000.00000000000000000000000000000000.0000",
                                    "job-send-time": source.created_at / 1000,
                                    "link-type": mode.mode_type.link_type(), "copies": source.copies
                                }},
                                { "method": "cut-job", "params": {
                                    "copies": source.copies, "media-size": canvas_size.media_size,
                                    "document-name": format!("{}.plt", source.created_at),
                                    "file-size": payload.plt.len(), "channel": mode.mode_type.channel(),
                                    "media-type": canvas_size.media_type, "job-type": mode.mode_type.job_type(),
                                    "document-format": 18, "job-send-time": source.created_at / 1000
                                }}
                            ]
                        })
                    } else {
                        serde_json::json!({ "id": id, "method": "print-job", "params": {
                            "media-size": canvas_size.media_size, "media-type": canvas_size.media_type,
                            "job-type": mode.mode_type.job_type(), "channel": mode.mode_type.channel(),
                            "file-size": source.encoded_image_len, "document-format": 9,
                            "document-name": format!("{}.jpeg", source.created_at),
                            "hash-method": 1, "hash-value": source.image_hash,
                            "user-account": "000000.00000000000000000000000000000000.0000",
                            "link-type": mode.mode_type.link_type(), "job-send-time": source.created_at / 1000,
                            "copies": source.copies
                        }})
                    };
                    let response = manager.wait_for_response(AvocadoPacket {
                        version: 100, content_type: ContentType::Message,
                        interaction_type: InteractionType::Request, encoding_type: EncodingType::Json,
                        encryption_mode: EncryptionMode::None, terminal_id: id, msg_number: id,
                        msg_package_total: 1, msg_package_num: 1, is_subpackage: false,
                        data: serde_json::to_vec(&data)?,
                    }).await?;
                    #[derive(Debug, Deserialize)]
                    #[serde(rename_all = "kebab-case")]
                    struct JobResult { #[serde(alias = "job_id")] job_id: u32 }
                    let device_job_id = response.as_json::<AvocadoResult<JobResult>>()
                        .ok_or_else(|| anyhow::anyhow!("printer returned an invalid job response"))?
                        .result.job_id;
                    let _ = tx.send(Action::CalibrationDeviceJobStarted {
                        queue_id,
                        device_job_id,
                    });
                    manager.send_data(device_job_id, &payload.packet_data, |total, sent| {
                        let _ = tx.send(Action::SendProgress {
                            job_id: queue_id, progress: sent as f32 / total as f32,
                        });
                    }).await?;
                    manager.poll_job(device_job_id).await?;
                    Ok::<(), anyhow::Error>(())
            }.await;
            if let Err(error) = result {
                let _ = tx.send(Action::PrinterJobError {
                    job_id: queue_id,
                    error,
                });
            }
        });
    }

    fn routed_canvas_to_plotter(
        &self,
        printer_id: &str,
        payload: &PendingPrintJob,
        canvas_size: &CanvasSize,
    ) -> PlotterMapping {
        resolve_routed_canvas_to_plotter(
            &self.calibration_store,
            &self.printer_identities,
            &self.printer_fallback_names,
            printer_id,
            payload.device_index,
            canvas_size,
        )
    }

    fn start_new_sheet(&mut self) {
        self.loaded_images.clear();
        self.document_kind = DocumentKind::Sheet;
        self.template_placeholders.clear();
        self.cutline_owners.clear();
        self.cutline_locked.clear();
        self.peel_tab_positions.clear();
        self.cut_shapes.clear();
        self.manual_cut_shapes.clear();
        self.cut_modes.clear();
        self.auto_cut_count = 0;
        self.cut_validation_snapshot = None;
        self.has_intersections = false;
        self.off_canvas = false;
        self.selected_images.clear();
        self.confirm_new_sheet = false;
    }

    fn request_new_sheet(&mut self) {
        if self.loaded_images.is_empty() && self.cut_shapes.is_empty() {
            self.start_new_sheet();
        } else {
            self.confirm_new_sheet = true;
        }
    }

    fn accent_color(&self) -> Color32 {
        self.appearance.accent.color()
    }

    fn appearance_settings(&mut self, ctx: &egui::Context) {
        if !self.show_settings {
            return;
        }

        let modal = Modal::new(Id::new("appearance_settings_modal")).show(ctx, |ui| {
            ui.set_width(390.0);
            theme::panel_title(ui, self.accent_color(), "Settings", "Appearance");
            theme::muted(
                ui,
                "Personalize the studio chrome without changing cut semantics.",
            );
            ui.add_space(8.0);
            egui::ScrollArea::vertical()
                .id_salt("appearance_settings_scroll")
                .auto_shrink([true, false])
                .max_height((ctx.content_rect().height() - 230.0).max(260.0))
                .show(ui, |ui| {
                    ui.strong("Appearance mode");
                    let mut preference = self.appearance.theme;
                    ui.horizontal(|ui| {
                        ui.selectable_value(
                            &mut preference,
                            egui::ThemePreference::System,
                            "System",
                        );
                        ui.selectable_value(&mut preference, egui::ThemePreference::Light, "Light");
                        ui.selectable_value(&mut preference, egui::ThemePreference::Dark, "Dark");
                    });
                    if preference != ctx.options(|options| options.theme_preference) {
                        ctx.set_theme(preference);
                    }
                    self.appearance.theme = preference;

                    ui.add_space(8.0);
                    ui.separator();
                    ui.strong("Accent color");
                    let current_rgb = self.appearance.accent.rgb();
                    ui.label(format!(
                        "{} · #{:02X}{:02X}{:02X}",
                        self.appearance.accent.name(),
                        current_rgb[0],
                        current_rgb[1],
                        current_rgb[2]
                    ));
                    ui.add_space(4.0);

                    egui::Grid::new("accent_preset_grid")
                        .num_columns(2)
                        .spacing([8.0, 8.0])
                        .show(ui, |ui| {
                            for (index, preset) in theme::AccentPreset::ALL.into_iter().enumerate()
                            {
                                if theme::accent_choice_button(ui, self.appearance.accent, preset)
                                    .clicked()
                                {
                                    self.appearance.accent = theme::AccentChoice::Preset(preset);
                                    self.custom_accent_rgb = preset.rgb();
                                }
                                if index % 2 == 1 {
                                    ui.end_row();
                                }
                            }
                        });

                    ui.add_space(6.0);
                    theme::card(ui.visuals().dark_mode).show(ui, |ui| {
                        ui.strong("Custom sRGB");
                        let mut custom_changed = ui
                            .color_edit_button_srgb(&mut self.custom_accent_rgb)
                            .on_hover_text("Choose an opaque custom accent color")
                            .changed();
                        ui.horizontal(|ui| {
                            for (label, channel) in
                                ["R", "G", "B"].into_iter().zip(&mut self.custom_accent_rgb)
                            {
                                ui.label(label);
                                custom_changed |= ui
                                    .add(egui::DragValue::new(channel).range(0..=255))
                                    .changed();
                            }
                        });
                        ui.monospace(format!(
                            "#{:02X}{:02X}{:02X}",
                            self.custom_accent_rgb[0],
                            self.custom_accent_rgb[1],
                            self.custom_accent_rgb[2]
                        ));
                        if custom_changed {
                            self.appearance.accent =
                                theme::AccentChoice::Custom(self.custom_accent_rgb);
                        }
                    });

                    ui.add_space(8.0);
                    ui.strong("Live preview");
                    theme::card(ui.visuals().dark_mode).show(ui, |ui| {
                        ui.horizontal(|ui| {
                            let accent = self.accent_color();
                            let _ = theme::primary_button(ui, accent, "Primary");
                            let _ = theme::secondary_button(ui, accent, "Secondary");
                            let mut selected = true;
                            let _ = theme::toolbar_icon_toggle(
                                ui,
                                accent,
                                &mut selected,
                                crate::icons::GRID,
                                "Selected control preview",
                                "Selected toolbar control",
                            );
                        });
                        theme::spectrum_rule(ui, self.accent_color());
                    });
                });
            ui.add_space(8.0);
            ui.separator();
            ui.horizontal(|ui| {
                if theme::secondary_button(ui, self.accent_color(), "Reset to Sapodilla Pink")
                    .clicked()
                {
                    self.appearance.accent = theme::AccentChoice::default();
                    self.custom_accent_rgb = theme::DEFAULT_ACCENT_RGB;
                }
                if ui.button("Close").clicked() {
                    self.show_settings = false;
                    ui.close();
                }
            });
        });
        if modal.should_close() {
            self.show_settings = false;
        }
    }

    fn calibration_profile_manager(&mut self, ctx: &egui::Context) {
        if !self.show_calibration_profiles {
            return;
        }
        let mut open = self.show_calibration_profiles;
        let mut activate = None;
        let mut reset = None;
        let mut rollback = None;
        let mut import = false;
        let mut export = false;
        let mut export_run = None;
        egui::Window::new("Calibration profiles")
            .open(&mut open)
            .default_width(620.0)
            .min_width(360.0)
            .show(ctx, |ui| {
                ui.heading("Print-to-cut calibration profiles");
                theme::muted(
                    ui,
                    "Profiles are isolated by printer identity, firmware, and media.",
                );
                if let Some(message) = &self.calibration_message {
                    ui.label(message);
                }
                ui.horizontal_wrapped(|ui| {
                    if ui.button("Import JSON…").clicked() {
                        import = true;
                    }
                    if ui.button("Export JSON…").clicked() {
                        export = true;
                    }
                    if ui.button("Copy JSON").clicked()
                        && let Ok(json) = serde_json::to_string_pretty(&self.calibration_store)
                    {
                        ui.ctx().copy_text(json);
                    }
                });
                ui.separator();
                if self.calibration_store.profiles.is_empty() {
                    ui.label("No completed calibration profiles yet.");
                }
                egui::ScrollArea::vertical()
                    .max_height((ctx.content_rect().height() - 240.0).max(220.0))
                    .show(ui, |ui| {
                        for profile in self.calibration_store.profiles.clone() {
                            let active = self
                                .calibration_store
                                .active_profile(&profile.key)
                                .is_some_and(|value| value.profile_id == profile.profile_id);
                            ui.group(|ui| {
                                ui.horizontal_wrapped(|ui| {
                                    ui.strong(&profile.profile_id);
                                    if active {
                                        ui.label("Active");
                                    }
                                });
                                ui.small(format!(
                                    "{} · firmware {} · {:?}",
                                    profile.key.model,
                                    profile.key.firmware_revision,
                                    profile.method
                                ));
                                ui.small(format!(
                                    "Validation RMS {:.3} → {:.3} mm · p95 {:.3} mm",
                                    profile.validation.before.rms_mm,
                                    profile.validation.after.rms_mm,
                                    profile.validation.after.p95_mm
                                ));
                                ui.horizontal_wrapped(|ui| {
                                    if !active && ui.small_button("Activate").clicked() {
                                        activate = Some(profile.profile_id.clone());
                                    }
                                    if active && ui.small_button("Revert to previous").clicked() {
                                        rollback = Some(profile.key.clone());
                                    }
                                    if active && ui.small_button("Use stock mapping").clicked() {
                                        reset = Some(profile.key.clone());
                                    }
                                });
                            });
                        }
                    });
                if !self.calibration_store.runs.is_empty() {
                    ui.separator();
                    ui.strong("Run reports");
                    for run in self.calibration_store.runs.iter().take(10) {
                        ui.horizontal_wrapped(|ui| {
                            ui.label(format!(
                                "{} · {:?} · {:?}",
                                run.run_id, run.method, run.state
                            ));
                            if ui.small_button("Export report…").clicked() {
                                export_run = Some(run.clone());
                            }
                        });
                    }
                }
            });
        self.show_calibration_profiles = open;

        if let Some(profile_id) = activate {
            self.calibration_message = self
                .calibration_store
                .activate(&profile_id)
                .err()
                .map(|error| error.to_string())
                .or_else(|| Some("Calibration profile activated.".into()));
        }
        if let Some(key) = rollback {
            self.calibration_message = match self.calibration_store.rollback(&key) {
                Ok(Some(_)) => Some("Previous calibration restored.".into()),
                Ok(None) => Some("No active calibration to revert.".into()),
                Err(error) => Some(error.to_string()),
            };
        }
        if let Some(key) = reset {
            self.calibration_store.reset(&key);
            self.calibration_message = Some("Stock mapping restored.".into());
        }
        if export {
            let store = self.calibration_store.clone();
            spawn(async move {
                let Some(file) = rfd::AsyncFileDialog::new()
                    .set_file_name("sapodilla-calibration-profiles.json")
                    .save_file()
                    .await
                else {
                    return;
                };
                if let Ok(json) = serde_json::to_vec_pretty(&store) {
                    let _ = file.write(&json).await;
                }
            });
        }
        if let Some(run) = export_run {
            spawn(async move {
                let file_name = format!("sapodilla-calibration-{}.json", run.run_id);
                let Some(file) = rfd::AsyncFileDialog::new()
                    .set_file_name(&file_name)
                    .save_file()
                    .await
                else {
                    return;
                };
                if let Ok(json) = serde_json::to_vec_pretty(&run) {
                    let _ = file.write(&json).await;
                }
            });
        }
        if import {
            let tx = self.tx.clone();
            spawn(async move {
                let Some(file) = rfd::AsyncFileDialog::new()
                    .add_filter("Calibration profiles", &["json"])
                    .pick_file()
                    .await
                else {
                    return;
                };
                let result = serde_json::from_slice::<CalibrationStore>(&file.read().await)
                    .map_err(|error| error.to_string())
                    .and_then(|store| store.sanitize().map_err(|error| error.to_string()));
                let _ = tx.send(Action::CalibrationStoreImported { result });
            });
        }
    }

    fn menu(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let previous_theme = ctx.options(|options| options.theme_preference);
        egui::widgets::global_theme_preference_switch(ui);
        let current_theme = ctx.options(|options| options.theme_preference);
        if current_theme != previous_theme {
            self.appearance.theme = current_theme;
        }

        ui.separator();

        let is_web = cfg!(target_arch = "wasm32");
        ui.menu_button("File", |ui| {
            let new_shortcut = KeyboardShortcut::new(Modifiers::COMMAND, egui::Key::N);
            if ui
                .add(
                    egui::Button::new("New Sheet")
                        .shortcut_text(ctx.format_shortcut(&new_shortcut)),
                )
                .clicked()
            {
                self.request_new_sheet();
                ui.close();
            }
            let open_shortcut = KeyboardShortcut::new(Modifiers::COMMAND, egui::Key::O);
            if ui
                .add(egui::Button::new("Open…").shortcut_text(ctx.format_shortcut(&open_shortcut)))
                .clicked()
            {
                self.open_document(ctx);
                ui.close();
            }
            ui.menu_button("Save As", |ui| {
                if ui.button("Sticker project (.sapodilla)").clicked() {
                    self.save_document(DocumentKind::Sticker);
                    ui.close();
                }
                if ui.button("Sheet project (.sapodilla)").clicked() {
                    self.save_document(DocumentKind::Sheet);
                    ui.close();
                }
                if ui.button("Template project (.sapodilla)").clicked() {
                    self.save_document(DocumentKind::Template);
                    ui.close();
                }
            });
            ui.menu_button("Export", |ui| {
                if ui.button("Artwork PNG…").clicked() {
                    self.export_png();
                    ui.close();
                }
                if ui.button("Artwork PDF…").clicked() {
                    self.export_pdf();
                    ui.close();
                }
                if ui.button("Cutlines SVG…").clicked() {
                    self.export_cut_svg();
                    ui.close();
                }
                if ui.button("Plotter PLT…").clicked() {
                    self.export_plt();
                    ui.close();
                }
                if ui.button("Toolpath debug SVG…").clicked() {
                    self.export_toolpath_debug_svg();
                    ui.close();
                }
            });
            if !is_web {
                ui.separator();
                if ui.button("Quit").clicked() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        });

        let settings_shortcut = KeyboardShortcut::new(Modifiers::COMMAND, egui::Key::Comma);
        ui.menu_button("Settings", |ui| {
            if ui
                .add(
                    egui::Button::new("Appearance…")
                        .shortcut_text(ctx.format_shortcut(&settings_shortcut)),
                )
                .clicked()
            {
                self.custom_accent_rgb = self.appearance.accent.rgb();
                self.show_settings = true;
                ui.close();
            }
            if ui.button("Calibration…").clicked() {
                if self.calibration_session.is_some() {
                    self.calibration_ui_state.open = true;
                } else if let Some(printer_id) = self
                    .job_queue
                    .printers()
                    .iter()
                    .find(|printer| self.printer_connections.contains_key(&printer.id))
                    .map(|printer| printer.id.clone())
                {
                    self.start_calibration(printer_id);
                } else {
                    self.calibration_message =
                        Some("Connect a PixCut S1 before starting calibration.".into());
                }
                ui.close();
            }
            if ui.button("Calibration profiles…").clicked() {
                self.show_calibration_profiles = true;
                ui.close();
            }
        });

        let image_shortcut =
            KeyboardShortcut::new(Modifiers::COMMAND | Modifiers::SHIFT, egui::Key::U);
        if ui.input_mut(|i| i.consume_shortcut(&image_shortcut)) {
            self.upload_image(ctx);
        }

        ui.menu_button("Canvas", |ui| {
            let btn =
                egui::Button::new("Add Image").shortcut_text(ctx.format_shortcut(&image_shortcut));

            if ui.add(btn).clicked() {
                self.upload_image(ctx);
            }
            if ui.button("Import SVG Cutlines…").clicked() {
                self.import_svg();
                ui.close();
            }
        });

        ui.menu_button("Connection", |ui| {
            ui.menu_button("Transport", |ui| {
                for (index, transport) in self.transport_names.iter().enumerate() {
                    if ui
                        .radio(self.selected_transport_index == index, transport.as_ref())
                        .clicked()
                    {
                        self.selected_transport_index = index;
                        self.discovered_devices.clear();
                        self.selected_transport_device = None;
                        self.discovering_devices = false;
                    }
                }
            });
        });

        ui.menu_button("Debug Tools", |ui| {
            ui.checkbox(&mut self.showing_packet_log, "Show Packet Log");
            ui.checkbox(
                &mut self.showing_avocado_packet_debug,
                "Saved Packet Debugger",
            );

            if let Some(manager) = &self.transport_manager
                && ui.button("Send Get Prop Packet").clicked()
            {
                let manager = manager.clone();
                let id = manager.next_message_id();

                spawn(async move {
                    let packet = manager
                        .wait_for_response(AvocadoPacket {
                            version: 100,
                            content_type: crate::protocol::ContentType::Message,
                            interaction_type: crate::protocol::InteractionType::Request,
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
                                    "model",
                                    "mac-address",
                                    "serial-number",
                                    "sn-pcba",
                                    "firmware-revision",
                                    "hardware-revision",
                                    "bt-phone-mac",
                                    "printer-state",
                                    "printer-sub-state",
                                    "printer-state-alerts",
                                    "auto-off-interval",
                                    "media-size",
                                ]
                            }))
                            .unwrap(),
                        })
                        .await
                        .unwrap();

                    info!("got info packet: {packet:?}");
                });
            }

            if let Some(manager) = &self.transport_manager
                && ui.button("Send Resume Printer").clicked()
            {
                let manager = manager.clone();
                let id = manager.next_message_id();

                spawn(async move {
                    let packet = manager
                        .wait_for_response(AvocadoPacket {
                            version: 100,
                            content_type: crate::protocol::ContentType::Message,
                            interaction_type: crate::protocol::InteractionType::Request,
                            encoding_type: EncodingType::Json,
                            encryption_mode: EncryptionMode::None,
                            terminal_id: id,
                            msg_number: id,
                            msg_package_total: 1,
                            msg_package_num: 1,
                            is_subpackage: false,
                            data: serde_json::to_vec(&serde_json::json!({
                                "id" : id,
                                "method" : "resume-printer",
                                "params" : []
                            }))
                            .unwrap(),
                        })
                        .await
                        .unwrap();

                    info!("got resume packet: {packet:?}");
                });
            }

            ui.separator();

            if ui.button("Export Canvas").clicked() {
                let im = self.render_image();

                let mut buf = Vec::with_capacity(1024 * 1024);
                let encoder = image::codecs::png::PngEncoder::new(&mut buf);
                encoder
                    .write_image(
                        im.to_rgba8().as_bytes(),
                        im.width(),
                        im.height(),
                        image::ExtendedColorType::Rgba8,
                    )
                    .unwrap();

                spawn(async move {
                    let Some(handle) = rfd::AsyncFileDialog::new()
                        .set_file_name("canvas.png")
                        .save_file()
                        .await
                    else {
                        return;
                    };

                    if let Err(err) = handle.write(&buf).await {
                        error!("could not write canvas image: {err}");
                    }
                });
            }
        });
    }

    fn selection_inspector(&mut self, ui: &mut egui::Ui) {
        self.selected_images
            .retain(|index| *index < self.loaded_images.len());
        if self.selected_images.is_empty() {
            ui.label("Select artwork on the sheet to edit it.");
            return;
        }

        ui.separator();
        ui.heading(if self.selected_images.len() == 1 {
            "Selection"
        } else {
            "Multi-selection"
        });
        let canvas = self.get_canvas().size;
        ui.label("Align to sheet");
        ui.horizontal_wrapped(|ui| {
            if theme::icon_button(
                ui,
                crate::icons::ALIGN_LEFT,
                "Align artwork left",
                "Align selected artwork to the left edge of the sheet",
            )
            .clicked()
            {
                for &index in &self.selected_images {
                    align_image_to_sheet(
                        &mut self.loaded_images[index],
                        canvas,
                        SheetAlignment::Left,
                    );
                }
            }
            if theme::icon_button(
                ui,
                crate::icons::ALIGN_CENTER_HORIZONTAL,
                "Center artwork horizontally",
                "Center selected artwork horizontally on the sheet",
            )
            .clicked()
            {
                for &index in &self.selected_images {
                    align_image_to_sheet(
                        &mut self.loaded_images[index],
                        canvas,
                        SheetAlignment::CenterX,
                    );
                }
            }
            if theme::icon_button(
                ui,
                crate::icons::ALIGN_RIGHT,
                "Align artwork right",
                "Align selected artwork to the right edge of the sheet",
            )
            .clicked()
            {
                for &index in &self.selected_images {
                    align_image_to_sheet(
                        &mut self.loaded_images[index],
                        canvas,
                        SheetAlignment::Right,
                    );
                }
            }
            if theme::icon_button(
                ui,
                crate::icons::ALIGN_TOP,
                "Align artwork top",
                "Align selected artwork to the top edge of the sheet",
            )
            .clicked()
            {
                for &index in &self.selected_images {
                    align_image_to_sheet(
                        &mut self.loaded_images[index],
                        canvas,
                        SheetAlignment::Top,
                    );
                }
            }
            if theme::icon_button(
                ui,
                crate::icons::ALIGN_CENTER_VERTICAL,
                "Center artwork vertically",
                "Center selected artwork vertically on the sheet",
            )
            .clicked()
            {
                for &index in &self.selected_images {
                    align_image_to_sheet(
                        &mut self.loaded_images[index],
                        canvas,
                        SheetAlignment::Middle,
                    );
                }
            }
            if theme::icon_button(
                ui,
                crate::icons::ALIGN_BOTTOM,
                "Align artwork bottom",
                "Align selected artwork to the bottom edge of the sheet",
            )
            .clicked()
            {
                for &index in &self.selected_images {
                    align_image_to_sheet(
                        &mut self.loaded_images[index],
                        canvas,
                        SheetAlignment::Bottom,
                    );
                }
            }
        });

        if self.document_kind == DocumentKind::Template && !self.template_placeholders.is_empty() {
            let selected = (self.selected_images.len() == 1)
                .then(|| self.loaded_images[self.selected_images[0]].id.clone());
            let mut assign = None;
            ui.collapsing("Template slots", |ui| {
                for (slot_index, slot) in self.template_placeholders.iter().enumerate() {
                    ui.horizontal(|ui| {
                        ui.label(format!("{} ({:?})", slot.name, slot.fit));
                        if slot.assigned_image_id.is_some() {
                            ui.small("Assigned");
                        } else if ui
                            .add_enabled(
                                selected.is_some(),
                                egui::Button::new("Assign selected artwork"),
                            )
                            .clicked()
                        {
                            assign = Some(slot_index);
                        }
                    });
                }
            });
            if let (Some(slot_index), Some(image_id)) = (assign, selected) {
                for slot in &mut self.template_placeholders {
                    if slot.assigned_image_id.as_ref() == Some(&image_id) {
                        slot.assigned_image_id = None;
                    }
                }
                let slot = &mut self.template_placeholders[slot_index];
                slot.assigned_image_id = Some(image_id.clone());
                if let Some(image) = self
                    .loaded_images
                    .iter_mut()
                    .find(|image| image.id == image_id)
                {
                    image.template_fit = slot.fit;
                    place_image_in_placeholder(image, slot);
                    image.locked = true;
                }
            }
        }

        if self.selected_images.len() == 1 {
            let index = self.selected_images[0];
            let image_processing = self
                .image_processing
                .contains(&self.loaded_images[index].id);
            let mut adjustment_request = None;
            let mut edge_removal_request = None;
            let template_slot_index = self.template_placeholders.iter().position(|placeholder| {
                placeholder.assigned_image_id.as_ref() == Some(&self.loaded_images[index].id)
            });
            if let Some(slot_index) = template_slot_index {
                let slot = &mut self.template_placeholders[slot_index];
                ui.label(format!("Template slot: {} ({:?})", slot.name, slot.fit));
                let previous_fit = slot.fit;
                egui::ComboBox::from_label("Slot fit")
                    .selected_text(placeholder_fit_label(slot.fit))
                    .show_ui(ui, |ui| {
                        for fit in [
                            PlaceholderFit::Contain,
                            PlaceholderFit::Cover,
                            PlaceholderFit::Stretch,
                        ] {
                            ui.selectable_value(&mut slot.fit, fit, placeholder_fit_label(fit));
                        }
                    });
                if slot.fit != previous_fit {
                    let slot = slot.clone();
                    if let Some(image) = self
                        .loaded_images
                        .iter_mut()
                        .find(|image| slot.assigned_image_id.as_ref() == Some(&image.id))
                    {
                        image.template_fit = slot.fit;
                        place_image_in_placeholder(image, &slot);
                    }
                }
            }
            if ui
                .button(if template_slot_index.is_some() {
                    "Replace slot artwork…"
                } else {
                    "Replace artwork…"
                })
                .clicked()
            {
                self.replace_image(ui.ctx(), index);
            }
            ui.collapsing("AI background removal", |ui| {
                #[cfg(all(feature = "background-ml", not(target_arch = "wasm32")))]
                {
                    ui.horizontal(|ui| {
                        if ui.button("Choose ONNX model…").clicked() {
                            self.choose_background_model();
                        }
                        let model = self
                            .background_model_path
                            .as_deref()
                            .and_then(std::path::Path::file_name)
                            .and_then(std::ffi::OsStr::to_str)
                            .unwrap_or("No model selected");
                        ui.small(model);
                    });
                    if ui
                        .add_enabled(
                            !self.background_ml_running,
                            egui::Button::new(if self.background_ml_running {
                                "Removing background…"
                            } else {
                                "Remove background with AI"
                            }),
                        )
                        .clicked()
                    {
                        self.remove_background_ml(index);
                    }
                }
                #[cfg(not(all(feature = "background-ml", not(target_arch = "wasm32"))))]
                ui.small("Available in native builds with the background-ml feature enabled.");
            });
            let image = &mut self.loaded_images[index];
            if template_slot_index.is_none() {
                egui::ComboBox::from_label("Template slot fit")
                    .selected_text(placeholder_fit_label(image.template_fit))
                    .show_ui(ui, |ui| {
                        for fit in [
                            PlaceholderFit::Contain,
                            PlaceholderFit::Cover,
                            PlaceholderFit::Stretch,
                        ] {
                            ui.selectable_value(
                                &mut image.template_fit,
                                fit,
                                placeholder_fit_label(fit),
                            );
                        }
                    });
                ui.small("Used if this artwork is saved as a template slot.");
            }
            ui.horizontal(|ui| {
                let label = ui.label("Selected artwork name");
                ui.text_edit_singleline(&mut image.name)
                    .labelled_by(label.id);
            });
            ui.label("Position and size");
            ui.horizontal(|ui| {
                ui.spacing_mut().interact_size.x = 72.0;
                let x_label = ui.monospace("X:");
                ui.add(views::px_slider(
                    &mut image.offset.x,
                    DEVICES[self.selected_device].dpi,
                    (-image.sized_texture.size.x * 2.0)
                        ..=(canvas.x + image.sized_texture.size.x * 2.0),
                ))
                .labelled_by(x_label.id);
                let y_label = ui.monospace("Y:");
                ui.add(views::px_slider(
                    &mut image.offset.y,
                    DEVICES[self.selected_device].dpi,
                    (-image.sized_texture.size.y * 2.0)
                        ..=(canvas.y + image.sized_texture.size.y * 2.0),
                ))
                .labelled_by(y_label.id);
            });
            ui.horizontal(|ui| {
                ui.spacing_mut().interact_size.x = 72.0;
                let width_label = ui.monospace("W:");
                let mut width = image.size().x;
                ui.add(views::px_slider(
                    &mut width,
                    DEVICES[self.selected_device].dpi,
                    1.0..=(canvas.x * 10.0),
                ))
                .labelled_by(width_label.id);
                if width != image.size().x {
                    let new_scale = if image.scale_locked {
                        width / image.size().x * image.scale
                    } else {
                        Vec2 {
                            x: width / image.size().x * image.scale.x,
                            ..image.scale
                        }
                    };
                    image.rescale(new_scale);
                }
                let proportions_label = if image.scale_locked {
                    "Unlock artwork proportions"
                } else {
                    "Lock artwork proportions"
                };
                if theme::icon_toggle(
                    ui,
                    if image.scale_locked {
                        crate::icons::LINK
                    } else {
                        crate::icons::LINK_BREAK
                    },
                    image.scale_locked,
                    proportions_label,
                    proportions_label,
                )
                .clicked()
                {
                    image.scale_locked = !image.scale_locked;
                }
                let height_label = ui.monospace("H:");
                let mut height = image.size().y;
                ui.add(views::px_slider(
                    &mut height,
                    DEVICES[self.selected_device].dpi,
                    1.0..=(canvas.y * 10.0),
                ))
                .labelled_by(height_label.id);
                if height != image.size().y {
                    let new_scale = if image.scale_locked {
                        height / image.size().y * image.scale
                    } else {
                        Vec2 {
                            y: height / image.size().y * image.scale.y,
                            ..image.scale
                        }
                    };
                    image.rescale(new_scale);
                }
            });
            ui.add(
                egui::Slider::new(&mut image.rotation_degrees, -180.0..=180.0)
                    .suffix("°")
                    .text("Rotation"),
            );
            ui.horizontal(|ui| {
                ui.checkbox(&mut image.visible, "Visible");
                ui.checkbox(&mut image.locked, "Lock");
                ui.checkbox(&mut image.enable_cutting, "Cut");
            });
            ui.horizontal(|ui| {
                if ui.button("Flip horizontal").clicked() {
                    image.flip_horizontal();
                }
                if ui.button("Flip vertical").clicked() {
                    image.flip_vertical();
                }
            });
            ui.collapsing("Image adjustments", |ui| {
                ui.add(
                    egui::Slider::new(&mut image.adjustments.brightness, -100..=100)
                        .text("Brightness"),
                );
                ui.add(
                    egui::Slider::new(&mut image.adjustments.contrast, -100.0..=100.0)
                        .text("Contrast"),
                );
                ui.add(
                    egui::Slider::new(&mut image.adjustments.saturation, -100.0..=100.0)
                        .text("Saturation"),
                );
                ui.add(
                    egui::Slider::new(&mut image.adjustments.hue_degrees, -180..=180)
                        .suffix("°")
                        .text("Hue"),
                );
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(!image_processing, egui::Button::new("Apply"))
                        .clicked()
                    {
                        adjustment_request = Some((
                            image.id.clone(),
                            image.content_revision,
                            image.original_image.clone(),
                            image.adjustments,
                        ));
                    }
                    if ui
                        .add_enabled(!image_processing, egui::Button::new("Reset"))
                        .clicked()
                    {
                        let adjustments = ImageAdjustments::default();
                        image.adjustments = adjustments;
                        adjustment_request = Some((
                            image.id.clone(),
                            image.content_revision,
                            image.original_image.clone(),
                            adjustments,
                        ));
                    }
                });
            });
            ui.collapsing("Background removal", |ui| {
                ui.add(
                    egui::Slider::new(&mut self.background_tolerance, 0..=220).text("Tolerance"),
                );
                ui.add(egui::Slider::new(&mut self.background_feather, 0..=80).text("Feather"));
                if ui
                    .add_enabled(
                        !image_processing,
                        egui::Button::new(if image_processing {
                            "Processing image…"
                        } else {
                            "Remove edge background"
                        }),
                    )
                    .clicked()
                {
                    edge_removal_request = Some((
                        image.id.clone(),
                        image.content_revision,
                        image.image.clone(),
                        self.background_tolerance,
                        self.background_feather,
                    ));
                }
            });
            let mut duplicate_clicked = false;
            let mut forward_clicked = false;
            let mut backward_clicked = false;
            let artwork_id = image.id.clone();
            let can_reorder = !image.locked;
            ui.horizontal(|ui| {
                duplicate_clicked = ui.button("Duplicate").clicked();
                forward_clicked = ui
                    .add_enabled(
                        can_reorder && index + 1 < self.loaded_images.len(),
                        egui::Button::new("Bring forward"),
                    )
                    .clicked();
                backward_clicked = ui
                    .add_enabled(can_reorder && index > 0, egui::Button::new("Send backward"))
                    .clicked();
            });
            let inspector_action = if duplicate_clicked {
                Some(views::ArtworkMenuCommand::Duplicate)
            } else if forward_clicked {
                Some(views::ArtworkMenuCommand::BringForward)
            } else if backward_clicked {
                Some(views::ArtworkMenuCommand::SendBackward)
            } else {
                None
            };
            if let Some(command) = inspector_action {
                self.apply_artwork_menu_action(views::ArtworkMenuAction {
                    image_ids: vec![artwork_id],
                    command,
                });
            }
            if let Some((image_id, source_revision, source, adjustments)) = adjustment_request {
                self.image_processing.insert(image_id.clone());
                let tx = self.tx.clone();
                spawn_blocking(move || {
                    let image = studio::adjust_image(&source, adjustments);
                    let _ = tx.send(Action::ImageAdjusted {
                        image_id,
                        source_revision,
                        adjustments,
                        image,
                    });
                });
            } else if let Some((image_id, source_revision, source, tolerance, feather)) =
                edge_removal_request
            {
                self.image_processing.insert(image_id.clone());
                let tx = self.tx.clone();
                spawn_blocking(move || {
                    let image = studio::remove_background(&source, tolerance, feather);
                    let _ = tx.send(Action::EdgeBackgroundRemoved {
                        image_id,
                        source_revision,
                        image,
                    });
                });
            }
        }
    }

    fn device_status(&mut self, ui: &mut egui::Ui) {
        ui.label(format!(
            "{} printer connection(s)",
            self.printer_connections.len()
        ));
        let mut disconnect = None;
        let mut accept_fallback = None;
        let mut calibrate = None;
        for printer in self.job_queue.printers().to_vec() {
            ui.horizontal(|ui| {
                ui.label(&printer.name);
                ui.small(format!("{:?}", printer.status));
                let has_stable_identity = self
                    .printer_identities
                    .get(&printer.id)
                    .is_some_and(|identity| identity.serial_number.is_some())
                    || self.printer_fallback_names.contains_key(&printer.id);
                if self.printer_connections.contains_key(&printer.id)
                    && has_stable_identity
                    && ui.small_button("Calibration profile…").clicked()
                {
                    calibrate = Some(printer.id.clone());
                }
                if self.printer_connections.contains_key(&printer.id)
                    && ui.small_button("Disconnect").clicked()
                {
                    disconnect = Some(printer.id.clone());
                }
            });
            if self.printer_identity_loading.contains(&printer.id) {
                ui.small("Reading calibration identity…");
            } else if let Some(identity) = self.printer_identities.get(&printer.id) {
                if let Some(serial) = &identity.serial_number {
                    ui.small(format!(
                        "{} · serial {} · firmware {}",
                        identity.model, serial, identity.firmware_revision
                    ));
                } else {
                    ui.small("This connection did not provide a stable serial number.");
                    ui.horizontal(|ui| {
                        let draft = self
                            .printer_fallback_drafts
                            .entry(printer.id.clone())
                            .or_insert_with(|| printer.name.clone());
                        ui.text_edit_singleline(draft).on_hover_text(
                            "Stable name used to select calibration for this printer",
                        );
                        if ui
                            .add_enabled(
                                !draft.trim().is_empty(),
                                egui::Button::new("Use named fallback"),
                            )
                            .clicked()
                        {
                            accept_fallback = Some((printer.id.clone(), draft.trim().to_owned()));
                        }
                    });
                }
            } else if let Some(error) = self.printer_identity_errors.get(&printer.id) {
                ui.small(format!("Calibration identity unavailable: {error}"));
            }
        }
        if let Some((printer_id, profile_name)) = accept_fallback {
            self.printer_fallback_names
                .insert(printer_id.clone(), profile_name);
            self.printer_identity_errors.remove(&printer_id);
            let _ = self.job_queue.set_printer_online(&printer_id);
        }
        if let Some(printer_id) = calibrate {
            if self.calibration_session.is_some() {
                self.calibration_ui_state.open = true;
            } else {
                self.start_calibration(printer_id);
            }
        }
        if let Some(printer_id) = disconnect
            && let Some(manager) = self.printer_connections.remove(&printer_id)
        {
            let _ = self
                .job_queue
                .set_printer_offline(&printer_id, "printer disconnected");
            self.printer_statuses
                .insert(printer_id, TransportStatus::Disconnecting);
            spawn(async move {
                let _ = manager.disconnect().await;
            });
        }

        let mode_has_cutting = DEVICES[self.selected_device].modes[self.selected_mode]
            .mode_type
            .has_cutting();
        let has_visible_artwork = self.loaded_images.iter().any(|image| image.visible);
        let has_enabled_cut_paths =
            has_enabled_cut_path(&self.cut_shapes, &self.cut_modes, self.perf_cut);
        let print_block_reason = if self.print_preparing {
            Some("Preparing print job")
        } else {
            print_block_reason(
                !self.printer_connections.is_empty(),
                has_visible_artwork,
                mode_has_cutting,
                self.cut_progress.is_some(),
                has_enabled_cut_paths,
                self.has_intersections,
                self.off_canvas,
            )
        };
        let can_print = print_block_reason.is_none();
        let print_label = if mode_has_cutting {
            "Print & cut"
        } else {
            "Print"
        };
        let print = theme::primary_button_enabled(ui, self.accent_color(), can_print, print_label);
        if let Some(reason) = print_block_reason {
            print.on_disabled_hover_text(reason);
        } else if print.clicked() {
            self.print_canvas();
        }
        if let Some(send_progress) = self.send_progress {
            ui.add(
                egui::ProgressBar::new(send_progress)
                    .show_percentage()
                    .animate(true),
            );
        }
        if let Some(status) = &self.job_status {
            ui.small(format!(
                "Job: {} / {}",
                serde_plain::to_string(&status.job_state).unwrap_or_default(),
                serde_plain::to_string(&status.job_sub_state).unwrap_or_default()
            ));
        }
        if let Some(status) = &self.device_status {
            ui.small(format!(
                "Device: {} / {} · {}",
                serde_plain::to_string(&status.0).unwrap_or_default(),
                serde_plain::to_string(&status.1).unwrap_or_default(),
                status.2
            ));
        }

        ui.separator();
        ui.strong("Add printer");
        let previous_transport = self.selected_transport_index;
        egui::ComboBox::from_label("Transport")
            .selected_text(self.transport_names[self.selected_transport_index].as_ref())
            .show_index(
                ui,
                &mut self.selected_transport_index,
                self.transport_names.len(),
                |index| self.transport_names[index].as_ref(),
            );
        if self.selected_transport_index != previous_transport {
            self.discovered_devices.clear();
            self.selected_transport_device = None;
            self.discovering_devices = false;
        }
        if self.transport_status == TransportStatus::Connecting {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Connecting");
            });
        }
        let requires_selection = self.selected_transport_supports_discovery();
        if requires_selection {
            ui.horizontal(|ui| {
                let refresh = ui.add_enabled(
                    !self.discovering_devices,
                    egui::Button::new("Refresh devices"),
                );
                if refresh.clicked() {
                    self.refresh_transport_devices();
                }
                if self.discovering_devices {
                    ui.spinner();
                }
            });
            let selected_text = self
                .selected_transport_device
                .as_ref()
                .and_then(|selected| {
                    self.discovered_devices
                        .iter()
                        .find(|device| &device.id == selected)
                })
                .map(|device| device.name.clone())
                .unwrap_or_else(|| "Select a device…".into());
            egui::ComboBox::from_id_salt("transport_device")
                .selected_text(selected_text)
                .show_ui(ui, |ui| {
                    for device in &self.discovered_devices {
                        ui.selectable_value(
                            &mut self.selected_transport_device,
                            Some(device.id.clone()),
                            &device.name,
                        );
                    }
                });
        }
        let can_connect = self.transport_status != TransportStatus::Connecting
            && (!requires_selection || self.selected_transport_device.is_some());
        if theme::secondary_button_enabled(ui, self.accent_color(), can_connect, "Connect printer")
            .on_disabled_hover_text("Choose an available device first")
            .clicked()
        {
            self.connect_transport();
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SheetAlignment {
    Left,
    CenterX,
    Right,
    Top,
    Middle,
    Bottom,
}

fn align_image_to_sheet(image: &mut LoadedImage, canvas: Vec2, alignment: SheetAlignment) {
    let visual_offset = image.visual_offset();
    let visual_size = image.rotated_size();
    match alignment {
        SheetAlignment::Left => image.offset.x -= visual_offset.x,
        SheetAlignment::CenterX => image.offset.x = (canvas.x - image.size().x) / 2.0,
        SheetAlignment::Right => image.offset.x += canvas.x - visual_offset.x - visual_size.x,
        SheetAlignment::Top => image.offset.y -= visual_offset.y,
        SheetAlignment::Middle => image.offset.y = (canvas.y - image.size().y) / 2.0,
        SheetAlignment::Bottom => image.offset.y += canvas.y - visual_offset.y - visual_size.y,
    }
}

const fn placeholder_fit_label(fit: PlaceholderFit) -> &'static str {
    match fit {
        PlaceholderFit::Contain => "Contain",
        PlaceholderFit::Cover => "Cover",
        PlaceholderFit::Stretch => "Stretch",
    }
}

fn print_block_reason(
    has_printer: bool,
    has_visible_artwork: bool,
    mode_has_cutting: bool,
    cut_generation_pending: bool,
    has_enabled_cut_paths: bool,
    has_intersections: bool,
    off_canvas: bool,
) -> Option<&'static str> {
    if !has_printer {
        Some("Connect a printer before starting production.")
    } else if !has_visible_artwork {
        Some("Add artwork before starting production.")
    } else if mode_has_cutting && cut_generation_pending {
        Some("Wait for cutline generation to finish.")
    } else if mode_has_cutting && !has_enabled_cut_paths {
        Some("Generate at least one enabled cutline before starting production.")
    } else if mode_has_cutting && (has_intersections || off_canvas) {
        Some("Resolve overlapping or out-of-bounds cutlines before starting production.")
    } else {
        None
    }
}

fn has_enabled_cut_path(
    cut_shapes: &[LineString<f32>],
    cut_modes: &[CutMode],
    all_perforation: bool,
) -> bool {
    cut_shapes
        .iter()
        .zip(effective_cut_modes(
            cut_shapes.len(),
            cut_modes,
            all_perforation,
        ))
        .any(|(path, mode)| {
            mode != CutMode::Disabled
                && path.0.len() >= 2
                && path
                    .0
                    .iter()
                    .all(|point| point.x.is_finite() && point.y.is_finite())
                && path.0.windows(2).any(|segment| segment[0] != segment[1])
        })
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct CutPathValidation {
    has_intersections: bool,
    off_canvas: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CutValidationSnapshot {
    geometry_hash: u64,
    canvas_size: [u32; 2],
    safe_area: [u32; 2],
}

fn cut_validation_snapshot(
    cut_shapes: &[LineString<f32>],
    cut_modes: &[CutMode],
    _all_perforation: bool,
    canvas_size: Vec2,
    safe_area: Vec2,
) -> CutValidationSnapshot {
    let mut hasher = DefaultHasher::new();
    cut_shapes.len().hash(&mut hasher);
    for path in cut_shapes {
        path.0.len().hash(&mut hasher);
        for point in &path.0 {
            point.x.to_bits().hash(&mut hasher);
            point.y.to_bits().hash(&mut hasher);
        }
    }
    for index in 0..cut_shapes.len() {
        (cut_modes.get(index) == Some(&CutMode::Disabled)).hash(&mut hasher);
    }
    CutValidationSnapshot {
        geometry_hash: hasher.finish(),
        canvas_size: [canvas_size.x.to_bits(), canvas_size.y.to_bits()],
        safe_area: [safe_area.x.to_bits(), safe_area.y.to_bits()],
    }
}

fn cut_validation_snapshot_with_tabs(
    cut_shapes: &[LineString<f32>],
    cut_modes: &[CutMode],
    all_perforation: bool,
    canvas_size: Vec2,
    safe_area: Vec2,
    peel_tabs_enabled: bool,
    peel_tab_positions: &[Option<f32>],
) -> CutValidationSnapshot {
    let mut snapshot = cut_validation_snapshot(
        cut_shapes,
        cut_modes,
        all_perforation,
        canvas_size,
        safe_area,
    );
    let mut hasher = DefaultHasher::new();
    snapshot.geometry_hash.hash(&mut hasher);
    peel_tabs_enabled.hash(&mut hasher);
    if peel_tabs_enabled {
        for position in peel_tab_positions.iter().take(cut_shapes.len()) {
            position.map(f32::to_bits).hash(&mut hasher);
        }
    }
    snapshot.geometry_hash = hasher.finish();
    snapshot
}

fn validate_current_cut_paths(
    cut_shapes: &[LineString<f32>],
    cut_modes: &[CutMode],
    all_perforation: bool,
    canvas_size: Vec2,
    safe_area: Vec2,
) -> CutPathValidation {
    let modes = effective_cut_modes(cut_shapes.len(), cut_modes, all_perforation);
    let effective_paths = cut_shapes
        .iter()
        .zip(modes)
        .filter_map(|(path, mode)| {
            (mode != CutMode::Disabled
                && path.0.len() >= 2
                && path
                    .0
                    .iter()
                    .all(|point| point.x.is_finite() && point.y.is_finite())
                && path.0.windows(2).any(|segment| segment[0] != segment[1]))
            .then_some(path)
        })
        .collect::<Vec<_>>();

    let bounds = effective_paths
        .iter()
        .map(|path| path.bounding_rect())
        .collect::<Vec<_>>();
    let mut order = bounds
        .iter()
        .enumerate()
        .filter_map(|(index, bounds)| bounds.map(|bounds| (index, bounds)))
        .collect::<Vec<_>>();
    order.sort_by(|(_, left), (_, right)| left.min().x.total_cmp(&right.min().x));
    let mut has_intersections = false;
    'intersection: for left in 0..order.len() {
        let (left_index, left_bounds) = order[left];
        for &(right_index, right_bounds) in &order[left + 1..] {
            if right_bounds.min().x > left_bounds.max().x {
                break;
            }
            if left_bounds.intersects(&right_bounds)
                && effective_paths[left_index].intersects(effective_paths[right_index])
            {
                has_intersections = true;
                break 'intersection;
            }
        }
    }
    let offset = (canvas_size - safe_area) / 2.0;
    let safe_canvas = GeoRect::new(
        Coord {
            x: offset.x,
            y: offset.y,
        },
        Coord {
            x: canvas_size.x - offset.x,
            y: canvas_size.y - offset.y,
        },
    )
    .to_polygon();
    let off_canvas = effective_paths
        .iter()
        .any(|path| !safe_canvas.contains(*path));

    CutPathValidation {
        has_intersections,
        off_canvas,
    }
}

fn validate_current_cut_paths_with_tabs(
    cut_shapes: &[LineString<f32>],
    cut_modes: &[CutMode],
    all_perforation: bool,
    canvas_size: Vec2,
    safe_area: Vec2,
    peel_tabs_enabled: bool,
    peel_tab_positions: &[Option<f32>],
) -> CutPathValidation {
    let mut validation = validate_current_cut_paths(
        cut_shapes,
        cut_modes,
        all_perforation,
        canvas_size,
        safe_area,
    );
    if !peel_tabs_enabled {
        return validation;
    }
    let enabled = (0..cut_shapes.len())
        .map(|index| cut_modes.get(index) != Some(&CutMode::Disabled))
        .collect::<Vec<_>>();
    let tabs = build_peel_tabs(cut_shapes, &enabled, peel_tab_positions);
    let offset = (canvas_size - safe_area) / 2.0;
    let safe_canvas = GeoRect::new(
        Coord {
            x: offset.x,
            y: offset.y,
        },
        Coord {
            x: canvas_size.x - offset.x,
            y: canvas_size.y - offset.y,
        },
    )
    .to_polygon();
    validation.off_canvas |= tabs.iter().any(|(_, tab)| !safe_canvas.contains(&tab.path));
    if !validation.has_intersections {
        validation.has_intersections = tabs.iter().any(|(owner, tab)| {
            cut_shapes
                .iter()
                .enumerate()
                .any(|(index, path)| index != *owner && enabled[index] && tab.path.intersects(path))
        });
    }
    if !validation.has_intersections {
        validation.has_intersections = tabs.iter().enumerate().any(|(left, (_, tab))| {
            tabs[left + 1..]
                .iter()
                .any(|(_, other)| tab.path.intersects(&other.path))
        });
    }
    validation
}

impl eframe::App for SapodillaApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let accent = self.accent_color();
        theme::apply(ctx, accent);
        self.apply_actions();
        self.cut_modes.resize(self.cut_shapes.len(), CutMode::Kiss);
        self.cut_modes.truncate(self.cut_shapes.len());
        self.cutline_owners.resize(self.cut_shapes.len(), None);
        self.cutline_owners.truncate(self.cut_shapes.len());
        self.cutline_locked.resize(self.cut_shapes.len(), false);
        self.cutline_locked.truncate(self.cut_shapes.len());
        self.peel_tab_positions.resize(self.cut_shapes.len(), None);
        self.peel_tab_positions.truncate(self.cut_shapes.len());
        views::synchronize_cut_preview(self);
        let canvas = self.get_canvas();
        let current_validation_snapshot = cut_validation_snapshot_with_tabs(
            &self.cut_shapes,
            &self.cut_modes,
            self.perf_cut,
            canvas.size,
            canvas.safe_area,
            self.peel_tabs,
            &self.peel_tab_positions,
        );
        if self.cut_validation_snapshot.as_ref() != Some(&current_validation_snapshot) {
            let cut_validation = validate_current_cut_paths_with_tabs(
                &self.cut_shapes,
                &self.cut_modes,
                self.perf_cut,
                canvas.size,
                canvas.safe_area,
                self.peel_tabs,
                &self.peel_tab_positions,
            );
            self.has_intersections = cut_validation.has_intersections;
            self.off_canvas = cut_validation.off_canvas;
            self.cut_validation_snapshot = Some(current_validation_snapshot);
        }

        let save_shortcut = KeyboardShortcut::new(Modifiers::COMMAND, egui::Key::S);
        if ctx.input_mut(|input| input.consume_shortcut(&save_shortcut)) {
            self.save_document(self.document_kind);
        }
        let new_shortcut = KeyboardShortcut::new(Modifiers::COMMAND, egui::Key::N);
        if ctx.input_mut(|input| input.consume_shortcut(&new_shortcut)) {
            self.request_new_sheet();
        }
        let open_shortcut = KeyboardShortcut::new(Modifiers::COMMAND, egui::Key::O);
        if ctx.input_mut(|input| input.consume_shortcut(&open_shortcut)) {
            self.open_document(ctx);
        }
        let settings_shortcut = KeyboardShortcut::new(Modifiers::COMMAND, egui::Key::Comma);
        if ctx.input_mut(|input| input.consume_shortcut(&settings_shortcut)) {
            self.custom_accent_rgb = self.appearance.accent.rgb();
            self.show_settings = true;
        }
        if ctx.input(|input| input.key_pressed(egui::Key::Escape)) && self.edit_cutlines {
            self.edit_cutlines = false;
            self.selected_cut_path = None;
            self.selected_cut_node = None;
        }

        let compact_layout = ctx.content_rect().width() < 1160.0;
        if compact_layout != self.compact_layout {
            self.compact_layout = compact_layout;
            self.show_library_panel = !compact_layout;
            self.show_inspector_panel = !compact_layout;
        }

        egui::TopBottomPanel::top("top_panel")
            .frame(theme::panel_frame(ctx.style().visuals.dark_mode))
            .show(ctx, |ui| {
                egui::MenuBar::new().ui(ui, |ui| {
                    self.menu(ui, ctx);
                });
                ui.add_space(5.0);
                ui.horizontal(|ui| {
                    egui::Frame::new()
                        .fill(accent)
                        .corner_radius(egui::CornerRadius::same(7))
                        .inner_margin(egui::Margin::symmetric(7, 4))
                        .show(ui, |ui| {
                            ui.label(
                                egui::RichText::new("S").size(15.0).strong().color(
                                    theme::Palette::for_accent(ui.visuals().dark_mode, accent)
                                        .on_accent,
                                ),
                            );
                        });
                    if !compact_layout {
                        ui.label(egui::RichText::new("Sapodilla").size(18.0).strong());
                        ui.label(egui::RichText::new("STUDIO").size(10.0).color(
                            theme::Palette::for_accent(ui.visuals().dark_mode, accent).accent_text,
                        ));
                    }
                    ui.separator();
                    if theme::primary_toolbar_action(
                        ui,
                        accent,
                        crate::icons::ADD_ARTWORK,
                        if compact_layout { "Add" } else { "Add artwork" },
                        "Add artwork",
                        "Import artwork (Ctrl/Cmd+Shift+U)",
                    )
                    .clicked()
                    {
                        self.upload_image(ctx);
                    }
                    if compact_layout {
                        if theme::toolbar_icon_button(
                            ui,
                            accent,
                            crate::icons::SAVE,
                            "Save document",
                            format!(
                                "Save this document ({})",
                                ctx.format_shortcut(&save_shortcut)
                            ),
                        )
                        .clicked()
                        {
                            self.save_document(self.document_kind);
                        }
                    } else {
                        if theme::secondary_toolbar_action(
                            ui,
                            accent,
                            crate::icons::AUTO_PACK,
                            "Auto-pack",
                            "Auto-pack sheet",
                            "Arrange artwork to use the sheet efficiently",
                        )
                        .clicked()
                        {
                            self.auto_pack();
                        }
                        if theme::toolbar_icon_button(
                            ui,
                            accent,
                            crate::icons::SAVE,
                            "Save document",
                            format!(
                                "Save this document ({})",
                                ctx.format_shortcut(&save_shortcut)
                            ),
                        )
                        .clicked()
                        {
                            self.save_document(self.document_kind);
                        }
                        ui.separator();
                        theme::toolbar_icon_toggle(
                            ui,
                            accent,
                            &mut self.snap_to_guides,
                            crate::icons::SNAP,
                            "Snap artwork to guides",
                            "Snap artwork to guides",
                        );
                        theme::toolbar_icon_toggle(
                            ui,
                            accent,
                            &mut self.show_grid,
                            crate::icons::GRID,
                            "Layout grid",
                            "Show or hide the layout grid",
                        );
                        theme::toolbar_icon_toggle(
                            ui,
                            accent,
                            &mut self.show_rulers,
                            crate::icons::RULERS,
                            "Canvas rulers",
                            "Show or hide canvas rulers",
                        );
                        theme::toolbar_icon_toggle(
                            ui,
                            accent,
                            &mut self.show_cutlines,
                            crate::icons::CUT_PREVIEW,
                            "Cut preview",
                            "Show or hide the cut preview",
                        );
                        theme::toolbar_icon_toggle(
                            ui,
                            accent,
                            &mut self.edit_cutlines,
                            crate::icons::EDIT_NODES,
                            "Edit cut nodes",
                            "Edit cut-path nodes",
                        );
                    }

                    if theme::toolbar_icon_toggle(
                        ui,
                        accent,
                        &mut self.show_library_panel,
                        crate::icons::LIBRARY,
                        "Library panel",
                        "Show or hide the artwork library",
                    )
                    .clicked()
                        && compact_layout
                        && self.show_library_panel
                    {
                        self.show_inspector_panel = false;
                    }
                    if theme::toolbar_icon_toggle(
                        ui,
                        accent,
                        &mut self.show_inspector_panel,
                        crate::icons::INSPECTOR,
                        "Inspector panel",
                        "Show or hide the inspector",
                    )
                    .clicked()
                        && compact_layout
                        && self.show_inspector_panel
                    {
                        self.show_library_panel = false;
                    }

                    if compact_layout {
                        theme::toolbar_icon_menu(
                            ui,
                            accent,
                            crate::icons::DOTS_THREE,
                            "More toolbar actions",
                            "More toolbar actions",
                            |ui| {
                                if ui.button("Auto-pack sheet").clicked() {
                                    self.auto_pack();
                                    ui.close();
                                }
                                ui.separator();
                                ui.checkbox(&mut self.snap_to_guides, "Snap to guides");
                                ui.checkbox(&mut self.show_grid, "Show grid");
                                ui.checkbox(&mut self.show_rulers, "Show rulers");
                                ui.checkbox(&mut self.show_cutlines, "Show cut preview");
                                ui.checkbox(&mut self.edit_cutlines, "Edit cut nodes");
                            },
                        );
                    }

                    if !compact_layout {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let (ready, status) = match self.printer_connections.len() {
                                0 if self.transport_status == TransportStatus::Connecting => {
                                    (false, "Connecting…".to_owned())
                                }
                                0 => (false, "No printer".to_owned()),
                                1 => (true, "1 printer ready".to_owned()),
                                count => (true, format!("{count} printers ready")),
                            };
                            theme::status_badge(ui, ready, &status)
                                .response
                                .on_hover_text(if ready {
                                    "A printer is available for this job"
                                } else {
                                    "Open the Inspector to connect a printer"
                                });
                        });
                    }
                });
                ui.add_space(5.0);
                theme::spectrum_rule(ui, accent);
            });

        egui::SidePanel::left("library_panel")
            .resizable(true)
            .default_width(if compact_layout { 260.0 } else { 240.0 })
            .width_range(220.0..=360.0)
            .frame(theme::panel_frame(ctx.style().visuals.dark_mode))
            .show_animated(ctx, self.show_library_panel, |ui| {
                theme::panel_title(ui, accent, "Assets", "Library");
                if theme::secondary_icon_text_button(
                    ui,
                    accent,
                    crate::icons::UPLOAD,
                    "Import artwork…",
                    "Import PNG or JPEG artwork into the Library",
                )
                .clicked()
                {
                    self.import_library_images(ctx);
                }
                theme::muted(ui, "Add artwork once, then reuse it across sheets.");
                ui.add_space(6.0);
                #[cfg(not(target_arch = "wasm32"))]
                if theme::icon_text_button(
                    ui,
                    crate::icons::FOLDER_PLUS,
                    "Import folder…",
                    "Add a watched artwork folder",
                )
                .clicked()
                {
                    self.import_library_folder(ctx);
                }
                #[cfg(not(target_arch = "wasm32"))]
                if !self.library_folders.is_empty() {
                    let mut forget = None;
                    ui.collapsing("Watched folders", |ui| {
                        for (index, folder) in self.library_folders.iter().enumerate() {
                            ui.horizontal(|ui| {
                                ui.small(folder);
                                if ui.small_button("Forget").clicked() {
                                    forget = Some(index);
                                }
                            });
                        }
                        if theme::icon_text_button(
                            ui,
                            crate::icons::ARROWS_CLOCKWISE,
                            "Rescan folders",
                            "Rescan watched artwork folders",
                        )
                        .clicked()
                        {
                            self.reset_library_cycle();
                            self.library_page = 0;
                            (self.library_disk_paths, self.library_has_more) = scan_library_page(
                                &self.library_folders,
                                self.library_page,
                                LIBRARY_PAGE_SIZE,
                            );
                        }
                    });
                    if let Some(index) = forget {
                        self.library_folders.remove(index);
                        self.reset_library_cycle();
                        self.library_page = 0;
                        (self.library_disk_paths, self.library_has_more) = scan_library_page(
                            &self.library_folders,
                            self.library_page,
                            LIBRARY_PAGE_SIZE,
                        );
                    }
                }
                #[cfg(not(target_arch = "wasm32"))]
                if !self.library_folders.is_empty() {
                    theme::muted(
                        ui,
                        "Folder locations are restored and rescanned on startup.",
                    );
                }
                ui.separator();
                #[cfg(not(target_arch = "wasm32"))]
                let library_empty = self.library.is_empty() && self.library_disk_paths.is_empty();
                #[cfg(target_arch = "wasm32")]
                let library_empty = self.library.is_empty();
                if library_empty {
                    theme::card(ui.visuals().dark_mode).show(ui, |ui| {
                        ui.label(egui::RichText::new("Your artwork library is empty").strong());
                        theme::muted(
                            ui,
                            "Import PNG or JPEG artwork, or drag files into the workspace.",
                        );
                    });
                    ui.add_space(4.0);
                }
                egui::ScrollArea::vertical()
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        let mut add = None;
                        let mut remove = None;
                        for (index, asset) in self.library.iter().enumerate() {
                            theme::card(ui.visuals().dark_mode).show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    let image = egui::Image::new(asset.sized_texture)
                                        .fit_to_exact_size(Vec2::splat(52.0));
                                    let add_button = ui.add(egui::Button::image(image));
                                    let add_label = format!("Add {} to sheet", asset.name);
                                    add_button.widget_info(|| {
                                        egui::WidgetInfo::labeled(
                                            egui::WidgetType::Button,
                                            true,
                                            add_label.clone(),
                                        )
                                    });
                                    if add_button.on_hover_text("Add to sheet").clicked() {
                                        add = Some(index);
                                    }
                                    ui.vertical(|ui| {
                                        ui.label(&asset.name);
                                        ui.small(format!(
                                            "{} × {} px",
                                            asset.image.width(),
                                            asset.image.height()
                                        ));
                                        if theme::icon_text_button_named(
                                            ui,
                                            crate::icons::TRASH,
                                            "Remove",
                                            format!("Remove {} from Library", asset.name),
                                            format!("Remove {} from the Library", asset.name),
                                        )
                                        .clicked()
                                        {
                                            remove = Some(index);
                                        }
                                    });
                                });
                            });
                        }
                        if let Some(index) = add {
                            let mut image = self.library[index].clone();
                            image.id = format!("image-{}", Uuid::new_v4());
                            self.add_new_artwork(image);
                        }
                        if let Some(index) = remove {
                            self.library.remove(index);
                            self.reset_library_cycle();
                        }
                        #[cfg(not(target_arch = "wasm32"))]
                        {
                            let mut open = None;
                            for path in &self.library_disk_paths {
                                let label = path
                                    .file_stem()
                                    .and_then(|name| name.to_str())
                                    .unwrap_or("Artwork");
                                if ui
                                    .button(label)
                                    .on_hover_text(path.to_string_lossy())
                                    .clicked()
                                {
                                    open = Some(path.clone());
                                }
                            }
                            if let Some(path) = open {
                                self.add_disk_library_image(ctx, path);
                            }
                        }
                    });
                #[cfg(not(target_arch = "wasm32"))]
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled_ui(self.library_page > 0, |ui| {
                            theme::icon_button(
                                ui,
                                crate::icons::CARET_LEFT,
                                "Previous Library page",
                                "Show the previous Library page",
                            )
                        })
                        .inner
                        .clicked()
                    {
                        self.library_page -= 1;
                        (self.library_disk_paths, self.library_has_more) = scan_library_page(
                            &self.library_folders,
                            self.library_page,
                            LIBRARY_PAGE_SIZE,
                        );
                    }
                    ui.label(format!("Page {}", self.library_page + 1));
                    if ui
                        .add_enabled_ui(self.library_has_more, |ui| {
                            theme::icon_button(
                                ui,
                                crate::icons::CARET_RIGHT,
                                "Next Library page",
                                "Show the next Library page",
                            )
                        })
                        .inner
                        .clicked()
                    {
                        self.library_page += 1;
                        (self.library_disk_paths, self.library_has_more) = scan_library_page(
                            &self.library_folders,
                            self.library_page,
                            LIBRARY_PAGE_SIZE,
                        );
                    }
                });
                ui.separator();
                ui.horizontal(|ui| {
                    if theme::icon_text_button(
                        ui,
                        crate::icons::GRID_NINE,
                        "Fill sheet",
                        "Fill the sheet with Library artwork",
                    )
                    .clicked()
                    {
                        self.add_library_to_sheet(false);
                    }
                    if theme::icon_text_button(
                        ui,
                        crate::icons::SHUFFLE,
                        "Shuffle fill",
                        "Shuffle Library artwork while filling the sheet",
                    )
                    .clicked()
                    {
                        self.add_library_to_sheet(true);
                    }
                });
            });

        egui::SidePanel::right("control_panel")
            .resizable(true)
            .default_width(if compact_layout { 280.0 } else { 350.0 })
            .width_range(260.0..=440.0)
            .frame(theme::panel_frame(ctx.style().visuals.dark_mode))
            .show_animated(ctx, self.show_inspector_panel, |ui| {
                theme::panel_title(ui, accent, "Make", "Inspector");
                theme::muted(ui, "Prepare the sheet, cut paths, and production job.");
                ui.separator();
                egui::ScrollArea::vertical()
                    .id_salt("inspector_scroll")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.heading("Connection");

                        self.device_status(ui);

                        let jobs = self.job_queue.jobs().cloned().collect::<Vec<_>>();
                        let queue_needs_attention = jobs.iter().any(|job| {
                            matches!(
                                job.status,
                                QueueJobStatus::Error | QueueJobStatus::Cancelled
                            )
                        });
                        egui::CollapsingHeader::new(format!("Production queue ({})", jobs.len()))
                            .id_salt("production_queue")
                            .default_open(queue_needs_attention)
                            .show(ui, |ui| {
                                let mut cancel = None;
                                let mut retry = None;
                                for job in jobs {
                                    ui.group(|ui| {
                                        ui.horizontal(|ui| {
                                            ui.label(format!("#{} {}", job.id, job.spec.name));
                                            ui.label(format!("{:?}", job.status));
                                        });
                                        ui.add(
                                            egui::ProgressBar::new(
                                                f32::from(job.progress_percent) / 100.0,
                                            )
                                            .show_percentage(),
                                        );
                                        if let Some(error) = &job.error {
                                            ui.small(error);
                                        }
                                        ui.horizontal(|ui| {
                                            if job.status == QueueJobStatus::Queued
                                                && ui.small_button("Cancel").clicked()
                                            {
                                                cancel = Some(job.id);
                                            }
                                            if matches!(
                                                job.status,
                                                QueueJobStatus::Error | QueueJobStatus::Cancelled
                                            ) && self.pending_print_jobs.contains_key(&job.id)
                                                && ui.small_button("Retry").clicked()
                                            {
                                                retry = Some(job.id);
                                            }
                                        });
                                    });
                                }
                                if let Some(job_id) = cancel {
                                    let _ = self.job_queue.cancel(job_id);
                                }
                                if let Some(job_id) = retry {
                                    let _ = self.job_queue.retry(job_id);
                                }
                            });

                        ui.separator();

                        ui.heading("Settings");

                        let previous = self.selected_device;
                        egui::ComboBox::from_label("Device")
                            .selected_text(&DEVICES[self.selected_device].name)
                            .show_index(ui, &mut self.selected_device, DEVICES.len(), |i| {
                                &DEVICES[i].name
                            });
                        if self.selected_device != previous {
                            self.selected_mode = 0;
                            self.selected_canvas_size = 0;
                        }

                        let previous = self.selected_mode;
                        egui::ComboBox::from_label("Mode")
                            .selected_text(
                                DEVICES[self.selected_device].modes[self.selected_mode]
                                    .mode_type
                                    .name(),
                            )
                            .show_index(
                                ui,
                                &mut self.selected_mode,
                                DEVICES[self.selected_device].modes.len(),
                                |i| DEVICES[self.selected_device].modes[i].mode_type.name(),
                            );
                        if self.selected_mode != previous {
                            self.selected_canvas_size = 0;
                        }

                        egui::ComboBox::from_label("Canvas Size")
                            .selected_text(
                                &DEVICES[self.selected_device].modes[self.selected_mode]
                                    .canvas_sizes[self.selected_canvas_size]
                                    .name,
                            )
                            .show_index(
                                ui,
                                &mut self.selected_canvas_size,
                                DEVICES[self.selected_device].modes[self.selected_mode]
                                    .canvas_sizes
                                    .len(),
                                |i| {
                                    &DEVICES[self.selected_device].modes[self.selected_mode]
                                        .canvas_sizes[i]
                                        .name
                                },
                            );

                        ui.horizontal(|ui| {
                            ui.add(egui::DragValue::new(&mut self.copies).range(1..=10));
                            ui.label("Copies");
                        });

                        ui.separator();
                        ui.heading("Material");
                        egui::ComboBox::from_id_salt("material_profile")
                            .selected_text(&self.material_profiles[self.selected_material].name)
                            .show_index(
                                ui,
                                &mut self.selected_material,
                                self.material_profiles.len(),
                                |index| &self.material_profiles[index].name,
                            );
                        let material = &mut self.material_profiles[self.selected_material];
                        ui.text_edit_singleline(&mut material.name);
                        ui.add(
                            egui::Slider::new(&mut material.blade_pressure, 0..=100)
                                .text("Blade pressure"),
                        );
                        ui.add(
                            egui::Slider::new(&mut material.perf_pressure, 0..=100)
                                .text("Perf pressure"),
                        );
                        ui.add(egui::Slider::new(&mut material.passes, 0..=4).text("Passes"));
                        ui.add(egui::Slider::new(&mut material.speed, 1..=10).text("Speed"));
                        ui.horizontal(|ui| {
                            if ui
                                .add_enabled(
                                    self.material_profiles.len() < 128,
                                    egui::Button::new("New preset"),
                                )
                                .clicked()
                            {
                                let mut profile =
                                    self.material_profiles[self.selected_material].clone();
                                profile.name = "Custom material".into();
                                self.material_profiles.push(profile);
                                self.selected_material = self.material_profiles.len() - 1;
                            }
                            if ui
                                .add_enabled(
                                    self.material_profiles.len() > 1,
                                    egui::Button::new("Delete preset"),
                                )
                                .clicked()
                            {
                                self.material_profiles.remove(self.selected_material);
                                self.selected_material = self
                                    .selected_material
                                    .min(self.material_profiles.len().saturating_sub(1));
                            }
                        });

                        if DEVICES[self.selected_device].modes[self.selected_mode]
                            .mode_type
                            .has_cutting()
                        {
                            ui.separator();

                            views::cut_controls(
                                ui,
                                DEVICES[self.selected_device].dpi,
                                &mut self.cut_tuning,
                                self.cut_progress,
                                self.has_intersections,
                                self.off_canvas,
                            );

                            ui.horizontal(|ui| {
                                if ui
                                    .checkbox(&mut self.perf_cut, "All paths perforation")
                                    .changed()
                                {
                                    self.cut_modes.fill(if self.perf_cut {
                                        CutMode::Perforation
                                    } else {
                                        CutMode::Kiss
                                    });
                                }
                                ui.checkbox(&mut self.peel_tabs, "Peel tabs").on_hover_text(
                                    "Drag the gold tab handles around each cut preview to reposition them",
                                );
                            });
                            if self.peel_tabs {
                                theme::muted(ui, "Drag the gold handles to reposition peel tabs.");
                            }
                            if self.perf_cut {
                                ui.add(
                                    egui::Slider::new(&mut self.perf_dash_mm, 0.25..=8.0)
                                        .suffix(" mm")
                                        .text("Dash"),
                                );
                                ui.add(
                                    egui::Slider::new(&mut self.perf_gap_mm, 0.1..=4.0)
                                        .suffix(" mm")
                                        .text("Gap"),
                                );
                            }
                            ui.collapsing("Overcut", |ui| {
                                ui.checkbox(
                                    &mut self.overcut.enabled,
                                    "Lead-in/out at closed seams",
                                );
                                if self.overcut.enabled {
                                    ui.add(
                                        egui::Slider::new(&mut self.overcut.steps, 1..=12)
                                            .text("Steps"),
                                    );
                                    ui.add(
                                        egui::Slider::new(
                                            &mut self.overcut.maximum_angle_degrees,
                                            0.0..=90.0,
                                        )
                                        .suffix("°")
                                        .text("Maximum angle"),
                                    );
                                    let dpi = DEVICES[self.selected_device].dpi;
                                    let mut reach_mm = self.overcut.reach_pixels * 25.4 / dpi;
                                    if ui
                                        .add(
                                            egui::Slider::new(&mut reach_mm, 0.1..=5.0)
                                                .suffix(" mm")
                                                .text("Reach"),
                                        )
                                        .changed()
                                    {
                                        self.overcut.reach_pixels = reach_mm * dpi / 25.4;
                                    }
                                    ui.checkbox(
                                        &mut self.overcut.snap_to_pixels,
                                        "Snap ramp points",
                                    );
                                }
                            });
                            ui.horizontal(|ui| {
                                if ui.button("Import SVG").clicked() {
                                    self.import_svg();
                                }
                            });
                            ui.collapsing("Shape designer", |ui| {
                                egui::ComboBox::from_id_salt("procedural_shape")
                                    .selected_text(
                                        ProceduralShape::ALL[self.selected_procedural_shape].name(),
                                    )
                                    .show_ui(ui, |ui| {
                                        for (index, shape) in
                                            ProceduralShape::ALL.iter().enumerate()
                                        {
                                            ui.selectable_value(
                                                &mut self.selected_procedural_shape,
                                                index,
                                                shape.name(),
                                            );
                                        }
                                    });
                                ui.horizontal(|ui| {
                                    ui.add(
                                        egui::DragValue::new(&mut self.shape_width_mm)
                                            .range(1.0..=300.0)
                                            .suffix(" mm")
                                            .prefix("W "),
                                    );
                                    ui.add(
                                        egui::DragValue::new(&mut self.shape_height_mm)
                                            .range(1.0..=300.0)
                                            .suffix(" mm")
                                            .prefix("H "),
                                    );
                                });
                                if ui.button("Add shape cutline").clicked() {
                                    let canvas = self.get_canvas().size;
                                    let dpi = DEVICES[self.selected_device].dpi;
                                    let size = Vec2::new(
                                        self.shape_width_mm * dpi / 25.4,
                                        self.shape_height_mm * dpi / 25.4,
                                    );
                                    let offset = (canvas - size) / 2.0;
                                    let mut path = shapes::generate(
                                        ProceduralShape::ALL[self.selected_procedural_shape],
                                        size,
                                    );
                                    for point in &mut path.0 {
                                        point.x += offset.x;
                                        point.y += offset.y;
                                    }
                                    self.manual_cut_shapes.push(path.clone());
                                    self.cut_shapes.push(path);
                                    self.cut_modes.push(CutMode::Kiss);
                                    self.cutline_owners.push(None);
                                    self.cutline_locked.push(false);
                                    self.peel_tab_positions.push(None);
                                }
                            });
                            if !self.cut_shapes.is_empty() {
                                let stats = self.cut_preview_stats;
                                let mm_per_pixel = 25.4 / DEVICES[self.selected_device].dpi;
                                ui.small(format!(
                                    "{} paths · {} nodes · {:.1} mm cutting · {:.1} mm travel",
                                    stats.paths,
                                    stats.nodes,
                                    stats.cut_length * mm_per_pixel,
                                    stats.travel_length * mm_per_pixel,
                                ));
                                ui.horizontal(|ui| {
                                    if ui
                                        .add_enabled(
                                            self.selected_cut_path.is_some_and(|index| {
                                                !self
                                                    .cutline_locked
                                                    .get(index)
                                                    .copied()
                                                    .unwrap_or(false)
                                            }),
                                            egui::Button::new("Smooth selected"),
                                        )
                                        .clicked()
                                        && let Some(index) = self.selected_cut_path
                                    {
                                        self.cut_shapes[index] =
                                            smooth_path(&self.cut_shapes[index], 1);
                                        if index >= self.auto_cut_count {
                                            self.manual_cut_shapes[index - self.auto_cut_count] =
                                                self.cut_shapes[index].clone();
                                        }
                                    }
                                    if ui
                                        .add_enabled(
                                            self.cut_shapes.len() >= 2
                                                && !self
                                                    .cutline_locked
                                                    .iter()
                                                    .any(|locked| *locked)
                                                && self.cut_shapes.iter().all(|path| {
                                                    path.0.len() >= 4
                                                        && path.0.first() == path.0.last()
                                                }),
                                            egui::Button::new("Union all"),
                                        )
                                        .clicked()
                                    {
                                        let union = union_paths(&self.cut_shapes);
                                        if !union.is_empty() {
                                            self.cut_shapes = union;
                                            self.manual_cut_shapes = self.cut_shapes.clone();
                                            self.auto_cut_count = 0;
                                            self.cut_modes =
                                                vec![CutMode::Kiss; self.cut_shapes.len()];
                                            self.cutline_owners = vec![None; self.cut_shapes.len()];
                                            self.cutline_locked =
                                                vec![false; self.cut_shapes.len()];
                                            self.peel_tab_positions =
                                                vec![None; self.cut_shapes.len()];
                                            self.selected_cut_path = None;
                                            self.selected_cut_node = None;
                                        }
                                    }
                                });
                                let mut delete_path = None;
                                egui::ComboBox::from_label("Editable path")
                                    .selected_text(
                                        self.selected_cut_path.map_or("None".into(), |index| {
                                            format!("Path {}", index + 1)
                                        }),
                                    )
                                    .show_ui(ui, |ui| {
                                        ui.selectable_value(
                                            &mut self.selected_cut_path,
                                            None,
                                            "None",
                                        );
                                        for index in 0..self.cut_shapes.len() {
                                            ui.selectable_value(
                                                &mut self.selected_cut_path,
                                                Some(index),
                                                if self.cutline_locked[index] {
                                                    format!("Path {} 🔒", index + 1)
                                                } else {
                                                    format!("Path {}", index + 1)
                                                },
                                            );
                                        }
                                    });
                                let selected_artwork_bounds = (self.selected_images.len() == 1)
                                    .then(|| {
                                        let image = &self.loaded_images[self.selected_images[0]];
                                        egui::Rect::from_min_size(
                                            image.visual_offset(),
                                            image.rotated_size(),
                                        )
                                    });
                                if let Some(path_index) = self.selected_cut_path
                                    && let Some(path) = self.cut_shapes.get_mut(path_index)
                                {
                                    let path_locked = self.cutline_locked[path_index];
                                    if path_locked {
                                        ui.label("This template cutline is locked.");
                                    }
                                    ui.add_enabled_ui(!path_locked, |ui| {
                                        egui::ComboBox::from_label("Cut operation")
                                            .selected_text(self.cut_modes[path_index].label())
                                            .show_ui(ui, |ui| {
                                                for mode in [
                                                    CutMode::Kiss,
                                                    CutMode::Perforation,
                                                    CutMode::Disabled,
                                                ] {
                                                    ui.selectable_value(
                                                        &mut self.cut_modes[path_index],
                                                        mode,
                                                        mode.label(),
                                                    );
                                                }
                                            });
                                        if let Some(target) = selected_artwork_bounds {
                                            ui.horizontal(|ui| {
                                                if ui.button("Center on artwork").clicked() {
                                                    center_path_in_rect(path, target, false);
                                                }
                                                if ui.button("Fit to artwork").clicked() {
                                                    center_path_in_rect(path, target, true);
                                                }
                                            });
                                        }
                                        let node_index = self
                                            .selected_cut_node
                                            .unwrap_or(0)
                                            .min(path.0.len().saturating_sub(1));
                                        self.selected_cut_node = Some(node_index);
                                        ui.label(format!("{} nodes", path.0.len()));
                                        if let Some(node) = path.0.get_mut(node_index) {
                                            ui.horizontal(|ui| {
                                                ui.label(format!("Node {}", node_index + 1));
                                                ui.add(
                                                    egui::DragValue::new(&mut node.x).prefix("X "),
                                                );
                                                ui.add(
                                                    egui::DragValue::new(&mut node.y).prefix("Y "),
                                                );
                                            });
                                        }
                                        ui.horizontal(|ui| {
                                            if ui.button("Insert after").clicked()
                                                && path.0.len() >= 2
                                            {
                                                let next = (node_index + 1).min(path.0.len() - 1);
                                                let a = path.0[node_index];
                                                let b = path.0[next];
                                                path.0.insert(
                                                    next,
                                                    Coord {
                                                        x: (a.x + b.x) / 2.0,
                                                        y: (a.y + b.y) / 2.0,
                                                    },
                                                );
                                                self.selected_cut_node = Some(next);
                                            }
                                            if ui.button("Delete node").clicked()
                                                && path.0.len() > 3
                                            {
                                                path.0.remove(node_index);
                                                self.selected_cut_node =
                                                    Some(node_index.min(path.0.len() - 1));
                                            }
                                            if ui.button("Delete path").clicked() {
                                                delete_path = Some(path_index);
                                            }
                                        });
                                    });
                                    if path_index >= self.auto_cut_count
                                        && let Some(manual) = self
                                            .manual_cut_shapes
                                            .get_mut(path_index - self.auto_cut_count)
                                        && manual != path
                                    {
                                        manual.clone_from(path);
                                    }
                                }
                                if let Some(path_index) = delete_path {
                                    self.cut_shapes.remove(path_index);
                                    self.cut_modes.remove(path_index);
                                    self.cutline_owners.remove(path_index);
                                    self.cutline_locked.remove(path_index);
                                    self.peel_tab_positions.remove(path_index);
                                    if path_index >= self.auto_cut_count {
                                        let manual_index = path_index - self.auto_cut_count;
                                        if manual_index < self.manual_cut_shapes.len() {
                                            self.manual_cut_shapes.remove(manual_index);
                                        }
                                    } else {
                                        self.auto_cut_count -= 1;
                                    }
                                    self.selected_cut_path = None;
                                    self.selected_cut_node = None;
                                }
                            }

                            if ui
                                .add_enabled(
                                    self.cut_progress.is_none()
                                        && self.active_cut_generation.is_none(),
                                    egui::Button::new("Generate Cut Lines"),
                                )
                                .clicked()
                            {
                                self.synchronize_cut_geometry();
                                self.has_intersections = false;
                                self.off_canvas = false;
                                self.cut_progress = Some((0, 1));

                                let tx = self.tx.clone();
                                let source_geometry = self.current_cut_geometry();
                                let generation_id = self.next_cut_generation_id;
                                self.next_cut_generation_id =
                                    self.next_cut_generation_id.wrapping_add(1);
                                self.active_cut_generation = Some(generation_id);
                                let mut rx = CutGenerator::start(
                                    self.loaded_images
                                        .iter()
                                        .filter(|image| image.enable_cutting && image.visible)
                                        .map(CutImage::from)
                                        .collect(),
                                    self.cut_tuning.clone(),
                                    self.get_canvas(),
                                );

                                spawn(async move {
                                    while let Some(action) = rx.next().await {
                                        debug!(?action, "got cut action");

                                        if let Err(err) = tx.send(Action::Cut {
                                            generation_id,
                                            source_geometry: source_geometry.clone(),
                                            action,
                                        }) {
                                            error!("could not send cut action: {err}");
                                        }
                                    }
                                });
                            }
                        } else {
                            ui.separator();
                            ui.heading("Cut Preparation");
                            theme::muted(
                                ui,
                                "Cutline generation and editing are available in Print & Cut mode.",
                            );
                            if ui.button("Switch to Print & Cut").clicked()
                                && let Some(mode) = DEVICES[self.selected_device]
                                    .modes
                                    .iter()
                                    .position(|mode| mode.mode_type.has_cutting())
                            {
                                self.selected_mode = mode;
                                self.selected_canvas_size = 0;
                            }
                        }

                        ui.separator();
                        ui.heading("Canvas");

                        ui.horizontal(|ui| {
                            ui.label("Background Color");
                            ui.color_edit_button_srgb(&mut self.background_color);
                        });

                        ui.horizontal(|ui| {
                            ui.checkbox(&mut self.show_safe_area, "Safe area");
                            ui.checkbox(&mut self.snap_to_guides, "Smart snapping");
                        });
                        ui.horizontal(|ui| {
                            ui.checkbox(&mut self.show_grid, "Grid");
                            ui.checkbox(&mut self.show_rulers, "Rulers");
                        });
                        canvas_measurement_controls(
                            ui,
                            &mut self.ruler_unit,
                            &mut self.grid_spacing_mm,
                            DEVICES[self.selected_device].dpi,
                        );
                        ui.add(
                            egui::Slider::new(&mut self.pack_gap_mm, 0.0..=10.0)
                                .suffix(" mm")
                                .text("Pack gap"),
                        );
                        ui.checkbox(&mut self.pack_allow_rotation, "Rotate while packing");
                        if ui.button("Auto-pack placements").clicked() {
                            self.auto_pack();
                        }
                        if self.pack_overflow > 0 {
                            ui.colored_label(
                                Color32::YELLOW,
                                format!("{} placement(s) did not fit", self.pack_overflow),
                            );
                        }

                        if !self.loaded_images.is_empty() {
                            ui.separator();
                            let (layers_changed, artwork_action) = views::loaded_images(
                                ui,
                                &mut self.loaded_images,
                                &mut self.selected_images,
                                DEVICES[self.selected_device].modes[self.selected_mode].mode_type,
                            );
                            if layers_changed {
                                self.synchronize_cut_geometry();
                            }
                            if let Some(action) = artwork_action {
                                self.apply_artwork_menu_action(action);
                            }
                            self.selection_inspector(ui);
                        }
                    });
            });

        egui::TopBottomPanel::bottom("workspace_status")
            .exact_height(44.0)
            .frame(theme::panel_frame(ctx.style().visuals.dark_mode))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    let canvas = self.get_canvas().size;
                    let dpi = DEVICES[self.selected_device].dpi;
                    theme::muted(
                        ui,
                        format!(
                            "{:.1} × {:.1} mm  ·  {} artwork  ·  {} cut paths",
                            canvas.x / dpi * 25.4,
                            canvas.y / dpi * 25.4,
                            self.loaded_images.len(),
                            self.cut_shapes.len()
                        ),
                    );
                    if self.edit_cutlines {
                        let accent_text =
                            theme::Palette::for_accent(ui.visuals().dark_mode, accent).accent_text;
                        ui.separator();
                        ui.label(
                            egui::RichText::new("EDITING CUT NODES · Esc to finish")
                                .size(11.0)
                                .strong()
                                .color(accent_text),
                        );
                    } else if self.show_cutlines && !self.cut_shapes.is_empty() {
                        let palette = theme::Palette::for_accent(ui.visuals().dark_mode, accent);
                        ui.separator();
                        ui.colored_label(palette.kiss_text, "— Kiss cut");
                        ui.colored_label(palette.perforation_text, "-- Perforation");
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if theme::secondary_button(ui, accent, "Fit sheet")
                            .on_hover_text("Fit the full sheet in the workspace")
                            .clicked()
                        {
                            self.canvas_fit_requested = true;
                        }
                        if ui.available_width() >= 460.0 {
                            theme::muted(
                                ui,
                                "Ctrl/Cmd+scroll or pinch to zoom · drag blank canvas to pan",
                            );
                        }
                    });
                });
            });

        let workspace_fill = theme::Palette::for_dark_mode(ctx.style().visuals.dark_mode).app;
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(workspace_fill).inner_margin(16.0))
            .show(ctx, |ui| {
            if let Some(action) = views::canvas_editor(ui, self) {
                self.apply_artwork_menu_action(action);
            }
            self.synchronize_cut_geometry();

            if self.loaded_images.is_empty() {
                let hint_size = Vec2::new(300.0, 142.0);
                let hint_position = ui.max_rect().center() - hint_size / 2.0;
                egui::Area::new(Id::new("empty_workspace_hint"))
                    .order(egui::Order::Foreground)
                    .movable(false)
                    .fixed_pos(hint_position)
                    .show(ctx, |ui| {
                        ui.set_width(hint_size.x);
                        theme::card(ui.visuals().dark_mode).show(ui, |ui| {
                            ui.label(egui::RichText::new("Start with your artwork").size(18.0).strong());
                            theme::muted(ui, "Drop PNG or JPEG files here, or import them from your computer.");
                            ui.add_space(4.0);
                            if theme::primary_button(ui, accent, "+ Add artwork").clicked() {
                                self.upload_image(ctx);
                            }
                            theme::muted(ui, "Then arrange · prepare cutlines · print & cut.");
                        });
                    });
            }

            ctx.input(|i| {
                if i.raw.dropped_files.is_empty() {
                    return;
                }

                let mut files: Vec<Vec<u8>> = Vec::with_capacity(i.raw.dropped_files.len());

                for file in i.raw.dropped_files.iter() {
                    debug!("processing file");
                    let data = if cfg!(target_arch = "wasm32") {
                        match &file.bytes {
                            Some(bytes) => bytes.to_vec(),
                            None => continue,
                        }
                    } else if let Some(path) = &file.path {
                        let mut file = std::fs::File::open(path).unwrap();
                        let mut buf = Vec::new();
                        std::io::Read::read_to_end(&mut file, &mut buf).unwrap();
                        buf
                    } else {
                        continue;
                    };

                    debug!("got file contents");
                    files.push(data);
                }

                let ctx = ctx.clone();
                let tx = self.tx.clone();
                spawn(async move {
                    for file in files {
                        tx.send(Action::LoadedImage(LoadedImage::new(&ctx, &file, None)))
                            .unwrap();
                        ctx.request_repaint();
                    }
                })
            });

            if self.confirm_new_sheet {
                let modal = Modal::new(Id::new("confirm_new_sheet_modal")).show(ui.ctx(), |ui| {
                    ui.set_width(390.0);
                    ui.heading("Start a new sheet?");
                    ui.label("Starting a new sheet clears the current artwork and cutlines.");
                    theme::muted(ui, "Save this sheet first if you want to keep the current layout.");
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("Cancel").clicked() {
                            self.confirm_new_sheet = false;
                            ui.close();
                        }
                        if theme::danger_button(ui, "Start new sheet").clicked() {
                            self.start_new_sheet();
                            ui.close();
                        }
                    });
                });
                if modal.should_close() {
                    self.confirm_new_sheet = false;
                }
            }

            self.appearance_settings(ctx);
            self.calibration_profile_manager(ctx);

            if let Some(err) = &self.error {
                let modal = Modal::new(Id::new("error_modal")).show(ui.ctx(), |ui| {
                    ui.set_width(380.0);
                    ui.heading("Error");

                    ui.label(err.to_string());

                    if ui.button("Close").clicked() {
                        ui.close();
                    }
                });

                if modal.should_close() {
                    self.error = None;
                }
            }
        });

        let calibration_events = if let Some(session) = self.calibration_session.as_mut() {
            if self.calibration_ui_state.selected_printer.is_empty() {
                self.calibration_ui_state.selected_printer =
                    calibration_printer_label(&session.printer_key);
                self.calibration_ui_state.selected_media =
                    calibration_media_label(&session.material);
            }
            let manifest_identity = match session.wizard.step {
                crate::calibration::WizardStep::PrintValidation => {
                    session.validation_manifest_identity()
                }
                crate::calibration::WizardStep::PrintSecondCalibration => {
                    session.manifest_identity_for_slot(CalibrationJobSlot::Second)
                }
                _ => session.manifest_identity_for_slot(CalibrationJobSlot::Primary),
            };
            calibration_ui::show_calibration_wizard(
                ctx,
                &mut session.wizard,
                &manifest_identity,
                &mut self.calibration_ui_state,
                current_timestamp_millis(),
                calibration_ui::CalibrationUiDiagnostics {
                    training_scan: session.training_scan_report.as_ref(),
                    validation_scan: session.validation_scan_report.as_ref(),
                    training_scan_preview_png: session.training_scan_preview_png.as_ref(),
                    validation_scan_preview_png: session.validation_scan_preview_png.as_ref(),
                    training_scan_preview_sha1: session.training_scan_preview_sha1.as_deref(),
                    validation_scan_preview_sha1: session.validation_scan_preview_sha1.as_deref(),
                    candidate: session.candidate.as_ref(),
                    validation: session.validation_metrics.as_ref(),
                },
            )
        } else {
            Vec::new()
        };
        for event in calibration_events {
            match event {
                calibration_ui::CalibrationUiEvent::PrintPrimary => {
                    self.prepare_calibration_job(CalibrationJobSlot::Primary)
                }
                calibration_ui::CalibrationUiEvent::PrintSecond => {
                    self.prepare_calibration_job(CalibrationJobSlot::Second)
                }
                calibration_ui::CalibrationUiEvent::PrintValidation => {
                    self.prepare_calibration_job(CalibrationJobSlot::Validation)
                }
                calibration_ui::CalibrationUiEvent::ImportTrainingScan => {
                    self.import_calibration_scan(ScanSlot::Training)
                }
                calibration_ui::CalibrationUiEvent::ImportValidationScan => {
                    self.import_calibration_scan(ScanSlot::Validation)
                }
                calibration_ui::CalibrationUiEvent::ComputeCandidate => {
                    self.compute_calibration_candidate()
                }
                calibration_ui::CalibrationUiEvent::EvaluateValidation => {
                    self.evaluate_calibration_validation()
                }
                calibration_ui::CalibrationUiEvent::ActivateProfile => {
                    self.activate_calibration_profile()
                }
                calibration_ui::CalibrationUiEvent::SaveAndExit => {
                    if let Some(session) = self.calibration_session.as_mut() {
                        let _ = session.wizard.save_json(current_timestamp_millis());
                    }
                }
                calibration_ui::CalibrationUiEvent::Discard => {
                    self.calibration_session = None;
                }
            }
        }

        egui::Window::new("Packet Log")
            .open(&mut self.showing_packet_log)
            .default_size([1000.0, 300.0])
            .show(ctx, |ui| {
                views::protocol_packets_table(ui, &self.packets, &mut self.viewing_packet)
            });

        views::packet_debug(
            ctx,
            &self.tx,
            &mut self.showing_avocado_packet_debug,
            &self.avocado_debug_packets,
        );
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(
            storage,
            MATERIAL_PROFILES_STORAGE_KEY,
            &self.material_profiles,
        );
        eframe::set_value(storage, LIBRARY_FOLDERS_STORAGE_KEY, &self.library_folders);
        eframe::set_value(storage, LIBRARY_CYCLE_STORAGE_KEY, &self.pack_cycle);
        eframe::set_value(
            storage,
            LIBRARY_CONSUMED_STORAGE_KEY,
            &self.library_consumed_ahead,
        );
        eframe::set_value(
            storage,
            CANVAS_VIEW_STORAGE_KEY,
            &CanvasViewPreferences {
                show_grid: self.show_grid,
                show_rulers: self.show_rulers,
                ruler_unit: self.ruler_unit,
                grid_spacing_mm: sanitize_grid_spacing_mm(self.grid_spacing_mm),
            },
        );
        eframe::set_value(storage, APPEARANCE_STORAGE_KEY, &self.appearance);
        eframe::set_value(storage, CALIBRATION_STORAGE_KEY, &self.calibration_store);
        eframe::set_value(
            storage,
            PRINTER_FALLBACK_NAMES_STORAGE_KEY,
            &sanitize_printer_fallback_names(self.printer_fallback_names.clone()),
        );
        if let Some(session) = self
            .calibration_session
            .as_ref()
            .filter(|session| session.is_resumable())
        {
            eframe::set_value(storage, CALIBRATION_SESSION_STORAGE_KEY, session);
        } else {
            storage.set_string(CALIBRATION_SESSION_STORAGE_KEY, "null".into());
        }
    }
}

fn calibration_manifest(
    mut identity: ManifestIdentity,
    method: CalibrationMethod,
    validation: bool,
) -> Result<TargetManifest, crate::calibration::LayoutError> {
    if validation {
        identity.run_id.push_str("-validation");
    }
    match (method, validation) {
        (CalibrationMethod::FlatbedScanner, false) => flatbed_calibration(identity),
        (CalibrationMethod::FlatbedScanner, true) => flatbed_validation(identity),
        (CalibrationMethod::ManualEastBay, false) => manual_calibration(identity),
        (CalibrationMethod::ManualEastBay, true) => manual_validation(identity),
    }
}

fn calibration_job_spec(name: &str, printer_id: String) -> JobSpec {
    JobSpec::named(name)
        .requiring(["print", "cut"])
        .restricted_to([printer_id])
}

fn build_calibration_print_job(
    identity: ManifestIdentity,
    method: CalibrationMethod,
    slot: CalibrationJobSlot,
    material: MaterialProfile,
    mapping_override: Option<CanvasToPlotter>,
) -> anyhow::Result<PendingPrintJob> {
    let validation = slot == CalibrationJobSlot::Validation;
    let manifest = calibration_manifest(identity, method, validation)?;
    let raster = render_print_raster(&manifest)?;
    let encoded_image = encode_image(&image::DynamicImage::ImageRgb8(raster));
    let pixels_per_mm = f64::from(DEVICES[0].dpi) / 25.4;
    let mut kiss_paths = Vec::new();
    let mut through_paths = Vec::new();
    for cut in &manifest.cuts {
        let destination = match cut.mode {
            TargetCutMode::Kiss => &mut kiss_paths,
            TargetCutMode::Through => &mut through_paths,
        };
        for segment in &cut.pen_down_segments_mm {
            let points = segment
                .iter()
                .map(|point| Coord {
                    x: (point.x * pixels_per_mm) as f32,
                    y: (point.y * pixels_per_mm) as f32,
                })
                .collect::<Vec<_>>();
            if points.len() >= 2 {
                destination.push(LineString::new(points));
            }
        }
    }
    let mut calibration_phases = Vec::new();
    if !kiss_paths.is_empty() {
        calibration_phases.push(CutPhase {
            mode: CutMode::Kiss,
            pressure: material.blade_pressure,
            paths: kiss_paths,
        });
    }
    if !through_paths.is_empty() {
        calibration_phases.push(CutPhase {
            mode: CutMode::Perforation,
            pressure: material.perf_pressure,
            paths: through_paths,
        });
    }
    if calibration_phases.is_empty() {
        anyhow::bail!("calibration target did not contain cut geometry");
    }
    Ok(PendingPrintJob {
        encoded_image_len: encoded_image.len(),
        image_hash: hex::encode(sha1::Sha1::digest(&encoded_image)),
        encoded_image,
        created_at: current_timestamp_millis(),
        copies: 1,
        device_index: 0,
        mode_index: 1,
        canvas_index: 0,
        cut_shapes: Vec::new(),
        cut_modes: Vec::new(),
        material,
        perf_cut: false,
        perf_dash: 0.0,
        perf_gap: 0.0,
        peel_tabs: false,
        peel_tab_positions: Vec::new(),
        overcut: OvercutSettings::default(),
        calibration_phases: Some(calibration_phases),
        mapping_override,
    })
}

fn calibration_slot_run_id(run_id: &str, slot: CalibrationJobSlot) -> String {
    match slot {
        CalibrationJobSlot::Primary => run_id.to_owned(),
        CalibrationJobSlot::Second => format!("{run_id}-sheet-2"),
        CalibrationJobSlot::Validation => run_id.to_owned(),
    }
}

fn calibration_plotter_commands_from_plt(plt: &[u8]) -> Vec<CalibrationPlotterCommand> {
    let mut commands = String::from_utf8_lossy(plt)
        .split_ascii_whitespace()
        .filter_map(|token| {
            let (prefix, coordinates) = token.split_at_checked(1)?;
            let kind = match prefix {
                "U" => CalibrationPlotterCommandKind::Move,
                "D" => CalibrationPlotterCommandKind::Draw,
                _ => return None,
            };
            let (x, y) = coordinates.split_once(',')?;
            Some(CalibrationPlotterCommand {
                kind,
                plotter_units: [x.parse().ok()?, y.parse().ok()?],
            })
        })
        .collect::<Vec<_>>();
    if commands.last().is_some_and(|command| {
        command.kind == CalibrationPlotterCommandKind::Move && command.plotter_units == [6476, 0]
    }) {
        commands.pop();
    }
    commands
}

fn persisted_calibration_run(
    session: &CalibrationSession,
    solution: &CalibrationSolution,
    validation: ValidationMetrics,
) -> anyhow::Result<CalibrationRun> {
    let method = session
        .wizard
        .method
        .ok_or_else(|| anyhow::anyhow!("calibration method is missing"))?;
    let target_manifest = calibration_manifest(
        session.manifest_identity_for_slot(CalibrationJobSlot::Primary),
        method,
        false,
    )?;
    let target_ids = target_manifest
        .targets
        .iter()
        .map(|target| target.id.clone())
        .collect::<Vec<_>>();
    let nominal_print_mm = target_manifest
        .targets
        .iter()
        .map(|target| [target.center_mm.x, target.center_mm.y])
        .collect::<Vec<_>>();
    let mut observations = session.wizard.training_observations();
    observations.extend(session.wizard.validation_observations_for_evaluation());
    let excluded_target_ids = observations
        .iter()
        .filter(|observation| !observation.included)
        .map(|observation| observation.target_id.clone())
        .collect();
    let second_required = session.wizard.method == Some(CalibrationMethod::ManualEastBay)
        && session.wizard.second_sheet_choice
            == Some(crate::calibration::SecondSheetChoice::MeasureAnotherSheet);
    let payload_hashes = ["primary", "second", "validation"]
        .into_iter()
        .enumerate()
        .map(
            |(index, slot)| -> anyhow::Result<Option<CalibrationPayloadHashes>> {
                let (Some(jpeg_sha1), Some(plt_sha1)) = (
                    session.image_sha1[index].clone(),
                    session.plotter_sha1[index].clone(),
                ) else {
                    if matches!(slot, "primary" | "validation")
                        || (slot == "second" && second_required)
                    {
                        anyhow::bail!("{slot} payload hashes were not captured");
                    }
                    return Ok(None);
                };
                let plotter_commands = session.plotter_commands[index].clone();
                if plotter_commands.is_empty() {
                    anyhow::bail!("{slot} payload is missing its dispatched plotter commands");
                }
                Ok(Some(CalibrationPayloadHashes {
                    slot: slot.into(),
                    jpeg_sha1,
                    plt_sha1,
                    plotter_commands,
                }))
            },
        )
        .collect::<anyhow::Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect();
    Ok(CalibrationRun {
        version: crate::calibration::CALIBRATION_SCHEMA_VERSION,
        run_id: session.wizard.run_id.clone(),
        key: session.printer_key.clone(),
        method,
        baseline_profile_id: session.baseline_profile_id.clone(),
        baseline_profile_version: session.baseline_profile_version,
        validation_generation: session.wizard.validation_generation,
        baseline_mapping: session.baseline_mapping,
        manifest: CalibrationTargetManifest {
            revision: target_manifest.schema_version,
            canvas_mm: [
                target_manifest.canvas.width_mm,
                target_manifest.canvas.height_mm,
            ],
            target_ids,
            nominal_print_mm,
            jpeg_sha1: session.image_sha1[0].clone(),
            plt_sha1: session.plotter_sha1[0].clone(),
        },
        queue_job_ids: [
            session.historical_queue_job_ids[0],
            session.historical_queue_job_ids[1],
            session.historical_queue_job_ids[2],
        ]
        .into_iter()
        .flatten()
        .collect(),
        device_job_ids: session.device_job_ids.clone(),
        payload_hashes,
        observations,
        excluded_target_ids,
        printability_insets_mm: session.wizard.printability_insets_mm,
        fit_candidates: solution.candidates.clone(),
        selected_model: Some(solution.selected.model),
        validation: Some(validation),
        state: CalibrationRunState::Passed,
        created_at: session.wizard.created_at,
        updated_at: current_timestamp_millis(),
    })
}

fn scan_report_observations(
    report: &ScanAnalysisReport,
    sheet_id: &str,
) -> Vec<CalibrationObservation> {
    report
        .targets
        .iter()
        .filter_map(|target| {
            let observed_cut_mm = target.observed_center_mm?;
            let uncertainty_x = target
                .covariance
                .map(|value| value.xx_mm2.max(0.0).sqrt())
                .unwrap_or(0.15)
                .clamp(0.02, 10.0);
            let uncertainty_y = target
                .covariance
                .map(|value| value.yy_mm2.max(0.0).sqrt())
                .unwrap_or(0.15)
                .clamp(0.02, 10.0);
            Some(CalibrationObservation {
                target_id: target.target_id.clone(),
                sheet_id: sheet_id.to_owned(),
                nominal_print_mm: target.expected_center_mm,
                observed_cut_mm,
                uncertainty_mm: [uncertainty_x, uncertainty_y],
                confidence: target.confidence.clamp(0.0, 1.0),
                included: target.status == crate::calibration::ScanTargetStatus::Accepted,
            })
        })
        .collect()
}

fn calibration_scan_preview(encoded: &[u8]) -> anyhow::Result<Vec<u8>> {
    const MAX_PREVIEW_SIDE: u32 = 1_200;
    let preview = image::load_from_memory(encoded)?
        .thumbnail(MAX_PREVIEW_SIDE, MAX_PREVIEW_SIDE)
        .to_rgba8();
    let mut png = Vec::new();
    image::codecs::png::PngEncoder::new(&mut png).write_image(
        preview.as_bytes(),
        preview.width(),
        preview.height(),
        image::ExtendedColorType::Rgba8,
    )?;
    Ok(png)
}

fn observations_cover_all_quadrants(observations: &[CalibrationObservation]) -> bool {
    let mut quadrants = 0u8;
    for observation in observations
        .iter()
        .filter(|observation| observation.included)
    {
        let right = observation.nominal_print_mm[0] >= 101.6 / 2.0;
        let bottom = observation.nominal_print_mm[1] >= 177.8 / 2.0;
        quadrants |= 1 << (usize::from(bottom) * 2 + usize::from(right));
    }
    quadrants == 0b1111
}

fn validation_coverage_passed(
    method: CalibrationMethod,
    observations: &[CalibrationObservation],
) -> bool {
    let accepted = observations
        .iter()
        .filter(|observation| observation.included && observation.is_valid())
        .collect::<Vec<_>>();
    let required = match method {
        CalibrationMethod::FlatbedScanner => 6,
        CalibrationMethod::ManualEastBay => 4,
    };
    accepted.len() >= required
        && accepted
            .iter()
            .any(|value| value.nominal_print_mm[0] <= 101.6 * 0.4)
        && accepted
            .iter()
            .any(|value| value.nominal_print_mm[0] >= 101.6 * 0.6)
        && accepted
            .iter()
            .any(|value| value.nominal_print_mm[1] <= 177.8 * 0.4)
        && accepted
            .iter()
            .any(|value| value.nominal_print_mm[1] >= 177.8 * 0.6)
}

fn calibration_cut_settings(
    material: &MaterialProfile,
    method: CalibrationMethod,
    validation: bool,
) -> crate::calibration::CutSettingsProvenance {
    crate::calibration::CutSettingsProvenance {
        mode: if method == CalibrationMethod::FlatbedScanner {
            CalibrationCutMode::ThroughCut
        } else {
            CalibrationCutMode::Kiss
        },
        pressure: if method == CalibrationMethod::FlatbedScanner {
            material.perf_pressure
        } else {
            material.blade_pressure
        },
        passes: material.passes.clamp(1, 4),
        configured_speed: (1..=10).contains(&material.speed).then_some(material.speed),
        path_direction: CutPathDirection::Mixed,
        path_order_id: if validation {
            "sapodilla-calibration-validation-v1".into()
        } else {
            "sapodilla-calibration-training-v1".into()
        },
    }
}

fn greatest_common_divisor(mut left: usize, mut right: usize) -> usize {
    while right != 0 {
        (left, right) = (right, left % right);
    }
    left
}

fn commit_library_position(
    cursor: &mut usize,
    consumed_ahead: &mut BTreeSet<usize>,
    position: usize,
) {
    if position < *cursor {
        return;
    }
    consumed_ahead.insert(position);
    while consumed_ahead.remove(cursor) {
        *cursor = cursor.wrapping_add(1);
    }
}

fn can_fit_empty_sheet(size: Vec2, safe_area: Vec2, allow_rotation: bool) -> bool {
    (size.x <= safe_area.x && size.y <= safe_area.y)
        || (allow_rotation && size.y <= safe_area.x && size.x <= safe_area.y)
}

fn image_index_by_id(images: &[LoadedImage], image_id: &str) -> Option<usize> {
    images.iter().position(|image| image.id == image_id)
}

/// Maps a persistent cursor to an asset without allocating an index of the
/// whole library. Every block of `length` cursors is a permutation, so even a
/// library much larger than the resident UI page is exhausted before repeat.
fn library_cycle_index(length: usize, cursor: usize, shuffle: bool) -> usize {
    assert!(length > 0);
    let position = cursor % length;
    if !shuffle || length < 2 {
        return position;
    }

    let round = cursor / length;
    let mut state = (round as u64)
        .wrapping_add(1)
        .wrapping_mul(0x9E37_79B9_7F4A_7C15);
    state ^= state >> 12;
    state ^= state << 25;
    state ^= state >> 27;
    state = state.wrapping_mul(0x2545_F491_4F6C_DD1D);
    let start = (state as usize) % length;
    let mut stride = (((state >> 32) as usize) | 1) % length;
    if stride == 0 {
        stride = 1;
    }
    while greatest_common_divisor(stride, length) != 1 {
        stride = (stride + 1) % length;
        if stride == 0 {
            stride = 1;
        }
    }
    ((start as u128 + position as u128 * stride as u128) % length as u128) as usize
}

#[cfg(test)]
fn library_fill_order(length: usize, cycle: usize, shuffle: bool) -> Vec<usize> {
    if length == 0 {
        return Vec::new();
    }
    let start = if shuffle {
        cycle.saturating_mul(length)
    } else {
        cycle
    };
    (0..length)
        .map(|offset| library_cycle_index(length, start.saturating_add(offset), shuffle))
        .collect()
}

fn sanitize_material_profiles(mut profiles: Vec<MaterialProfile>) -> Vec<MaterialProfile> {
    profiles.truncate(128);
    profiles.retain(|profile| {
        !profile.name.trim().is_empty()
            && profile.name.len() <= 128
            && profile.blade_pressure <= 100
            && profile.perf_pressure <= 100
            && profile.passes <= 4
            && (1..=10).contains(&profile.speed)
    });
    profiles
}

fn fill_trial_succeeded(packed: &[usize], candidate_index: usize, required_count: usize) -> bool {
    packed.contains(&candidate_index) && packed.len() == required_count
}

fn reconcile_template_placeholders<'a>(
    mut placeholders: Vec<TemplatePlaceholder>,
    valid_image_ids: impl IntoIterator<Item = &'a str>,
) -> Vec<TemplatePlaceholder> {
    let valid_image_ids = valid_image_ids
        .into_iter()
        .collect::<std::collections::HashSet<_>>();
    for placeholder in &mut placeholders {
        if placeholder
            .assigned_image_id
            .as_deref()
            .is_some_and(|id| !valid_image_ids.contains(id))
        {
            placeholder.assigned_image_id = None;
        }
    }
    placeholders
}

fn modes_after_regeneration(
    previous: &[CutMode],
    previous_auto_count: usize,
    next_auto_count: usize,
    manual_count: usize,
) -> Vec<CutMode> {
    let mut modes = vec![CutMode::Kiss; next_auto_count];
    modes.extend((0..manual_count).map(|index| {
        previous
            .get(previous_auto_count + index)
            .copied()
            .unwrap_or_default()
    }));
    modes
}

fn center_path_in_rect(path: &mut LineString<f32>, target: egui::Rect, fit: bool) {
    let Some(bounds) = path.bounding_rect() else {
        return;
    };
    let source = Vec2::new(bounds.width(), bounds.height());
    let scale = if fit && source.x > f32::EPSILON && source.y > f32::EPSILON {
        (target.width() / source.x).min(target.height() / source.y)
    } else {
        1.0
    };
    let source_center = Pos2::new(
        (bounds.min().x + bounds.max().x) / 2.0,
        (bounds.min().y + bounds.max().y) / 2.0,
    );
    for point in &mut path.0 {
        point.x = target.center().x + (point.x - source_center.x) * scale;
        point.y = target.center().y + (point.y - source_center.y) * scale;
    }
}

fn place_image_in_placeholder(image: &mut LoadedImage, placeholder: &TemplatePlaceholder) {
    let target_size = Vec2::new(placeholder.bounds[2], placeholder.bounds[3]);
    image.rotation_degrees = placeholder.rotation_degrees;

    let original_scale = image.scale;
    image.scale = Vec2::splat(1.0);
    let natural_rotated = image.rotated_size();
    image.scale = match placeholder.fit {
        PlaceholderFit::Contain | PlaceholderFit::Cover => {
            let x = target_size.x / natural_rotated.x.max(1.0);
            let y = target_size.y / natural_rotated.y.max(1.0);
            let scale = if placeholder.fit == PlaceholderFit::Contain {
                x.min(y)
            } else {
                x.max(y)
            };
            Vec2::splat(scale.max(f32::EPSILON))
        }
        PlaceholderFit::Stretch => Vec2::new(
            target_size.x / image.sized_texture.size.x.max(1.0),
            target_size.y / image.sized_texture.size.y.max(1.0),
        ),
    };
    if !image.scale.is_finite() {
        image.scale = original_scale;
    }
    let rendered = image.rotated_size();
    let visual_min =
        Pos2::new(placeholder.bounds[0], placeholder.bounds[1]) + (target_size - rendered) / 2.0;
    image.offset = visual_min - (image.size() - rendered) / 2.0;
}

fn composite_layer(buffer: &mut image::RgbaImage, layer: &RenderLayer) {
    let resized = if layer.image.dimensions() == (layer.size.x as u32, layer.size.y as u32) {
        Cow::Borrowed(&layer.image)
    } else {
        Cow::Owned(image::imageops::resize(
            &layer.image,
            layer.size.x as u32,
            layer.size.y as u32,
            image::imageops::FilterType::Lanczos3,
        ))
    };
    let transformed = if layer.rotation_degrees.abs() > 0.001 {
        Cow::Owned(studio::rotate_image(&resized, layer.rotation_degrees))
    } else {
        resized
    };
    image::imageops::overlay(
        buffer,
        transformed.as_ref(),
        layer.visual_offset.x as i64,
        layer.visual_offset.y as i64,
    );
}

fn save_export(
    tx: ContextSender<Action>,
    file_name: &'static str,
    filter_name: &'static str,
    extensions: &'static [&'static str],
    bytes: Vec<u8>,
) {
    spawn(async move {
        let Some(file) = rfd::AsyncFileDialog::new()
            .add_filter(filter_name, extensions)
            .set_file_name(file_name)
            .save_file()
            .await
        else {
            return;
        };
        if let Err(error) = file.write(&bytes).await {
            let _ = tx.send(Action::Error(error.into()));
        }
    });
}

fn encode_image(im: &image::DynamicImage) -> Vec<u8> {
    const MAX_BYTES: usize = 1024 * 1024;
    let encode = |quality| {
        let mut bytes = Vec::with_capacity(MAX_BYTES);
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, quality)
            .encode_image(im)
            .unwrap();
        debug!(quality, len = bytes.len(), "got jpeg size");
        bytes
    };

    let highest = encode(100);
    if highest.len() <= MAX_BYTES {
        return highest;
    }

    let lowest = encode(0);
    if lowest.len() > MAX_BYTES {
        // Preserve the previous best-effort behaviour for sheets which cannot
        // meet the device limit even at minimum quality.
        return lowest;
    }

    // JPEG size is monotonic enough for encoder quality selection. Find the
    // highest fitting quality in at most seven additional full encodes instead
    // of walking every value from 100 downwards.
    let mut fitting_quality = 0u8;
    let mut fitting = lowest;
    let mut too_large_quality = 100u8;
    while fitting_quality + 1 < too_large_quality {
        let quality = fitting_quality + (too_large_quality - fitting_quality) / 2;
        let candidate = encode(quality);
        if candidate.len() <= MAX_BYTES {
            fitting_quality = quality;
            fitting = candidate;
        } else {
            too_large_quality = quality;
        }
    }
    fitting
}

#[allow(clippy::too_many_arguments)]
#[derive(Clone, Copy, Debug, PartialEq)]
struct PlotterMapping {
    direct: CanvasToPlotter,
    legacy_f32: Option<LegacyF32Mapping>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct LegacyF32Mapping {
    canvas_height: f32,
    scale: f32,
    offset: Vec2,
}

impl PlotterMapping {
    fn direct(mapping: CanvasToPlotter) -> Self {
        Self {
            direct: mapping,
            legacy_f32: None,
        }
    }

    fn apply(self, point: Coord<f32>) -> [f64; 2] {
        if let Some(legacy) = self.legacy_f32 {
            // Preserve the historical f32 operation order for the stock
            // mapping. Calibrated profiles use the direct f64 affine path.
            return [
                f64::from((legacy.canvas_height - point.y + legacy.offset.y) * legacy.scale),
                f64::from((point.x + legacy.offset.x) * legacy.scale),
            ];
        }
        self.direct.apply([f64::from(point.x), f64::from(point.y)])
    }
}

#[allow(clippy::too_many_arguments)]
fn encode_plt(
    cut_shapes: &[LineString<f32>],
    cut_modes: &[CutMode],
    plotter_mapping: PlotterMapping,
    canvas_size: &CanvasSize,
    material: &MaterialProfile,
    perf_cut_enabled: bool,
    perf_dash: f32,
    perf_gap: f32,
    peel_tabs: bool,
    peel_tab_positions: &[Option<f32>],
    overcut: OvercutSettings,
) -> Vec<u8> {
    let modes = effective_cut_modes(cut_shapes.len(), cut_modes, perf_cut_enabled);
    let kiss_paths = cut_shapes
        .iter()
        .zip(&modes)
        .map(|(path, mode)| {
            if *mode == CutMode::Kiss && overcut.enabled && path.0.first() == path.0.last() {
                apply_overcut(path, overcut)
            } else {
                path.clone()
            }
        })
        .collect::<Vec<_>>();
    let mut phases = plan_cut_phases(
        &kiss_paths,
        &modes,
        material.blade_pressure,
        material.perf_pressure,
        perf_dash,
        perf_gap,
    );
    if peel_tabs {
        let enabled = modes
            .iter()
            .map(|mode| *mode != CutMode::Disabled)
            .collect::<Vec<_>>();
        let tabs = build_peel_tabs(cut_shapes, &enabled, peel_tab_positions)
            .into_iter()
            .map(|(_, tab)| tab.path)
            .collect::<Vec<_>>();
        if !tabs.is_empty() {
            if let Some(kiss) = phases.iter_mut().find(|phase| phase.mode == CutMode::Kiss) {
                kiss.paths.extend(tabs);
            } else {
                phases.insert(
                    0,
                    crate::toolpath::CutPhase {
                        mode: CutMode::Kiss,
                        pressure: material.blade_pressure,
                        paths: tabs,
                    },
                );
            }
        }
    }

    let mut buf = b"IN VER0.1.0".to_vec();
    for phase in phases {
        write!(buf, " KP{}", phase.pressure).unwrap();
        let mut ordered = phase.paths;
        ordered.sort_by(|a, b| {
            let a_start = *a.0.first().unwrap();
            let b_start = *b.0.first().unwrap();
            (canvas_size.size.y - a_start.y)
                .total_cmp(&(canvas_size.size.y - b_start.y))
                .then(a_start.x.total_cmp(&b_start.x))
        });
        for _ in 0..material.passes {
            for line in &ordered {
                write_line_string(plotter_mapping, &mut buf, line);
            }
        }
    }

    write!(buf, " U6476,0  @ ").unwrap();

    buf
}

fn encode_calibration_plt(
    phases: &[CutPhase],
    plotter_mapping: PlotterMapping,
    passes: u8,
) -> Vec<u8> {
    let mut buf = b"IN VER0.1.0".to_vec();
    for phase in phases {
        if phase.paths.is_empty() {
            continue;
        }
        write!(buf, " KP{}", phase.pressure).unwrap();
        for _ in 0..passes.max(1) {
            for path in &phase.paths {
                if !path.0.is_empty() {
                    write_line_string(plotter_mapping, &mut buf, path);
                }
            }
        }
    }
    write!(buf, " U6476,0  @ ").unwrap();
    buf
}

fn write_line_string(
    plotter_mapping: PlotterMapping,
    buf: &mut Vec<u8>,
    line_shape: &geo::LineString<f32>,
) {
    let first = plotter_mapping.apply(line_shape.0[0]);
    write!(buf, " U{:.0},{:.0}", first[0], first[1]).unwrap();

    for point in line_shape.coords() {
        let mapped = plotter_mapping.apply(*point);
        write!(buf, " D{:.0},{:.0}", mapped[0], mapped[1]).unwrap();
    }
}

fn stock_plotter_mapping(device_index: usize, canvas_size: &CanvasSize) -> PlotterMapping {
    let calibration = DEVICES[device_index]
        .cutter_calibration
        .clone()
        .unwrap_or_default();
    let direct = CanvasToPlotter::from_legacy_components(
        f64::from(canvas_size.size.y),
        f64::from(calibration.scale_factor),
        [
            f64::from(calibration.offset.x),
            f64::from(calibration.offset.y),
        ],
    );
    PlotterMapping {
        direct,
        legacy_f32: Some(LegacyF32Mapping {
            canvas_height: canvas_size.size.y,
            scale: calibration.scale_factor,
            offset: calibration.offset,
        }),
    }
}

fn resolve_routed_canvas_to_plotter(
    store: &CalibrationStore,
    identities: &BTreeMap<String, PrinterIdentityInfo>,
    fallback_names: &BTreeMap<String, String>,
    printer_id: &str,
    device_index: usize,
    canvas_size: &CanvasSize,
) -> PlotterMapping {
    let stock = stock_plotter_mapping(device_index, canvas_size);
    let Some(key) =
        calibration_key_for_printer(identities, fallback_names, printer_id, canvas_size)
    else {
        return stock;
    };
    store
        .active_profile(&key)
        .map(|profile| PlotterMapping::direct(profile.canvas_to_plotter))
        .unwrap_or(stock)
}

fn calibration_key_for_printer(
    identities: &BTreeMap<String, PrinterIdentityInfo>,
    fallback_names: &BTreeMap<String, String>,
    printer_id: &str,
    canvas_size: &CanvasSize,
) -> Option<PrinterCalibrationKey> {
    let identity = identities.get(printer_id)?;
    let stable_identity = if let Some(serial_number) = identity.serial_number.as_ref() {
        StablePrinterIdentity::SerialNumber {
            serial_number: serial_number.clone(),
        }
    } else {
        // A transient transport identifier must never select a persisted profile.
        let profile_name = fallback_names.get(printer_id)?;
        StablePrinterIdentity::NamedFallback {
            profile_name: profile_name.clone(),
        }
    };
    Some(PrinterCalibrationKey {
        identity: stable_identity,
        model: identity.model.clone(),
        firmware_revision: identity.firmware_revision.clone(),
        media_size: canvas_size.media_size,
        media_type: canvas_size.media_type,
    })
}

fn calibration_printer_label(key: &PrinterCalibrationKey) -> String {
    let identity = match &key.identity {
        StablePrinterIdentity::SerialNumber { serial_number } => {
            format!("serial {serial_number}")
        }
        StablePrinterIdentity::NamedFallback { profile_name } => {
            format!("named {profile_name}")
        }
    };
    format!(
        "{} · {} · firmware {}",
        key.model, identity, key.firmware_revision
    )
}

fn unique_named_fallback_for_identity(
    store: &CalibrationStore,
    identity: &PrinterIdentityInfo,
) -> Option<String> {
    let names = store
        .active_profiles
        .iter()
        .filter_map(|active| {
            let profile = store.profiles.iter().find(|profile| {
                profile.profile_id == active.profile_id && profile.key == active.key
            })?;
            if profile.key.model != identity.model
                || profile.key.firmware_revision != identity.firmware_revision
            {
                return None;
            }
            match &profile.key.identity {
                StablePrinterIdentity::NamedFallback { profile_name } => Some(profile_name.clone()),
                StablePrinterIdentity::SerialNumber { .. } => None,
            }
        })
        .collect::<BTreeSet<_>>();
    (names.len() == 1)
        .then(|| names.into_iter().next())
        .flatten()
}

fn calibration_media_label(material: &MaterialProfile) -> String {
    format!(
        "PixCut S1 · 4×7 sticker paper · {} · kiss {} · through {} · {} pass{}",
        material.name,
        material.blade_pressure,
        material.perf_pressure,
        material.passes,
        if material.passes == 1 { "" } else { "es" }
    )
}

fn sanitize_printer_fallback_names(names: BTreeMap<String, String>) -> BTreeMap<String, String> {
    names
        .into_iter()
        .filter_map(|(printer_id, profile_name)| {
            let printer_id = printer_id.trim();
            let profile_name = profile_name.trim();
            if printer_id.is_empty() || profile_name.is_empty() {
                return None;
            }
            Some((
                printer_id.chars().take(128).collect(),
                profile_name.chars().take(128).collect(),
            ))
        })
        .take(32)
        .collect()
}

#[cfg(target_arch = "wasm32")]
fn current_timestamp_millis() -> u64 {
    web_sys::window().unwrap().performance().unwrap().now() as u64
}

#[cfg(test)]
mod ui_tests;

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::*;

    #[test]
    fn appearance_preferences_round_trip_and_reject_unknown_versions() {
        let preferences = AppearancePreferences {
            version: APPEARANCE_VERSION,
            accent: theme::AccentChoice::Custom([12, 96, 211]),
            theme: egui::ThemePreference::Dark,
        };
        let encoded = serde_json::to_string(&preferences).unwrap();
        let decoded: AppearancePreferences = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, preferences);
        assert_eq!(sanitize_appearance(decoded), preferences);

        let legacy: AppearancePreferences =
            serde_json::from_str(r#"{"version":1,"accent":{"custom":[12,96,211]}}"#).unwrap();
        assert_eq!(legacy.accent, preferences.accent);
        assert_eq!(legacy.theme, egui::ThemePreference::System);

        assert_eq!(
            sanitize_appearance(AppearancePreferences {
                version: APPEARANCE_VERSION + 1,
                accent: theme::AccentChoice::Preset(theme::AccentPreset::SignalLime),
                theme: egui::ThemePreference::Light,
            }),
            AppearancePreferences::default()
        );
    }

    #[test]
    fn canvas_units_convert_using_the_selected_device_dpi() {
        let dpi = 254.0;
        assert!((CanvasUnit::Px.from_mm(25.4, dpi) - 254.0).abs() < 0.001);
        assert!((CanvasUnit::Pt.from_mm(25.4, dpi) - 72.0).abs() < 0.001);
        assert!((CanvasUnit::Mm.from_mm(25.4, dpi) - 25.4).abs() < 0.001);
        assert!((CanvasUnit::Cm.from_mm(25.4, dpi) - 2.54).abs() < 0.001);
        assert!((CanvasUnit::In.from_mm(25.4, dpi) - 1.0).abs() < 0.001);

        for unit in CanvasUnit::ALL {
            let displayed = unit.from_mm(12.7, dpi);
            assert!((unit.to_mm(displayed, dpi) - 12.7).abs() < 0.001);
        }
    }

    #[test]
    fn legacy_canvas_preferences_default_to_millimetres() {
        let preferences: CanvasViewPreferences =
            serde_json::from_str(r#"{"show_grid":true,"show_rulers":true,"grid_spacing_mm":10.0}"#)
                .unwrap();
        assert_eq!(preferences.ruler_unit, CanvasUnit::Mm);
        assert_eq!(sanitize_grid_spacing_mm(f32::NAN), 10.0);
        assert_eq!(sanitize_grid_spacing_mm(f32::INFINITY), 10.0);
        assert_eq!(sanitize_grid_spacing_mm(0.1), 0.5);
        assert_eq!(sanitize_grid_spacing_mm(200.0), 100.0);
    }

    fn cut_geometry(offset_x: f32) -> CutGeometrySnapshot {
        CutGeometrySnapshot {
            device: 0,
            mode: 0,
            canvas_size: 0,
            tuning: CutTuningSnapshot {
                buffer: 1.0_f32.to_bits(),
                minimum_length: 1.0_f32.to_bits(),
                smoothing: 1,
                simplify: 1.0_f32.to_bits(),
                internal: false,
                white_transparent: true,
            },
            images: vec![CutImageGeometry {
                id: "image-a".into(),
                offset: [offset_x.to_bits(), 0.0_f32.to_bits()],
                scale: [1.0_f32.to_bits(), 1.0_f32.to_bits()],
                rotation_degrees: 0.0_f32.to_bits(),
                content_revision: 0,
                visible: true,
                enable_cutting: true,
            }],
        }
    }

    #[test]
    fn every_geometry_snapshot_change_invalidates_once() {
        let original = cut_geometry(10.0);
        let transformed = cut_geometry(20.0);
        let mut tracked = None;
        assert!(update_cut_geometry_snapshot(&mut tracked, original.clone()));
        assert!(!update_cut_geometry_snapshot(&mut tracked, original));
        assert!(update_cut_geometry_snapshot(
            &mut tracked,
            transformed.clone()
        ));
        assert_eq!(tracked, Some(transformed));
    }

    #[test]
    fn asynchronous_cut_result_key_rejects_later_sidebar_transform() {
        let generation_source = cut_geometry(10.0);
        let after_sidebar_move = cut_geometry(11.0);
        assert!(!should_accept_cut_action(
            Some(7),
            7,
            &generation_source,
            &after_sidebar_move
        ));
        assert!(!should_accept_cut_action(
            Some(8),
            7,
            &generation_source,
            &generation_source
        ));
        assert!(should_accept_cut_action(
            Some(7),
            7,
            &generation_source,
            &generation_source
        ));
    }

    #[test]
    fn raster_and_tuning_changes_invalidate_cut_inputs() {
        let original = cut_geometry(10.0);
        let mut raster_changed = original.clone();
        raster_changed.images[0].content_revision += 1;
        assert_ne!(original, raster_changed);

        let mut tuning_changed = original.clone();
        tuning_changed.tuning.simplify = 2.0_f32.to_bits();
        assert_ne!(original, tuning_changed);
    }

    #[test]
    fn production_preflight_blocks_missing_inputs_and_invalid_cut_geometry() {
        assert_eq!(
            print_block_reason(false, true, false, false, false, false, false),
            Some("Connect a printer before starting production.")
        );
        assert_eq!(
            print_block_reason(true, false, false, false, false, false, false),
            Some("Add artwork before starting production.")
        );
        assert_eq!(
            print_block_reason(true, true, true, true, true, false, false),
            Some("Wait for cutline generation to finish.")
        );
        assert_eq!(
            print_block_reason(true, true, true, false, false, false, false),
            Some("Generate at least one enabled cutline before starting production.")
        );
        assert!(print_block_reason(true, true, true, false, true, true, false).is_some());
        assert!(print_block_reason(true, true, true, false, true, false, true).is_some());
        assert_eq!(
            print_block_reason(true, true, false, true, false, true, true),
            None,
            "cut warnings do not block a print-only job"
        );
        assert_eq!(
            print_block_reason(true, true, true, false, true, false, false),
            None
        );
    }

    #[test]
    fn production_preflight_requires_a_current_effective_cut_path() {
        let valid = LineString::from(vec![(0.0, 0.0), (10.0, 0.0)]);
        let degenerate = LineString::from(vec![(1.0, 1.0), (1.0, 1.0)]);

        assert!(!has_enabled_cut_path(&[], &[], false), "empty paths");
        assert!(
            !has_enabled_cut_path(std::slice::from_ref(&valid), &[CutMode::Disabled], false,),
            "all-disabled paths"
        );
        assert!(
            !has_enabled_cut_path(&[degenerate], &[CutMode::Kiss], false),
            "degenerate geometry"
        );
        assert!(
            has_enabled_cut_path(std::slice::from_ref(&valid), &[], false),
            "missing legacy modes default to kiss cut"
        );

        let mut transform_invalidated_paths = vec![valid];
        transform_invalidated_paths.clear();
        assert!(
            !has_enabled_cut_path(&transform_invalidated_paths, &[], false),
            "a transform that invalidates auto paths must block until regeneration"
        );
    }

    #[test]
    fn current_cut_validation_tracks_manual_geometry_and_ignores_disabled_paths() {
        let canvas_size = Vec2::new(100.0, 100.0);
        let safe_area = Vec2::new(80.0, 80.0);
        let horizontal = LineString::from(vec![(20.0, 50.0), (80.0, 50.0)]);
        let vertical = LineString::from(vec![(50.0, 20.0), (50.0, 80.0)]);
        let outside = LineString::from(vec![(5.0, 20.0), (20.0, 20.0)]);

        assert_eq!(
            validate_current_cut_paths(
                std::slice::from_ref(&horizontal),
                &[CutMode::Kiss],
                false,
                canvas_size,
                safe_area,
            ),
            CutPathValidation::default()
        );

        let crossed = validate_current_cut_paths(
            &[horizontal.clone(), vertical],
            &[CutMode::Kiss, CutMode::Perforation],
            false,
            canvas_size,
            safe_area,
        );
        assert!(crossed.has_intersections);
        assert!(!crossed.off_canvas);

        let moved_outside = validate_current_cut_paths(
            &[horizontal.clone(), outside.clone()],
            &[CutMode::Kiss, CutMode::Kiss],
            false,
            canvas_size,
            safe_area,
        );
        assert!(moved_outside.off_canvas);

        let disabled_problem_paths = validate_current_cut_paths(
            &[horizontal, outside],
            &[CutMode::Disabled, CutMode::Disabled],
            false,
            canvas_size,
            safe_area,
        );
        assert_eq!(disabled_problem_paths, CutPathValidation::default());
    }

    #[test]
    fn cut_validation_snapshot_changes_only_with_preflight_inputs() {
        let mut path = LineString::from(vec![(20.0, 50.0), (80.0, 50.0)]);
        let before = cut_validation_snapshot(
            std::slice::from_ref(&path),
            &[CutMode::Kiss],
            false,
            Vec2::new(100.0, 100.0),
            Vec2::new(80.0, 80.0),
        );
        let identical = cut_validation_snapshot(
            std::slice::from_ref(&path),
            &[CutMode::Kiss],
            false,
            Vec2::new(100.0, 100.0),
            Vec2::new(80.0, 80.0),
        );
        assert_eq!(before, identical);
        let perforation = cut_validation_snapshot(
            std::slice::from_ref(&path),
            &[CutMode::Perforation],
            true,
            Vec2::new(100.0, 100.0),
            Vec2::new(80.0, 80.0),
        );
        assert_eq!(
            before, perforation,
            "changing cut pressure mode does not affect geometric preflight"
        );
        let disabled = cut_validation_snapshot(
            std::slice::from_ref(&path),
            &[CutMode::Disabled],
            false,
            Vec2::new(100.0, 100.0),
            Vec2::new(80.0, 80.0),
        );
        assert_ne!(before, disabled);

        path.0[0].x = 5.0;
        let after_manual_edit = cut_validation_snapshot(
            &[path],
            &[CutMode::Kiss],
            false,
            Vec2::new(100.0, 100.0),
            Vec2::new(80.0, 80.0),
        );
        assert_ne!(before, after_manual_edit);
    }

    #[test]
    fn peel_tabs_participate_in_safe_area_validation_and_cache_keys() {
        let path = LineString::from(vec![
            (20.0, 60.0),
            (80.0, 60.0),
            (80.0, 90.0),
            (20.0, 90.0),
            (20.0, 60.0),
        ]);
        let without_tab = validate_current_cut_paths_with_tabs(
            std::slice::from_ref(&path),
            &[CutMode::Kiss],
            false,
            Vec2::splat(100.0),
            Vec2::splat(100.0),
            false,
            &[],
        );
        assert!(!without_tab.off_canvas);
        let with_tab = validate_current_cut_paths_with_tabs(
            std::slice::from_ref(&path),
            &[CutMode::Kiss],
            false,
            Vec2::splat(100.0),
            Vec2::splat(100.0),
            true,
            &[None],
        );
        assert!(with_tab.off_canvas);

        let default_key = cut_validation_snapshot_with_tabs(
            std::slice::from_ref(&path),
            &[CutMode::Kiss],
            false,
            Vec2::splat(100.0),
            Vec2::splat(100.0),
            true,
            &[None],
        );
        let moved_key = cut_validation_snapshot_with_tabs(
            &[path],
            &[CutMode::Kiss],
            false,
            Vec2::splat(100.0),
            Vec2::splat(100.0),
            true,
            &[Some(0.125)],
        );
        assert_ne!(default_key, moved_key);
    }

    #[test]
    fn template_relationships_survive_reorder_and_clear_after_delete() {
        let placeholder = TemplatePlaceholder {
            id: "slot".into(),
            name: "Slot".into(),
            bounds: [0.0, 0.0, 10.0, 10.0],
            rotation_degrees: 0.0,
            fit: PlaceholderFit::Contain,
            assigned_image_id: Some("image-b".into()),
        };
        let reordered =
            reconcile_template_placeholders(vec![placeholder.clone()], ["image-b", "image-a"]);
        assert_eq!(reordered[0].assigned_image_id.as_deref(), Some("image-b"));

        let after_delete = reconcile_template_placeholders(vec![placeholder], ["image-a"]);
        assert_eq!(after_delete[0].assigned_image_id, None);
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn disk_library_scan_keeps_resident_index_page_bounded() {
        let root = std::env::temp_dir().join(format!(
            "sapodilla-library-page-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        for index in 0..250 {
            std::fs::write(root.join(format!("asset-{index:03}.png")), []).unwrap();
        }
        let (page, has_more) = scan_library_page(&[root.to_string_lossy().into_owned()], 0, 32);
        assert_eq!(page.len(), 32);
        assert!(has_more);
        assert!(page.iter().all(|path| path.is_absolute()));
        let folder = root.to_string_lossy().into_owned();
        let mut all = std::collections::BTreeSet::new();
        for page_index in 0..8 {
            let (page, more) = scan_library_page(std::slice::from_ref(&folder), page_index, 32);
            assert!(page.into_iter().all(|path| all.insert(path)));
            assert_eq!(more, page_index < 7);
        }
        assert_eq!(all.len(), 250);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn disk_library_scan_does_not_follow_directory_links() {
        let root = std::env::temp_dir().join(format!(
            "sapodilla-library-cycle-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("real.png"), []).unwrap();
        #[cfg(unix)]
        let linked = std::os::unix::fs::symlink(&root, root.join("cycle"));
        #[cfg(windows)]
        let linked = std::os::windows::fs::symlink_dir(&root, root.join("cycle"));
        if linked.is_ok() {
            let (page, more) = scan_library_page(&[root.to_string_lossy().into_owned()], 0, 10);
            assert_eq!(page.len(), 1);
            assert!(!more);
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn placeholder_fit_controls_replacement_geometry() {
        let context = egui::Context::default();
        let source = image::RgbaImage::new(100, 50);
        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(source.as_bytes(), 100, 50, image::ExtendedColorType::Rgba8)
            .unwrap();
        let mut image = LoadedImage::new(&context, &png, None).unwrap();
        let mut slot = TemplatePlaceholder {
            id: "slot".into(),
            name: "Slot".into(),
            bounds: [10.0, 20.0, 200.0, 200.0],
            rotation_degrees: 0.0,
            fit: PlaceholderFit::Contain,
            assigned_image_id: Some(image.id.clone()),
        };

        place_image_in_placeholder(&mut image, &slot);
        assert_eq!(image.size(), Vec2::new(200.0, 100.0));
        assert_eq!(image.visual_offset(), Pos2::new(10.0, 70.0));

        slot.fit = PlaceholderFit::Cover;
        place_image_in_placeholder(&mut image, &slot);
        assert_eq!(image.size(), Vec2::new(400.0, 200.0));
        assert_eq!(image.visual_offset(), Pos2::new(-90.0, 20.0));
    }

    fn canvas() -> CanvasSize {
        CanvasSize {
            name: "test".into(),
            media_size: 0,
            media_type: 0,
            size: Vec2::new(1200.0, 2100.0),
            safe_area: Vec2::new(1200.0, 2100.0),
        }
    }

    fn unscaled_test_mapping(canvas: &CanvasSize) -> PlotterMapping {
        PlotterMapping::direct(CanvasToPlotter::from_legacy_components(
            f64::from(canvas.size.y),
            1.0,
            [0.0, 0.0],
        ))
    }

    #[test]
    fn new_artwork_normalization_handles_small_and_oversized_sources() {
        let context = egui::Context::default();
        let source = image::RgbaImage::new(240, 150);
        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(
                source.as_bytes(),
                source.width(),
                source.height(),
                image::ExtendedColorType::Rgba8,
            )
            .unwrap();

        let sheet = canvas();
        let mut small = LoadedImage::new(&context, &png, None).unwrap();
        normalize_new_artwork(&mut small, &sheet);
        assert!(small.size().x >= sheet.safe_area.x * NEW_ARTWORK_MIN_FRACTION);
        assert!(small.size().y >= sheet.safe_area.y * NEW_ARTWORK_MIN_FRACTION);
        assert_eq!(
            small.visual_offset(),
            ((sheet.size - small.rotated_size()) / 2.0).to_pos2()
        );

        let mut oversized = LoadedImage::new(&context, &png, None).unwrap();
        oversized.sized_texture.size = Vec2::new(4000.0, 3000.0);
        normalize_new_artwork(&mut oversized, &sheet);
        assert!(oversized.size().x <= sheet.safe_area.x * NEW_ARTWORK_MAX_FRACTION + 0.01);
        assert!(oversized.size().y <= sheet.safe_area.y * NEW_ARTWORK_MAX_FRACTION + 0.01);
        assert!(oversized.scale.x.is_finite() && oversized.scale.x > 0.0);
        assert_eq!(oversized.scale.x, oversized.scale.y);
        assert_eq!(
            oversized.visual_offset(),
            ((sheet.size - oversized.rotated_size()) / 2.0).to_pos2()
        );
    }

    #[test]
    fn library_fill_cycles_and_shuffle_are_deterministic() {
        assert_eq!(library_fill_order(4, 0, false), vec![0, 1, 2, 3]);
        assert_eq!(library_fill_order(4, 1, false), vec![1, 2, 3, 0]);
        let shuffled = library_fill_order(8, 3, true);
        assert_eq!(shuffled, library_fill_order(8, 3, true));
        assert_ne!(shuffled, (0..8).collect::<Vec<_>>());
        let mut sorted = shuffled;
        sorted.sort_unstable();
        assert_eq!(sorted, (0..8).collect::<Vec<_>>());
    }

    #[test]
    fn library_cycle_exhausts_assets_larger_than_ui_page_before_repeat() {
        let length = LIBRARY_PAGE_SIZE + 37;
        for shuffle in [false, true] {
            for round in 0..2 {
                let start = round * length;
                let visited = (start..start + length)
                    .map(|cursor| library_cycle_index(length, cursor, shuffle))
                    .collect::<std::collections::BTreeSet<_>>();
                assert_eq!(visited.len(), length);
                assert_eq!(visited.first(), Some(&0));
                assert_eq!(visited.last(), Some(&(length - 1)));
            }
        }
    }

    #[test]
    fn production_cursor_preserves_multi_sheet_order_without_early_repeats() {
        let asset_count = 1_000;
        let sheet_capacity = 20;
        for shuffle in [false, true] {
            let mut cursor = 0;
            let mut placed = Vec::new();
            for _sheet in 0..asset_count / sheet_capacity {
                let sheet = (0..sheet_capacity)
                    .map(|_| {
                        let index = library_cycle_index(asset_count, cursor, shuffle);
                        cursor += 1; // mirrors a successful placement commit
                        index
                    })
                    .collect::<Vec<_>>();
                placed.extend(sheet);
            }
            assert_eq!(placed.len(), asset_count);
            assert_eq!(
                placed
                    .iter()
                    .copied()
                    .collect::<std::collections::BTreeSet<_>>()
                    .len(),
                asset_count
            );
            assert_eq!(cursor, asset_count);
        }
    }

    #[test]
    fn oversized_asset_is_consumed_instead_of_blocking_a_later_small_asset() {
        assert!(!can_fit_empty_sheet(
            Vec2::new(2_000.0, 2_000.0),
            Vec2::new(1_000.0, 1_500.0),
            true,
        ));
        let mut cursor = 0;
        let mut consumed = BTreeSet::new();
        commit_library_position(&mut cursor, &mut consumed, 0);
        commit_library_position(&mut cursor, &mut consumed, 1);
        assert_eq!(cursor, 2);
        assert!(consumed.is_empty());
    }

    #[test]
    fn gap_rejection_is_deferred_while_a_later_small_asset_is_consumed() {
        let mut cursor = 0;
        let mut consumed = BTreeSet::new();
        // Position zero did not fit the current gap, so only the later small
        // asset is committed on this sheet.
        commit_library_position(&mut cursor, &mut consumed, 1);
        assert_eq!(cursor, 0);
        assert_eq!(consumed, BTreeSet::from([1]));
        // It fits on the next blank sheet. Advancing also skips position one,
        // which was already printed, so no item repeats before the cycle ends.
        commit_library_position(&mut cursor, &mut consumed, 0);
        assert_eq!(cursor, 2);
        assert!(consumed.is_empty());
    }

    #[test]
    fn async_image_results_follow_stable_identity_across_reorder_and_delete() {
        let context = egui::Context::default();
        let source = image::RgbaImage::new(1, 1);
        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(source.as_bytes(), 1, 1, image::ExtendedColorType::Rgba8)
            .unwrap();
        let mut first = LoadedImage::new(&context, &png, None).unwrap();
        first.id = "first".into();
        let mut second = LoadedImage::new(&context, &png, None).unwrap();
        second.id = "second".into();
        let mut images = vec![first, second];
        images.swap(0, 1);
        assert_eq!(image_index_by_id(&images, "first"), Some(1));
        images.remove(1);
        assert_eq!(image_index_by_id(&images, "first"), None);
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn fill_index_is_built_by_one_bounded_decode_pass() {
        let root = std::env::temp_dir().join(format!(
            "sapodilla-library-fill-index-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("nested")).unwrap();
        for index in 0..(LIBRARY_PAGE_SIZE + 37) {
            let folder = if index % 2 == 0 {
                root.clone()
            } else {
                root.join("nested")
            };
            std::fs::write(folder.join(format!("asset-{index:03}.png")), []).unwrap();
        }
        let paths = collect_library_paths(&[root.to_string_lossy().into_owned()]);
        assert_eq!(paths.len(), LIBRARY_PAGE_SIZE + 37);
        assert!(paths.len() < MAX_LIBRARY_FILL_ATTEMPTS);
        assert!(paths.iter().all(|path| is_library_image_path(path)));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn regenerated_auto_paths_reset_to_kiss_and_preserve_manual_modes() {
        let modes = modes_after_regeneration(
            &[
                CutMode::Perforation,
                CutMode::Disabled,
                CutMode::Perforation,
            ],
            2,
            3,
            2,
        );
        assert_eq!(
            modes,
            [
                CutMode::Kiss,
                CutMode::Kiss,
                CutMode::Kiss,
                CutMode::Perforation,
                CutMode::Kiss,
            ]
        );
    }

    #[test]
    fn path_can_be_centered_and_fit_to_artwork_bounds() {
        let mut path = LineString::from(vec![(0.0, 0.0), (10.0, 0.0), (10.0, 5.0)]);
        center_path_in_rect(
            &mut path,
            egui::Rect::from_min_size(Pos2::new(20.0, 30.0), Vec2::new(40.0, 40.0)),
            true,
        );
        let bounds = path.bounding_rect().unwrap();
        assert!((bounds.width() - 40.0).abs() < 0.001);
        assert!((bounds.height() - 20.0).abs() < 0.001);
        assert!(((bounds.min().y + bounds.max().y) / 2.0 - 50.0).abs() < 0.001);
    }

    #[test]
    fn material_profile_sanitization_preserves_valid_custom_profiles() {
        let valid = MaterialProfile {
            name: "Vinyl".into(),
            blade_pressure: 42,
            perf_pressure: 53,
            passes: 1,
            speed: 5,
        };
        let mut invalid = valid.clone();
        invalid.name.clear();
        assert_eq!(
            sanitize_material_profiles(vec![invalid, valid.clone()]),
            vec![valid]
        );
    }

    #[test]
    fn fill_rejects_candidate_that_displaces_an_existing_sticker() {
        assert!(!fill_trial_succeeded(&[1], 1, 2));
        assert!(fill_trial_succeeded(&[0, 1], 1, 2));
    }

    #[test]
    fn plt_uses_material_pressure_and_valid_terminator() {
        let path = LineString::from(vec![(10.0, 10.0), (20.0, 10.0), (10.0, 10.0)]);
        let canvas = canvas();
        let material = MaterialProfile {
            name: "test".into(),
            blade_pressure: 53,
            perf_pressure: 63,
            passes: 1,
            speed: 5,
        };
        let data = encode_plt(
            &[path],
            &[],
            unscaled_test_mapping(&canvas),
            &canvas,
            &material,
            false,
            2.0,
            1.0,
            false,
            &[],
            OvercutSettings {
                enabled: false,
                ..OvercutSettings::default()
            },
        );
        let text = String::from_utf8(data).unwrap();
        assert!(text.starts_with("IN VER0.1.0 KP53"));
        assert!(text.ends_with(" U6476,0  @ "));
        assert!(!text.contains("NaN") && !text.contains("None"));
    }

    #[test]
    fn perf_output_has_multiple_blade_up_moves() {
        let path = LineString::from(vec![(0.0, 0.0), (30.0, 0.0)]);
        let canvas = canvas();
        let material = MaterialProfile {
            name: "test".into(),
            blade_pressure: 53,
            perf_pressure: 63,
            passes: 1,
            speed: 5,
        };
        let data = encode_plt(
            &[path],
            &[],
            unscaled_test_mapping(&canvas),
            &canvas,
            &material,
            true,
            5.0,
            5.0,
            false,
            &[],
            OvercutSettings {
                enabled: false,
                ..OvercutSettings::default()
            },
        );
        let text = String::from_utf8(data).unwrap();
        assert!(text.starts_with("IN VER0.1.0 KP63"));
        assert!(text.matches(" U").count() >= 4); // three dashes plus park
    }

    #[test]
    fn mixed_cut_modes_emit_kiss_before_perf_and_omit_disabled() {
        let paths = vec![
            LineString::from(vec![(10.0, 10.0), (20.0, 10.0), (10.0, 10.0)]),
            LineString::from(vec![(30.0, 30.0), (60.0, 30.0)]),
            LineString::from(vec![(999.0, 999.0), (1000.0, 999.0)]),
        ];
        let material = MaterialProfile {
            name: "test".into(),
            blade_pressure: 42,
            perf_pressure: 53,
            passes: 1,
            speed: 5,
        };
        let canvas = canvas();
        let text = String::from_utf8(encode_plt(
            &paths,
            &[CutMode::Kiss, CutMode::Perforation, CutMode::Disabled],
            unscaled_test_mapping(&canvas),
            &canvas,
            &material,
            false,
            5.0,
            2.0,
            false,
            &[],
            OvercutSettings {
                enabled: false,
                ..OvercutSettings::default()
            },
        ))
        .unwrap();
        assert!(text.find("KP42").unwrap() < text.find("KP53").unwrap());
        assert!(!text.contains("999"));
    }

    #[test]
    fn peel_tabs_do_not_reenable_disabled_paths() {
        let path = LineString::from(vec![(10.0, 10.0), (30.0, 10.0), (30.0, 30.0), (10.0, 10.0)]);
        let canvas = canvas();
        let material = MaterialProfile {
            name: "test".into(),
            blade_pressure: 42,
            perf_pressure: 53,
            passes: 1,
            speed: 5,
        };
        let text = String::from_utf8(encode_plt(
            &[path],
            &[CutMode::Disabled],
            unscaled_test_mapping(&canvas),
            &canvas,
            &material,
            false,
            5.0,
            2.0,
            true,
            &[],
            OvercutSettings::default(),
        ))
        .unwrap();
        assert_eq!(text, "IN VER0.1.0 U6476,0  @ ");
    }

    #[test]
    fn peel_tab_position_reaches_plt_and_kiss_tab_precedes_perforation() {
        let path = LineString::from(vec![
            (100.0, 100.0),
            (300.0, 100.0),
            (300.0, 300.0),
            (100.0, 300.0),
            (100.0, 100.0),
        ]);
        let canvas = canvas();
        let material = MaterialProfile {
            name: "test".into(),
            blade_pressure: 42,
            perf_pressure: 53,
            passes: 1,
            speed: 5,
        };
        let encode = |position| {
            String::from_utf8(encode_plt(
                std::slice::from_ref(&path),
                &[CutMode::Perforation],
                unscaled_test_mapping(&canvas),
                &canvas,
                &material,
                false,
                25.0,
                5.0,
                true,
                &[Some(position)],
                OvercutSettings::default(),
            ))
            .unwrap()
        };
        let top = encode(0.125);
        let bottom = encode(0.625);
        assert_ne!(top, bottom);
        assert!(top.find("KP42").unwrap() < top.find("KP53").unwrap());
    }

    #[test]
    fn global_perforation_does_not_reenable_a_disabled_path() {
        let path = LineString::from(vec![(10.0, 10.0), (30.0, 10.0)]);
        let canvas = canvas();
        let material = MaterialProfile {
            name: "test".into(),
            blade_pressure: 42,
            perf_pressure: 53,
            passes: 1,
            speed: 5,
        };
        let text = String::from_utf8(encode_plt(
            &[path],
            &[CutMode::Disabled],
            unscaled_test_mapping(&canvas),
            &canvas,
            &material,
            true,
            5.0,
            2.0,
            false,
            &[],
            OvercutSettings::default(),
        ))
        .unwrap();
        assert_eq!(text, "IN VER0.1.0 U6476,0  @ ");
    }

    #[test]
    fn plt_matches_honeymaro_square_fixture_byte_for_byte() {
        let path = LineString::from(vec![
            (1016.0, 5077.0),
            (1016.0, 2029.0),
            (3048.0, 2029.0),
            (3048.0, 5077.0),
            (1016.0, 5077.0),
        ]);
        let mut fixture_canvas = canvas();
        fixture_canvas.size.y = 7106.0;
        let material = MaterialProfile {
            name: "fixture".into(),
            blade_pressure: 1,
            perf_pressure: 1,
            passes: 1,
            speed: 1,
        };
        let actual = encode_plt(
            &[path],
            &[CutMode::Kiss],
            unscaled_test_mapping(&fixture_canvas),
            &fixture_canvas,
            &material,
            false,
            5.0,
            2.0,
            false,
            &[],
            OvercutSettings {
                enabled: false,
                ..OvercutSettings::default()
            },
        );
        assert_eq!(
            actual.as_slice(),
            include_bytes!("../tests/fixtures/honeymaro-pixcut/square-exact.plt")
        );
    }

    #[test]
    fn stock_mapping_preserves_legacy_f32_quantization_for_subpixel_paths() {
        let canvas = canvas();
        let mapping = stock_plotter_mapping(0, &canvas);
        let calibration = DEVICES[0].cutter_calibration.clone().unwrap();
        for point in [
            Coord { x: 0.0, y: 0.0 },
            Coord {
                x: 669.729_2,
                y: 1_011.375_4,
            },
            Coord {
                x: 1_199.999_9,
                y: 2_099.5,
            },
        ] {
            let expected = [
                f64::from(
                    (canvas.size.y - point.y + calibration.offset.y) * calibration.scale_factor,
                ),
                f64::from((point.x + calibration.offset.x) * calibration.scale_factor),
            ];
            let actual = mapping.apply(point);
            assert_eq!(actual, expected);
            assert_eq!(
                [format!("{:.0}", actual[0]), format!("{:.0}", actual[1])],
                [format!("{:.0}", expected[0]), format!("{:.0}", expected[1])]
            );
        }
    }

    #[test]
    fn print_jpeg_is_decodable_and_respects_the_device_size_target() {
        let image = image::RgbaImage::from_fn(1024, 1024, |x, y| {
            let noise = x
                .wrapping_mul(1_664_525)
                .wrapping_add(y.wrapping_mul(1_013_904_223));
            image::Rgba([
                (noise & 255) as u8,
                ((noise >> 8) & 255) as u8,
                ((noise >> 16) & 255) as u8,
                255,
            ])
        });
        let encoded = encode_image(&image::DynamicImage::ImageRgba8(image));
        assert!(encoded.len() <= 1024 * 1024);
        let decoded = image::load_from_memory(&encoded).unwrap();
        assert_eq!(decoded.dimensions(), (1024, 1024));
    }

    #[test]
    fn direct_overlay_matches_the_previous_clipped_rendering() {
        let source = image::RgbaImage::from_fn(7, 5, |x, y| {
            image::Rgba([(x * 31) as u8, (y * 47) as u8, 180, 80 + (x * y) as u8])
        });
        for offset in [
            Pos2::new(-3.0, -2.0),
            Pos2::new(2.0, 3.0),
            Pos2::new(9.0, 7.0),
        ] {
            let layer = RenderLayer {
                image: source.clone(),
                size: Vec2::new(7.0, 5.0),
                rotation_degrees: 0.0,
                visual_offset: offset,
            };
            let mut actual = image::RgbaImage::from_pixel(10, 8, image::Rgba([7, 9, 11, 255]));
            composite_layer(&mut actual, &layer);

            let mut expected = image::RgbaImage::from_pixel(10, 8, image::Rgba([7, 9, 11, 255]));
            let offset_x = offset.x as i32;
            let offset_y = offset.y as i32;
            let start_x = -offset_x.min(0);
            let start_y = -offset_y.min(0);
            let end_x = offset_x.max(0);
            let end_y = offset_y.max(0);
            let width = (7 - start_x).min(10 - end_x);
            let height = (5 - start_y).min(8 - end_y);
            if width > 0 && height > 0 {
                let clipped = source
                    .view(start_x as u32, start_y as u32, width as u32, height as u32)
                    .to_image();
                image::imageops::overlay(
                    &mut expected,
                    &clipped,
                    i64::from(end_x),
                    i64::from(end_y),
                );
            }
            assert_eq!(actual, expected);
        }
    }

    #[test]
    fn routed_printers_resolve_isolated_active_calibration_profiles() {
        use crate::calibration::{
            CALIBRATION_SCHEMA_VERSION, CalibrationCutMode, CalibrationMethod, CalibrationModel,
            CalibrationProfile, CutPathDirection, CutSettingsProvenance, ErrorMetrics,
            ValidationMetrics,
        };

        let mut canvas = canvas();
        canvas.media_size = 5013;
        canvas.media_type = 2030;
        let stock = stock_plotter_mapping(0, &canvas);
        let before_metrics = ErrorMetrics {
            sample_count: 5,
            rms_mm: 0.8,
            p95_mm: 0.9,
            maximum_mm: 1.0,
            mean_xy_mm: [0.0, 0.0],
        };
        let after_metrics = ErrorMetrics {
            sample_count: 5,
            rms_mm: 0.2,
            p95_mm: 0.3,
            maximum_mm: 0.35,
            mean_xy_mm: [0.0, 0.0],
        };
        let settings = CutSettingsProvenance {
            mode: CalibrationCutMode::Kiss,
            pressure: 30,
            passes: 1,
            configured_speed: Some(5),
            path_direction: CutPathDirection::Mixed,
            path_order_id: "manual-v1".into(),
        };
        let make_key = |serial_number: &str| PrinterCalibrationKey {
            identity: StablePrinterIdentity::SerialNumber {
                serial_number: serial_number.into(),
            },
            model: "DHP700".into(),
            firmware_revision: "1.0".into(),
            media_size: canvas.media_size,
            media_type: canvas.media_type,
        };
        let mut store = CalibrationStore::default();
        for (id, serial, x_shift) in [("profile-a", "A", 7.0), ("profile-b", "B", -11.0)] {
            let mut mapping = stock.direct;
            mapping.translation[0] += x_shift;
            store.profiles.push(CalibrationProfile {
                version: CALIBRATION_SCHEMA_VERSION,
                profile_id: id.into(),
                key: make_key(serial),
                method: CalibrationMethod::ManualEastBay,
                canvas_to_plotter: mapping,
                baseline_mapping_id: "pixcut-s1-stock-v1".into(),
                created_at: 1,
                validation: ValidationMetrics {
                    before: before_metrics.clone(),
                    after: after_metrics.clone(),
                    required_coverage_passed: true,
                    maximum_error_passed: true,
                    normal_kiss_cut_passed: Some(true),
                },
                measurement_settings: settings.clone(),
                validation_settings: settings.clone(),
                selected_model: CalibrationModel::Translation,
                previous_profile_id: None,
            });
            store.activate(id).unwrap();
        }
        let identities = BTreeMap::from([
            (
                "route-a".into(),
                PrinterIdentityInfo {
                    model: "DHP700".into(),
                    serial_number: Some("A".into()),
                    firmware_revision: "1.0".into(),
                },
            ),
            (
                "route-b".into(),
                PrinterIdentityInfo {
                    model: "DHP700".into(),
                    serial_number: Some("B".into()),
                    firmware_revision: "1.0".into(),
                },
            ),
        ]);

        let mut fallback_profile = store.profiles[0].clone();
        fallback_profile.profile_id = "named-profile".into();
        fallback_profile.key.identity = StablePrinterIdentity::NamedFallback {
            profile_name: "Bench cutter".into(),
        };
        fallback_profile.canvas_to_plotter.translation[1] += 13.0;
        store.profiles.push(fallback_profile);
        store.activate("named-profile").unwrap();
        let mut identities = identities;
        identities.insert(
            "route-fallback".into(),
            PrinterIdentityInfo {
                model: "DHP700".into(),
                serial_number: None,
                firmware_revision: "1.0".into(),
            },
        );
        let fallback_names = BTreeMap::from([("route-fallback".into(), "Bench cutter".into())]);
        let a = resolve_routed_canvas_to_plotter(
            &store,
            &identities,
            &fallback_names,
            "route-a",
            0,
            &canvas,
        );
        let b = resolve_routed_canvas_to_plotter(
            &store,
            &identities,
            &fallback_names,
            "route-b",
            0,
            &canvas,
        );
        assert_eq!(a.direct.translation[0], stock.direct.translation[0] + 7.0);
        assert_eq!(b.direct.translation[0], stock.direct.translation[0] - 11.0);
        assert_ne!(
            a.direct.apply([500.0, 800.0]),
            b.direct.apply([500.0, 800.0])
        );
        assert_eq!(
            resolve_routed_canvas_to_plotter(
                &store,
                &identities,
                &fallback_names,
                "route-fallback",
                0,
                &canvas,
            )
            .direct
            .translation[1],
            store
                .profiles
                .iter()
                .find(|profile| profile.profile_id == "named-profile")
                .unwrap()
                .canvas_to_plotter
                .translation[1]
        );
        assert_eq!(
            unique_named_fallback_for_identity(&store, identities.get("route-fallback").unwrap(),)
                .as_deref(),
            Some("Bench cutter")
        );

        let mut stale = identities;
        stale.get_mut("route-a").unwrap().firmware_revision = "2.0".into();
        assert_eq!(
            resolve_routed_canvas_to_plotter(
                &store,
                &stale,
                &fallback_names,
                "route-a",
                0,
                &canvas,
            ),
            stock
        );
    }

    #[test]
    fn named_printer_fallbacks_are_bounded_and_persistence_safe() {
        let mut names = BTreeMap::from([
            (" printer-a ".into(), " Bench cutter ".into()),
            ("empty".into(), "   ".into()),
        ]);
        for index in 0..40 {
            names.insert(format!("printer-{index:02}"), "x".repeat(200));
        }
        let sanitized = sanitize_printer_fallback_names(names);
        assert_eq!(sanitized.len(), 32);
        assert_eq!(
            sanitized.get("printer-a").map(String::as_str),
            Some("Bench cutter")
        );
        assert!(sanitized.values().all(|value| value.chars().count() <= 128));
        assert!(!sanitized.contains_key("empty"));
    }

    #[test]
    fn flatbed_calibration_job_preserves_explicit_bridges_and_phase_order() {
        let material = MaterialProfile {
            name: "test".into(),
            blade_pressure: 40,
            perf_pressure: 88,
            passes: 1,
            speed: 5,
        };
        let job = build_calibration_print_job(
            ManifestIdentity::stock("job-test"),
            CalibrationMethod::FlatbedScanner,
            CalibrationJobSlot::Primary,
            material,
            None,
        )
        .unwrap();
        assert_eq!(
            (job.device_index, job.mode_index, job.canvas_index),
            (0, 1, 0)
        );
        assert!(!job.encoded_image.is_empty());
        let phases = job.calibration_phases.as_ref().unwrap();
        assert_eq!(phases.len(), 1);
        assert_eq!(phases[0].mode, CutMode::Perforation);
        assert_eq!(phases[0].pressure, 88);
        // Twelve measured apertures plus the backing-control aperture are
        // represented as four explicit arcs each; no perf dashes are added.
        assert_eq!(phases[0].paths.len(), 52);
        let plt = encode_calibration_plt(
            phases,
            stock_plotter_mapping(0, &DEVICES[0].modes[1].canvas_sizes[0]),
            1,
        );
        assert_eq!(plt.windows(2).filter(|window| *window == b" U").count(), 53);
        assert!(plt.windows(5).any(|window| window == b" KP88"));
        let spec = calibration_job_spec("Calibration sheet", "printer-b".into());
        assert_eq!(spec.eligible_printers.len(), 1);
        assert!(spec.eligible_printers.contains("printer-b"));
        assert_eq!(
            spec.required_capabilities,
            BTreeSet::from(["cut".into(), "print".into()])
        );
    }

    #[test]
    fn validation_job_uses_candidate_override_without_activating_store() {
        let override_mapping = CanvasToPlotter::legacy_pixcut_s1(2100.0).compose(CanvasToPlotter {
            matrix: [[1.0, 0.0], [0.0, 1.0]],
            translation: [2.0, -3.0],
        });
        let job = build_calibration_print_job(
            ManifestIdentity::stock("validation-test"),
            CalibrationMethod::ManualEastBay,
            CalibrationJobSlot::Validation,
            MaterialProfile::built_ins().remove(0),
            Some(override_mapping),
        )
        .unwrap();
        assert_eq!(job.mapping_override, Some(override_mapping));
        assert!(
            job.calibration_phases
                .as_ref()
                .unwrap()
                .iter()
                .all(|phase| phase.mode == CutMode::Kiss)
        );
    }

    #[test]
    fn dispatched_command_evidence_uses_final_mapping_and_integer_quantization() {
        let material = MaterialProfile::built_ins().remove(0);
        let job = build_calibration_print_job(
            ManifestIdentity::stock("command-evidence"),
            CalibrationMethod::ManualEastBay,
            CalibrationJobSlot::Validation,
            material.clone(),
            None,
        )
        .unwrap();
        let phases = job.calibration_phases.as_ref().unwrap();
        let baseline = PlotterMapping::direct(CanvasToPlotter {
            matrix: [[2.37, 0.04], [-0.03, 2.41]],
            translation: [19.4, -7.6],
        });
        let candidate = PlotterMapping::direct(CanvasToPlotter {
            matrix: [[2.39, 0.02], [-0.01, 2.38]],
            translation: [31.2, 6.4],
        });
        let baseline_plt = encode_calibration_plt(phases, baseline, material.passes);
        let candidate_plt = encode_calibration_plt(phases, candidate, material.passes);
        let baseline_commands = calibration_plotter_commands_from_plt(&baseline_plt);
        let candidate_commands = calibration_plotter_commands_from_plt(&candidate_plt);
        assert!(!baseline_commands.is_empty());
        assert_eq!(baseline_commands.len(), candidate_commands.len());
        assert_ne!(baseline_commands, candidate_commands);
        assert!(
            baseline_commands
                .iter()
                .all(|command| { command.plotter_units != [6476, 0] })
        );

        let first_canvas_point = phases[0].paths[0].0[0];
        let first_mapped = baseline.apply(first_canvas_point);
        let expected = [
            format!("{:.0}", first_mapped[0]).parse::<i64>().unwrap(),
            format!("{:.0}", first_mapped[1]).parse::<i64>().unwrap(),
        ];
        assert_eq!(
            baseline_commands[0].kind,
            CalibrationPlotterCommandKind::Move
        );
        assert_eq!(baseline_commands[0].plotter_units, expected);
        assert_eq!(
            baseline_commands[1].kind,
            CalibrationPlotterCommandKind::Draw
        );
        assert_eq!(baseline_commands[1].plotter_units, expected);
    }

    #[test]
    fn validation_coverage_requires_opposite_sheet_regions() {
        let make = |id: &str, x: f64, y: f64| CalibrationObservation {
            target_id: id.into(),
            sheet_id: "validation".into(),
            nominal_print_mm: [x, y],
            observed_cut_mm: [x, y],
            uncertainty_mm: [0.1, 0.1],
            confidence: 1.0,
            included: true,
        };
        let covered = vec![
            make("1", 10.0, 10.0),
            make("2", 90.0, 10.0),
            make("3", 10.0, 160.0),
            make("4", 90.0, 160.0),
        ];
        assert!(validation_coverage_passed(
            CalibrationMethod::ManualEastBay,
            &covered
        ));
        let clustered = (0..6)
            .map(|index| make(&index.to_string(), 10.0 + f64::from(index), 10.0))
            .collect::<Vec<_>>();
        assert!(!validation_coverage_passed(
            CalibrationMethod::FlatbedScanner,
            &clustered
        ));
    }

    #[test]
    fn completed_calibration_run_persists_evidence_and_rejects_stale_async_results() {
        use crate::calibration::{ErrorMetrics, ManualEdgeDraft, ManualSheetSlot};

        let mut wizard = CalibrationWizard::new("persisted-run", 10).unwrap();
        wizard
            .select_method(CalibrationMethod::ManualEastBay, 11)
            .unwrap();
        wizard
            .set_print_scale(ManualSheetSlot::Primary, None, 12)
            .unwrap();
        for id in ["C1", "C2", "C6", "C7"] {
            wizard
                .set_manual_target(
                    ManualSheetSlot::Primary,
                    id,
                    ManualEdgeDraft {
                        left_mm: Some(6.5),
                        right_mm: Some(7.5),
                        top_mm: Some(6.75),
                        bottom_mm: Some(7.25),
                    },
                    false,
                    13,
                )
                .unwrap();
        }
        let solution = solve_calibration(
            CalibrationMethod::ManualEastBay,
            &wizard.training_observations(),
            CalibrationPolicy::pixcut_s1_4x7(),
        )
        .unwrap();
        let metrics = ValidationMetrics {
            before: ErrorMetrics {
                sample_count: 4,
                rms_mm: 1.0,
                p95_mm: 1.0,
                maximum_mm: 1.0,
                mean_xy_mm: [0.5, 0.25],
            },
            after: ErrorMetrics {
                sample_count: 4,
                rms_mm: 0.2,
                p95_mm: 0.25,
                maximum_mm: 0.3,
                mean_xy_mm: [0.05, 0.02],
            },
            required_coverage_passed: true,
            maximum_error_passed: true,
            normal_kiss_cut_passed: None,
        };
        let baseline_mapping = CanvasToPlotter::legacy_pixcut_s1(2100.0).compose(CanvasToPlotter {
            matrix: [[1.0, 0.0], [0.0, 1.0]],
            translation: [23.0, -17.0],
        });
        let candidate_mapping = baseline_mapping.compose(CanvasToPlotter {
            matrix: [[1.0, 0.0], [0.0, 1.0]],
            translation: [9.0, 11.0],
        });
        let session = CalibrationSession {
            printer_id: "printer-a".into(),
            printer_key: PrinterCalibrationKey {
                identity: StablePrinterIdentity::SerialNumber {
                    serial_number: "SERIAL-A".into(),
                },
                model: "DHP700".into(),
                firmware_revision: "1.0".into(),
                media_size: 5013,
                media_type: 2030,
            },
            wizard,
            baseline_profile_id: Some("active-baseline-profile".into()),
            baseline_profile_version: 7,
            baseline_mapping,
            material: MaterialProfile::built_ins().remove(0),
            candidate: Some(solution.clone()),
            candidate_mapping: Some(candidate_mapping),
            validation_metrics: Some(metrics.clone()),
            training_scan_report: None,
            validation_scan_report: None,
            training_scan_preview_png: None,
            validation_scan_preview_png: None,
            training_scan_preview_sha1: None,
            validation_scan_preview_sha1: None,
            primary_queue_job: Some(7),
            second_queue_job: None,
            validation_queue_job: Some(8),
            historical_queue_job_ids: [Some(7), None, Some(8)],
            image_sha1: [Some("a".repeat(40)), None, Some("b".repeat(40))],
            plotter_sha1: [Some("c".repeat(40)), None, Some("d".repeat(40))],
            plotter_commands: [
                vec![CalibrationPlotterCommand {
                    kind: CalibrationPlotterCommandKind::Move,
                    plotter_units: [101, 202],
                }],
                vec![],
                vec![CalibrationPlotterCommand {
                    kind: CalibrationPlotterCommandKind::Move,
                    plotter_units: [303, 404],
                }],
            ],
            validation_generation: 0,
            device_job_ids: vec![70, 80],
            validation_device_job_ids: vec![80],
            device_job_ids_by_slot: [vec![70], vec![], vec![80]],
            physical_sheet_attempts: [0; 3],
            scan_request_generations: [0; 2],
        };
        let active_manifest = calibration_manifest(
            session.manifest_identity_for_slot(CalibrationJobSlot::Primary),
            CalibrationMethod::ManualEastBay,
            false,
        )
        .unwrap();
        assert_eq!(
            active_manifest.identity.baseline_mapping_id,
            "active-baseline-profile"
        );
        assert_eq!(active_manifest.identity.profile_version, 7);
        let stock_manifest = manual_calibration(ManifestIdentity::stock("persisted-run")).unwrap();
        let binding_digest = |manifest: &TargetManifest| {
            manifest.diagnostics.iter().find_map(|diagnostic| {
                if let crate::calibration::TargetDiagnostic::RunBinding { digest_hex, .. } =
                    diagnostic
                {
                    Some(digest_hex.clone())
                } else {
                    None
                }
            })
        };
        assert_ne!(
            binding_digest(&active_manifest),
            binding_digest(&stock_manifest)
        );
        let mut replacement_sheet = session.clone();
        replacement_sheet.physical_sheet_attempts
            [CalibrationSession::slot_index(CalibrationJobSlot::Primary)] = 1;
        let replacement_manifest = calibration_manifest(
            replacement_sheet.manifest_identity_for_slot(CalibrationJobSlot::Primary),
            CalibrationMethod::ManualEastBay,
            false,
        )
        .unwrap();
        assert_ne!(
            binding_digest(&active_manifest),
            binding_digest(&replacement_manifest),
            "a reprinted physical sheet must reject scans from the prior attempt"
        );
        let current_generation = session.wizard.validation_generation;
        assert!(session.accepts_job_result(
            "persisted-run",
            current_generation,
            CalibrationJobSlot::Primary,
            0,
        ));
        assert!(!replacement_sheet.accepts_job_result(
            "persisted-run",
            current_generation,
            CalibrationJobSlot::Primary,
            0,
        ));
        assert!(session.accepts_scan_result(
            "persisted-run",
            current_generation,
            ScanSlot::Training,
            0,
            0,
        ));
        let mut newer_scan_request = session.clone();
        newer_scan_request.scan_request_generations[0] = 1;
        assert!(!newer_scan_request.accepts_scan_result(
            "persisted-run",
            current_generation,
            ScanSlot::Training,
            0,
            0,
        ));
        let mut run = persisted_calibration_run(&session, &solution, metrics).unwrap();
        run.validate_and_sanitize().unwrap();
        assert_eq!(run.queue_job_ids, [7, 8]);
        assert_eq!(run.device_job_ids, [70, 80]);
        assert_eq!(run.baseline_profile_version, 7);
        assert_eq!(
            run.validation_generation,
            session.wizard.validation_generation
        );
        assert_eq!(run.payload_hashes.len(), 2);
        assert_eq!(run.payload_hashes[1].slot, "validation");
        assert!(!run.payload_hashes[0].plotter_commands.is_empty());
        assert!(!run.payload_hashes[1].plotter_commands.is_empty());
        assert_ne!(
            run.payload_hashes[0].plotter_commands[0].plotter_units,
            run.payload_hashes[1].plotter_commands[0].plotter_units
        );
        assert_eq!(
            run.payload_hashes[0].plotter_commands[0].plotter_units,
            [101, 202]
        );
        assert_eq!(
            run.payload_hashes[1].plotter_commands[0].plotter_units,
            [303, 404]
        );
        assert_eq!(run.manifest.target_ids.len(), 7);
        assert_eq!(
            run.manifest.jpeg_sha1.as_deref(),
            Some("a".repeat(40).as_str())
        );
        assert_eq!(
            run.manifest.plt_sha1.as_deref(),
            Some("c".repeat(40).as_str())
        );

        let mut saved_session = session.clone();
        saved_session.material.blade_pressure = 30;
        saved_session.material.perf_pressure = 80;
        saved_session.material.passes = 1;
        let resumed = saved_session.sanitize_after_load().unwrap();
        assert!(resumed.primary_queue_job.is_none());
        assert!(resumed.validation_queue_job.is_none());
        assert_eq!(resumed.historical_queue_job_ids, [Some(7), None, Some(8)]);

        let mut missing_validation_evidence = session.clone();
        missing_validation_evidence.image_sha1[2] = None;
        assert!(
            persisted_calibration_run(
                &missing_validation_evidence,
                &solution,
                ValidationMetrics {
                    before: run.validation.as_ref().unwrap().before.clone(),
                    after: run.validation.as_ref().unwrap().after.clone(),
                    required_coverage_passed: true,
                    maximum_error_passed: true,
                    normal_kiss_cut_passed: None,
                },
            )
            .unwrap_err()
            .to_string()
            .contains("validation payload hashes")
        );

        let mut stale = session.clone();
        stale.validation_generation = stale.wizard.validation_generation;
        let delayed_generation = stale.wizard.validation_generation;
        let old_validation_identity = stale.validation_manifest_identity();
        stale
            .wizard
            .set_manual_target(
                crate::calibration::ManualSheetSlot::Primary,
                "C1",
                crate::calibration::ManualEdgeDraft {
                    left_mm: Some(6.4),
                    right_mm: Some(7.6),
                    top_mm: Some(6.7),
                    bottom_mm: Some(7.3),
                },
                false,
                100,
            )
            .unwrap();
        assert!(!stale.accepts_async_result("persisted-run", delayed_generation));
        assert!(stale.accepts_async_result("persisted-run", stale.wizard.validation_generation));
        assert_eq!(stale.clear_stale_candidate_evidence(), Some(8));
        assert!(stale.candidate.is_none());
        assert!(stale.candidate_mapping.is_none());
        assert!(stale.validation_metrics.is_none());
        assert!(stale.validation_queue_job.is_none());
        assert!(stale.image_sha1[2].is_none());
        assert!(stale.plotter_sha1[2].is_none());
        assert!(stale.plotter_commands[2].is_empty());
        assert_eq!(stale.device_job_ids, [70]);
        assert!(stale.validation_device_job_ids.is_empty());
        assert_eq!(stale.device_job_ids_by_slot[0], [70]);
        assert!(stale.device_job_ids_by_slot[2].is_empty());
        assert_ne!(
            old_validation_identity.candidate_generation,
            stale.validation_manifest_identity().candidate_generation
        );
    }

    #[test]
    #[ignore = "performance probe; run with --release -- --ignored --nocapture"]
    fn perf_sample_print_encoding() {
        use std::{hint::black_box, time::Instant};

        let sample = image::RgbaImage::from_fn(1600, 1600, |x, y| {
            let noise = x
                .wrapping_mul(1_664_525)
                .wrapping_add(y.wrapping_mul(1_013_904_223));
            image::Rgba([
                (noise & 255) as u8,
                ((noise >> 8) & 255) as u8,
                ((noise >> 16) & 255) as u8,
                255,
            ])
        });
        let started = Instant::now();
        let encoded = encode_image(black_box(&image::DynamicImage::ImageRgba8(sample)));
        println!(
            "print JPEG 1600x1600: {:?} ({} bytes)",
            started.elapsed(),
            encoded.len()
        );
        black_box(encoded);
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn scan_library_page(
    folders: &[String],
    page: usize,
    page_size: usize,
) -> (Vec<std::path::PathBuf>, bool) {
    let start = page.saturating_mul(page_size);
    let mut seen = 0usize;
    let mut paths = Vec::with_capacity(page_size);
    let mut has_more = false;
    for folder in folders {
        if !scan_library_directory(
            std::path::Path::new(folder),
            start,
            page_size,
            &mut seen,
            &mut paths,
            &mut has_more,
        ) {
            break;
        }
    }
    (paths, has_more)
}

#[cfg(not(target_arch = "wasm32"))]
fn collect_library_paths(folders: &[String]) -> Vec<std::path::PathBuf> {
    let mut paths = Vec::new();
    for folder in folders {
        collect_library_directory(std::path::Path::new(folder), &mut paths);
    }
    paths
}

#[cfg(not(target_arch = "wasm32"))]
fn collect_library_directory(root: &std::path::Path, paths: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    let mut entries = entries.flatten().collect::<Vec<_>>();
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            collect_library_directory(&path, paths);
        } else if is_library_image_path(&path) {
            paths.push(path);
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn is_library_image_path(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "png" | "jpg" | "jpeg"
            )
        })
}

#[cfg(not(target_arch = "wasm32"))]
fn scan_library_directory(
    root: &std::path::Path,
    start: usize,
    page_size: usize,
    seen: &mut usize,
    paths: &mut Vec<std::path::PathBuf>,
    has_more: &mut bool,
) -> bool {
    let Ok(entries) = std::fs::read_dir(root) else {
        return true;
    };
    let mut entries = entries.flatten().collect::<Vec<_>>();
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        // Never follow directory links: a junction/symlink can point back to
        // an ancestor and otherwise recurse until stack exhaustion.
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            if !scan_library_directory(&path, start, page_size, seen, paths, has_more) {
                return false;
            }
        } else if is_library_image_path(&path) {
            if *seen >= start {
                if paths.len() == page_size {
                    *has_more = true;
                    return false;
                }
                paths.push(path);
            }
            *seen += 1;
        }
    }
    true
}

#[cfg(not(target_arch = "wasm32"))]
fn current_timestamp_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}
