use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet, VecDeque, hash_map::DefaultHasher},
    hash::{Hash, Hasher},
    io::Write,
    sync::mpsc,
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
    cut::{CutAction, CutGenerator, CutImage, CutTuning, OvercutSettings, apply_overcut},
    export::{ToolpathStats, cut_svg, jpeg_pdf, toolpath_debug_svg},
    jobs::{JobQueue, JobSpec, JobStatus as QueueJobStatus, Printer as QueuePrinter},
    path_edit::{smooth_path, union_paths},
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

const MATERIAL_PROFILES_STORAGE_KEY: &str = "sapodilla.material-profiles.v1";
const LIBRARY_FOLDERS_STORAGE_KEY: &str = "sapodilla.library-folders.v1";
const LIBRARY_CYCLE_STORAGE_KEY: &str = "sapodilla.library-cycle.v1";
const LIBRARY_CONSUMED_STORAGE_KEY: &str = "sapodilla.library-consumed-ahead.v1";
const CANVAS_VIEW_STORAGE_KEY: &str = "sapodilla.canvas-view.v1";
const APPEARANCE_STORAGE_KEY: &str = "sapodilla.appearance.v1";
const APPEARANCE_VERSION: u8 = 1;
#[cfg(any(not(target_arch = "wasm32"), test))]
const LIBRARY_PAGE_SIZE: usize = 100;
const MAX_LIBRARY_FILL_ATTEMPTS: usize = 512;
const NEW_ARTWORK_MIN_FRACTION: f32 = 0.22;
const NEW_ARTWORK_MAX_FRACTION: f32 = 0.72;

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
    pub library: Vec<LoadedImage>,
    pub library_folders: Vec<String>,
    pub library_disk_paths: Vec<std::path::PathBuf>,
    pub library_page: usize,
    pub library_has_more: bool,
    pub selected_images: Vec<usize>,
    pub(crate) canvas_transform_gesture: Option<views::TransformGesture>,
    pub background_color: [u8; 3],
    pub material_profiles: Vec<MaterialProfile>,
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

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
struct CanvasViewPreferences {
    show_grid: bool,
    show_rulers: bool,
    grid_spacing_mm: f32,
}

impl Default for CanvasViewPreferences {
    fn default() -> Self {
        Self {
            show_grid: true,
            show_rulers: true,
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
    encoded_image_len: usize,
    plt: Vec<u8>,
    packet_data: Vec<u8>,
    image_hash: String,
    created_at: u64,
    copies: usize,
    device_index: usize,
    mode_index: usize,
    canvas_index: usize,
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
                view.grid_spacing_mm = view.grid_spacing_mm.clamp(0.5, 100.0);
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
            library,
            library_folders,
            library_disk_paths,
            library_page: 0,
            library_has_more,
            selected_images: Default::default(),
            canvas_transform_gesture: None,
            background_color: [255, 255, 255],
            material_profiles,
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
            prepared.extend(
                self.cut_shapes
                    .iter()
                    .zip(&modes)
                    .filter(|(_, mode)| **mode != CutMode::Disabled)
                    .filter_map(|(path, _)| peel_tab_unmirrored(path)),
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
            DEVICES[self.selected_device]
                .cutter_calibration
                .clone()
                .unwrap_or_default(),
            canvas_size,
            &self.material_profiles[self.selected_material],
            self.perf_cut,
            self.perf_dash_mm * DEVICES[self.selected_device].dpi / 25.4,
            self.perf_gap_mm * DEVICES[self.selected_device].dpi / 25.4,
            self.peel_tabs,
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
                        self.printer_connections.insert(printer_id, manager);
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
                            let _ = self.job_queue.set_printer_online(&printer_id);
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
                            self.cut_shapes.extend(self.manual_cut_shapes.clone());
                            self.cutline_owners.extend(manual_owners);
                            self.cutline_locked.extend(manual_locks);
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
            }
        }
        self.dispatch_queued_jobs();
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
        let mode = &DEVICES[self.selected_device].modes[self.selected_mode];
        let canvas_size = mode.canvas_sizes[self.selected_canvas_size].clone();
        let snapshot = self.render_snapshot();
        let cut_shapes = self.cut_shapes.clone();
        let cut_modes = self.cut_modes.clone();
        let calibration = DEVICES[self.selected_device]
            .cutter_calibration
            .clone()
            .unwrap_or_default();
        let material = self.material_profiles[self.selected_material].clone();
        let perf_cut = self.perf_cut;
        let dpi = DEVICES[self.selected_device].dpi;
        let perf_dash = self.perf_dash_mm * dpi / 25.4;
        let perf_gap = self.perf_gap_mm * dpi / 25.4;
        let peel_tabs = self.peel_tabs;
        let overcut = self.overcut;
        let copies = self.copies;
        let device_index = self.selected_device;
        let mode_index = self.selected_mode;
        let canvas_index = self.selected_canvas_size;
        let tx = self.tx.clone();
        spawn_blocking(move || {
            let image = encode_image(&snapshot.render());
            let plt = encode_plt(
                &cut_shapes,
                &cut_modes,
                calibration,
                &canvas_size,
                &material,
                perf_cut,
                perf_dash,
                perf_gap,
                peel_tabs,
                overcut,
            );
            let mut packet_data = Vec::with_capacity(image.len() + plt.len());
            if mode_type.has_cutting() {
                packet_data.extend_from_slice(&plt);
            }
            packet_data.extend_from_slice(&image);
            let job = PendingPrintJob {
                encoded_image_len: image.len(),
                plt,
                packet_data,
                image_hash: hex::encode(sha1::Sha1::digest(&image)),
                created_at: current_timestamp_millis(),
                copies,
                device_index,
                mode_index,
                canvas_index,
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
            let Some(manager) = self.printer_connections.get(&route.printer_id).cloned() else {
                let _ = self
                    .job_queue
                    .set_printer_offline(&route.printer_id, "connection unavailable");
                continue;
            };
            self.active_queue_job = Some(route.job_id);
            self.active_queue_jobs
                .insert(route.printer_id.clone(), route.job_id);
            self.send_progress = None;
            let tx = self.tx.clone();
            let queue_id = route.job_id;
            spawn(async move {
                let result = async {
                    let mode = &DEVICES[payload.device_index].modes[payload.mode_index];
                    let canvas_size = &mode.canvas_sizes[payload.canvas_index];
                    let id = manager.next_message_id();
                    let data = if mode.mode_type.has_cutting() {
                        serde_json::json!({
                            "id": id, "method": "combo-job", "params": [
                                { "method": "print-job", "params": {
                                    "media-size": canvas_size.media_size, "media-type": canvas_size.media_type,
                                    "job-type": mode.mode_type.job_type(), "channel": mode.mode_type.channel(),
                                    "file-size": payload.encoded_image_len, "document-format": 9,
                                    "document-name": format!("{}.jpeg", payload.created_at),
                                    "hash-method": 1, "hash-value": payload.image_hash,
                                    "user-account": "000000.00000000000000000000000000000000.0000",
                                    "job-send-time": payload.created_at / 1000,
                                    "link-type": mode.mode_type.link_type(), "copies": payload.copies
                                }},
                                { "method": "cut-job", "params": {
                                    "copies": payload.copies, "media-size": canvas_size.media_size,
                                    "document-name": format!("{}.plt", payload.created_at),
                                    "file-size": payload.plt.len(), "channel": mode.mode_type.channel(),
                                    "media-type": canvas_size.media_type, "job-type": mode.mode_type.job_type(),
                                    "document-format": 18, "job-send-time": payload.created_at / 1000
                                }}
                            ]
                        })
                    } else {
                        serde_json::json!({ "id": id, "method": "print-job", "params": {
                            "media-size": canvas_size.media_size, "media-type": canvas_size.media_type,
                            "job-type": mode.mode_type.job_type(), "channel": mode.mode_type.channel(),
                            "file-size": payload.encoded_image_len, "document-format": 9,
                            "document-name": format!("{}.jpeg", payload.created_at),
                            "hash-method": 1, "hash-value": payload.image_hash,
                            "user-account": "000000.00000000000000000000000000000000.0000",
                            "link-type": mode.mode_type.link_type(), "job-send-time": payload.created_at / 1000,
                            "copies": payload.copies
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
    }

    fn start_new_sheet(&mut self) {
        self.loaded_images.clear();
        self.document_kind = DocumentKind::Sheet;
        self.template_placeholders.clear();
        self.cutline_owners.clear();
        self.cutline_locked.clear();
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
                            let _ = theme::toolbar_toggle(ui, accent, &mut selected, "Selected");
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
            if ui.small_button("Left").clicked() {
                for &index in &self.selected_images {
                    align_image_to_sheet(
                        &mut self.loaded_images[index],
                        canvas,
                        SheetAlignment::Left,
                    );
                }
            }
            if ui.small_button("Center X").clicked() {
                for &index in &self.selected_images {
                    align_image_to_sheet(
                        &mut self.loaded_images[index],
                        canvas,
                        SheetAlignment::CenterX,
                    );
                }
            }
            if ui.small_button("Right").clicked() {
                for &index in &self.selected_images {
                    align_image_to_sheet(
                        &mut self.loaded_images[index],
                        canvas,
                        SheetAlignment::Right,
                    );
                }
            }
            if ui.small_button("Top").clicked() {
                for &index in &self.selected_images {
                    align_image_to_sheet(
                        &mut self.loaded_images[index],
                        canvas,
                        SheetAlignment::Top,
                    );
                }
            }
            if ui.small_button("Middle").clicked() {
                for &index in &self.selected_images {
                    align_image_to_sheet(
                        &mut self.loaded_images[index],
                        canvas,
                        SheetAlignment::Middle,
                    );
                }
            }
            if ui.small_button("Bottom").clicked() {
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
            ui.horizontal(|ui| {
                duplicate_clicked = ui.button("Duplicate").clicked();
                forward_clicked = ui.button("Bring forward").clicked();
                backward_clicked = ui.button("Send backward").clicked();
            });
            let duplicate = duplicate_clicked.then(|| {
                let mut duplicate = image.clone();
                duplicate.id = format!("image-{}", Uuid::new_v4());
                duplicate.offset += Vec2::splat(20.0);
                duplicate
            });
            if let Some(duplicate) = duplicate {
                self.loaded_images.push(duplicate);
                self.selected_images = vec![self.loaded_images.len() - 1];
            } else if forward_clicked && index + 1 < self.loaded_images.len() {
                self.loaded_images.swap(index, index + 1);
                self.selected_images = vec![index + 1];
            } else if backward_clicked && index > 0 {
                self.loaded_images.swap(index, index - 1);
                self.selected_images = vec![index - 1];
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
        for printer in self.job_queue.printers() {
            ui.horizontal(|ui| {
                ui.label(&printer.name);
                ui.small(format!("{:?}", printer.status));
                if self.printer_connections.contains_key(&printer.id)
                    && ui.small_button("Disconnect").clicked()
                {
                    disconnect = Some(printer.id.clone());
                }
            });
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
    all_perforation: bool,
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
    for mode in cut_modes {
        match mode {
            CutMode::Kiss => 0u8,
            CutMode::Perforation => 1,
            CutMode::Disabled => 2,
        }
        .hash(&mut hasher);
    }
    all_perforation.hash(&mut hasher);
    CutValidationSnapshot {
        geometry_hash: hasher.finish(),
        canvas_size: [canvas_size.x.to_bits(), canvas_size.y.to_bits()],
        safe_area: [safe_area.x.to_bits(), safe_area.y.to_bits()],
    }
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
    let mut has_intersections = false;
    'intersection: for left in 0..effective_paths.len() {
        let Some(left_bounds) = bounds[left] else {
            continue;
        };
        for right in left + 1..effective_paths.len() {
            let Some(right_bounds) = bounds[right] else {
                continue;
            };
            if left_bounds.intersects(&right_bounds)
                && effective_paths[left].intersects(effective_paths[right])
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
        views::synchronize_cut_preview(self);
        let canvas = self.get_canvas();
        let current_validation_snapshot = cut_validation_snapshot(
            &self.cut_shapes,
            &self.cut_modes,
            self.perf_cut,
            canvas.size,
            canvas.safe_area,
        );
        if self.cut_validation_snapshot.as_ref() != Some(&current_validation_snapshot) {
            let cut_validation = validate_current_cut_paths(
                &self.cut_shapes,
                &self.cut_modes,
                self.perf_cut,
                canvas.size,
                canvas.safe_area,
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
                    if theme::primary_button(
                        ui,
                        accent,
                        if compact_layout {
                            "+ Add"
                        } else {
                            "+ Add artwork"
                        },
                    )
                    .on_hover_text("Import artwork (Ctrl/Cmd+Shift+U)")
                    .clicked()
                    {
                        self.upload_image(ctx);
                    }
                    if compact_layout {
                        ui.menu_button("More", |ui| {
                            if ui.button("Auto-pack sheet").clicked() {
                                self.auto_pack();
                                ui.close();
                            }
                            if ui
                                .add(
                                    egui::Button::new("Save")
                                        .shortcut_text(ctx.format_shortcut(&save_shortcut)),
                                )
                                .clicked()
                            {
                                self.save_document(self.document_kind);
                                ui.close();
                            }
                            ui.separator();
                            ui.checkbox(&mut self.snap_to_guides, "Snap to guides");
                            ui.checkbox(&mut self.show_grid, "Show grid");
                            ui.checkbox(&mut self.show_rulers, "Show rulers");
                            ui.checkbox(&mut self.show_cutlines, "Show cut preview");
                            ui.checkbox(&mut self.edit_cutlines, "Edit cut nodes");
                        });
                    } else {
                        if theme::secondary_button(ui, accent, "Auto-pack")
                            .on_hover_text("Arrange artwork to use the sheet efficiently")
                            .clicked()
                        {
                            self.auto_pack();
                        }
                        if theme::secondary_button(ui, accent, "Save")
                            .on_hover_text(format!(
                                "Save this document ({})",
                                ctx.format_shortcut(&save_shortcut)
                            ))
                            .clicked()
                        {
                            self.save_document(self.document_kind);
                        }
                        ui.separator();
                        theme::toolbar_toggle(ui, accent, &mut self.snap_to_guides, "Snap");
                        theme::toolbar_toggle(ui, accent, &mut self.show_grid, "Grid");
                        theme::toolbar_toggle(ui, accent, &mut self.show_rulers, "Rulers");
                        theme::toolbar_toggle(ui, accent, &mut self.show_cutlines, "Cut preview");
                        theme::toolbar_toggle(ui, accent, &mut self.edit_cutlines, "Edit nodes");
                    }

                    if theme::toolbar_toggle(ui, accent, &mut self.show_library_panel, "Library")
                        .clicked()
                        && compact_layout
                        && self.show_library_panel
                    {
                        self.show_inspector_panel = false;
                    }
                    if theme::toolbar_toggle(
                        ui,
                        accent,
                        &mut self.show_inspector_panel,
                        "Inspector",
                    )
                    .clicked()
                        && compact_layout
                        && self.show_inspector_panel
                    {
                        self.show_library_panel = false;
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
                if theme::secondary_button(ui, accent, "Import artwork…").clicked() {
                    self.import_library_images(ctx);
                }
                theme::muted(ui, "Add artwork once, then reuse it across sheets.");
                ui.add_space(6.0);
                #[cfg(not(target_arch = "wasm32"))]
                if ui.button("Import folder…").clicked() {
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
                        if ui.button("Rescan folders").clicked() {
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
                                        if ui.small_button("Remove").clicked() {
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
                        .add_enabled(self.library_page > 0, egui::Button::new("Previous"))
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
                        .add_enabled(self.library_has_more, egui::Button::new("Next"))
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
                    if ui.button("Fill sheet").clicked() {
                        self.add_library_to_sheet(false);
                    }
                    if ui.button("Shuffle fill").clicked() {
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
                                ui.checkbox(&mut self.peel_tabs, "Peel tabs");
                            });
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
                        ui.add(
                            egui::Slider::new(&mut self.grid_spacing_mm, 0.5..=100.0)
                                .logarithmic(true)
                                .suffix(" mm")
                                .text("Grid spacing"),
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
                            let layers_changed = views::loaded_images(
                                ui,
                                DEVICES[self.selected_device].dpi,
                                self.get_canvas().size,
                                &mut self.loaded_images,
                                DEVICES[self.selected_device].modes[self.selected_mode].mode_type,
                            );
                            if layers_changed {
                                self.selected_images.clear();
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
            views::canvas_editor(ui, self);
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
                grid_spacing_mm: self.grid_spacing_mm.clamp(0.5, 100.0),
            },
        );
        eframe::set_value(storage, APPEARANCE_STORAGE_KEY, &self.appearance);
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
fn encode_plt(
    cut_shapes: &[LineString<f32>],
    cut_modes: &[CutMode],
    cutter_calibration: CutterCalibration,
    canvas_size: &CanvasSize,
    material: &MaterialProfile,
    perf_cut_enabled: bool,
    perf_dash: f32,
    perf_gap: f32,
    peel_tabs: bool,
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
        let tabs = cut_shapes
            .iter()
            .zip(&modes)
            .filter(|(_, mode)| **mode != CutMode::Disabled)
            .filter_map(|(path, _)| peel_tab_unmirrored(path))
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
        let mut flipped =
            CutGenerator::mirror_cuts(&phase.paths, canvas_size.size).collect::<Vec<_>>();
        flipped.sort_by(|a, b| {
            let a_start = *a.0.first().unwrap();
            let b_start = *b.0.first().unwrap();
            a_start
                .y
                .total_cmp(&b_start.y)
                .then(a_start.x.total_cmp(&b_start.x))
        });
        for _ in 0..material.passes {
            for line in &flipped {
                write_line_string(&cutter_calibration, &mut buf, line);
            }
        }
    }

    write!(buf, " U6476,0  @ ").unwrap();

    buf
}

pub(crate) fn peel_tab_unmirrored(path: &LineString<f32>) -> Option<LineString<f32>> {
    let bounds = path.bounding_rect()?;
    let width = (bounds.width() * 0.25).clamp(30.0, 120.0);
    let depth = (width * 0.35).clamp(12.0, 42.0);
    let center_x = (bounds.min().x + bounds.max().x) / 2.0;
    let y = bounds.max().y;
    let points = (0..=12)
        .map(|index| {
            let t = index as f32 / 12.0;
            let angle = std::f32::consts::PI * t;
            (center_x - width / 2.0 + width * t, y - depth * angle.sin())
        })
        .collect::<Vec<_>>();
    Some(LineString::from(points))
}

fn write_line_string(
    cutter_calibration: &CutterCalibration,
    buf: &mut Vec<u8>,
    line_shape: &geo::LineString<f32>,
) {
    write!(
        buf,
        " U{:.0},{:.0}",
        (line_shape.0[0].y + cutter_calibration.offset.y) * cutter_calibration.scale_factor,
        (line_shape.0[0].x + cutter_calibration.offset.x) * cutter_calibration.scale_factor
    )
    .unwrap();

    for point in line_shape.coords() {
        write!(
            buf,
            " D{:.0},{:.0}",
            (point.y + cutter_calibration.offset.y) * cutter_calibration.scale_factor,
            (point.x + cutter_calibration.offset.x) * cutter_calibration.scale_factor
        )
        .unwrap();
    }
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
            CutterCalibration::default(),
            &canvas(),
            &material,
            false,
            2.0,
            1.0,
            false,
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
            CutterCalibration::default(),
            &canvas(),
            &material,
            true,
            5.0,
            5.0,
            false,
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
        let text = String::from_utf8(encode_plt(
            &paths,
            &[CutMode::Kiss, CutMode::Perforation, CutMode::Disabled],
            CutterCalibration::default(),
            &canvas(),
            &material,
            false,
            5.0,
            2.0,
            false,
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
            CutterCalibration::default(),
            &canvas(),
            &material,
            false,
            5.0,
            2.0,
            true,
            OvercutSettings::default(),
        ))
        .unwrap();
        assert_eq!(text, "IN VER0.1.0 U6476,0  @ ");
    }

    #[test]
    fn global_perforation_does_not_reenable_a_disabled_path() {
        let path = LineString::from(vec![(10.0, 10.0), (30.0, 10.0)]);
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
            CutterCalibration::default(),
            &canvas(),
            &material,
            true,
            5.0,
            2.0,
            false,
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
            CutterCalibration::default(),
            &fixture_canvas,
            &material,
            false,
            5.0,
            2.0,
            false,
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
