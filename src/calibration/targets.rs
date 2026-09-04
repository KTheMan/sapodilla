//! Deterministic calibration layouts and original Sapodilla target artwork.
//!
//! The manifest is authored in physical millimetres. Raster conversion occurs
//! only in the renderer; cut geometry remains unquantized for the plotter
//! encoder's final device-unit conversion.

use std::{collections::BTreeSet, f64::consts::TAU};

use image::{Rgb, RgbImage};
use imageproc::{
    drawing::{
        draw_filled_circle_mut, draw_filled_rect_mut, draw_hollow_circle_mut, draw_hollow_rect_mut,
        draw_line_segment_mut,
    },
    rect::Rect,
};
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};
use thiserror::Error;

use super::CalibrationMethod;

pub const TARGET_MANIFEST_SCHEMA_VERSION: u16 = 1;
pub const PIXCUT_DPI: f64 = 300.0;
pub const PIXCUT_WIDTH_PX: u32 = 1200;
pub const PIXCUT_HEIGHT_PX: u32 = 2100;
pub const MM_PER_INCH: f64 = 25.4;

pub const MANUAL_CALIBRATION_REVISION: &str = "manual-seven-target-v1";
pub const MANUAL_VALIDATION_REVISION: &str = "manual-validation-five-v1";
pub const FLATBED_CALIBRATION_REVISION: &str = "flatbed-aperture-twelve-v1";
pub const FLATBED_VALIDATION_REVISION: &str = "flatbed-validation-six-v1";

const MANUAL_TARGET_MM: f64 = 14.0;
const APERTURE_PATCH_MM: f64 = 20.0;
const APERTURE_DIAMETER_MM: f64 = 10.0;
const APERTURE_BRIDGE_ARC_MM: f64 = 0.8;
const ARC_STEPS: usize = 20;
const RUN_BINDING_SYNC: u8 = 0b1010_0101;
// The full mark, including its 0.35 mm white surround, stays below the
// PixCut's unreliable ~1.5 mm top imaging strip and the documented 2 mm guard.
const RUN_BINDING_ORIGIN_MM: MmPoint = MmPoint::new(47.0, 2.5);
const RUN_BINDING_CELL_MM: f64 = 0.8;
const RUN_BINDING_COLUMNS: u8 = 8;
const RUN_BINDING_ROWS: u8 = 6;

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct MmPoint {
    pub x: f64,
    pub y: f64,
}

impl MmPoint {
    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct MmRect {
    pub origin: MmPoint,
    pub width: f64,
    pub height: f64,
}

impl MmRect {
    pub const fn new(x: f64, y: f64, width: f64, height: f64) -> Self {
        Self {
            origin: MmPoint::new(x, y),
            width,
            height,
        }
    }

    pub fn centered(center: MmPoint, width: f64, height: f64) -> Self {
        Self::new(
            center.x - width / 2.0,
            center.y - height / 2.0,
            width,
            height,
        )
    }

    pub fn center(self) -> MmPoint {
        MmPoint::new(
            self.origin.x + self.width / 2.0,
            self.origin.y + self.height / 2.0,
        )
    }

    pub fn inset(self, amount: f64) -> Self {
        Self::new(
            self.origin.x + amount,
            self.origin.y + amount,
            self.width - 2.0 * amount,
            self.height - 2.0 * amount,
        )
    }

    pub fn max_x(self) -> f64 {
        self.origin.x + self.width
    }

    pub fn max_y(self) -> f64 {
        self.origin.y + self.height
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct CanvasSpec {
    pub width_px: u32,
    pub height_px: u32,
    pub dpi: f64,
    pub width_mm: f64,
    pub height_mm: f64,
}

impl CanvasSpec {
    pub fn pixcut_4x7() -> Self {
        Self {
            width_px: PIXCUT_WIDTH_PX,
            height_px: PIXCUT_HEIGHT_PX,
            dpi: PIXCUT_DPI,
            width_mm: f64::from(PIXCUT_WIDTH_PX) * MM_PER_INCH / PIXCUT_DPI,
            height_mm: f64::from(PIXCUT_HEIGHT_PX) * MM_PER_INCH / PIXCUT_DPI,
        }
    }

    pub fn dots_per_mm(self) -> f64 {
        self.dpi / MM_PER_INCH
    }

    /// Converts to a continuous raster coordinate without premature rounding.
    pub fn mm_to_raster(self, point: MmPoint) -> [f64; 2] {
        let dots_per_mm = self.dots_per_mm();
        [point.x * dots_per_mm, point.y * dots_per_mm]
    }

    pub fn raster_pixel(self, point: MmPoint) -> [i32; 2] {
        let [x, y] = self.mm_to_raster(point);
        [x.round() as i32, y.round() as i32]
    }

    pub fn contains(self, point: MmPoint, tolerance_mm: f64) -> bool {
        point.x >= -tolerance_mm
            && point.y >= -tolerance_mm
            && point.x <= self.width_mm + tolerance_mm
            && point.y <= self.height_mm + tolerance_mm
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rgb8(pub u8, pub u8, pub u8);

impl From<Rgb8> for Rgb<u8> {
    fn from(value: Rgb8) -> Self {
        Rgb([value.0, value.1, value.2])
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CalibrationPurpose {
    Calibration,
    Validation,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestIdentity {
    pub run_id: String,
    pub baseline_mapping_id: String,
    pub profile_version: u16,
    #[serde(default)]
    pub candidate_generation: u32,
}

impl ManifestIdentity {
    pub fn stock(run_id: impl Into<String>) -> Self {
        Self {
            run_id: run_id.into(),
            baseline_mapping_id: "pixcut-s1-stock-v1".into(),
            profile_version: 1,
            candidate_generation: 0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TargetKind {
    ManualRectangle,
    FlatbedAperture,
    KissCutCheck,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TargetStation {
    pub id: String,
    pub kind: TargetKind,
    pub center_mm: MmPoint,
    pub print_bounds_mm: MmRect,
    pub nominal_cut_bounds_mm: MmRect,
    /// Clockwise bridge centers in degrees, with zero pointing right.
    pub bridge_angles_degrees: Vec<f64>,
    pub bridge_arc_mm: Option<f64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TargetCutMode {
    Kiss,
    Through,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TargetCutPhase {
    ProductionKiss,
    ApertureThrough,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CutGeometry {
    pub id: String,
    pub target_id: String,
    pub mode: TargetCutMode,
    pub phase: TargetCutPhase,
    /// Every polyline is a separate pen-down segment. The space between
    /// segments is an explicit blade-up bridge rather than a perf dash.
    pub pen_down_segments_mm: Vec<Vec<MmPoint>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FiducialKind {
    NestedSquareNorth,
    RingEast,
    DiamondSouth,
    BracketWest,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Fiducial {
    pub id: String,
    pub center_mm: MmPoint,
    pub kind: FiducialKind,
    pub extent_mm: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum PrintPrimitive {
    Line {
        start_mm: MmPoint,
        end_mm: MmPoint,
        width_mm: f64,
        color: Rgb8,
    },
    Rect {
        bounds_mm: MmRect,
        width_mm: f64,
        color: Rgb8,
        filled: bool,
    },
    Circle {
        center_mm: MmPoint,
        radius_mm: f64,
        width_mm: f64,
        color: Rgb8,
        filled: bool,
    },
    Text {
        origin_mm: MmPoint,
        value: String,
        height_mm: f64,
        color: Rgb8,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum TargetDiagnostic {
    PrintabilityInsets {
        values_mm: Vec<f64>,
    },
    PrintScaleBar {
        id: String,
        start_mm: MmPoint,
        end_mm: MmPoint,
        expected_length_mm: f64,
    },
    BackingSample {
        aperture_id: String,
        center_mm: MmPoint,
        diameter_mm: f64,
    },
    KissCutInspection {
        target_ids: Vec<String>,
    },
    /// A scanner-readable token binding the physical sheet to its run,
    /// layout, method, and calibration/validation purpose.
    RunBinding {
        origin_mm: MmPoint,
        cell_mm: f64,
        columns: u8,
        rows: u8,
        digest_hex: String,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TargetManifest {
    pub schema_version: u16,
    pub layout_revision: String,
    pub identity: ManifestIdentity,
    pub method: CalibrationMethod,
    pub purpose: CalibrationPurpose,
    pub canvas: CanvasSpec,
    pub targets: Vec<TargetStation>,
    pub fiducials: Vec<Fiducial>,
    pub print_primitives: Vec<PrintPrimitive>,
    pub cuts: Vec<CutGeometry>,
    pub diagnostics: Vec<TargetDiagnostic>,
}

impl TargetManifest {
    pub fn validate_geometry(&self) -> Result<(), LayoutError> {
        validate_identity(&self.identity)?;
        if self.schema_version != TARGET_MANIFEST_SCHEMA_VERSION {
            return Err(LayoutError::UnknownSchema(self.schema_version));
        }

        let mut ids = BTreeSet::new();
        for target in &self.targets {
            if !ids.insert(target.id.as_str()) {
                return Err(LayoutError::DuplicateTarget(target.id.clone()));
            }
            for bounds in [target.print_bounds_mm, target.nominal_cut_bounds_mm] {
                if !self.canvas.contains(bounds.origin, 0.001)
                    || !self
                        .canvas
                        .contains(MmPoint::new(bounds.max_x(), bounds.max_y()), 0.001)
                {
                    return Err(LayoutError::OutOfCanvas(target.id.clone()));
                }
            }
        }

        let mut fiducial_ids = BTreeSet::new();
        for fiducial in &self.fiducials {
            if !fiducial_ids.insert(fiducial.id.as_str()) {
                return Err(LayoutError::DuplicateFiducial(fiducial.id.clone()));
            }
            let half = fiducial.extent_mm / 2.0;
            if !self.canvas.contains(
                MmPoint::new(fiducial.center_mm.x - half, fiducial.center_mm.y - half),
                0.001,
            ) || !self.canvas.contains(
                MmPoint::new(fiducial.center_mm.x + half, fiducial.center_mm.y + half),
                0.001,
            ) {
                return Err(LayoutError::OutOfCanvas(fiducial.id.clone()));
            }
        }

        for cut in &self.cuts {
            if !ids.contains(cut.target_id.as_str()) && cut.target_id != "BACKING" {
                return Err(LayoutError::UnknownCutTarget(cut.target_id.clone()));
            }
            if cut.pen_down_segments_mm.is_empty()
                || cut
                    .pen_down_segments_mm
                    .iter()
                    .any(|points| points.len() < 2)
            {
                return Err(LayoutError::EmptyCut(cut.id.clone()));
            }
            if cut
                .pen_down_segments_mm
                .iter()
                .flatten()
                .any(|point| !self.canvas.contains(*point, 0.001))
            {
                return Err(LayoutError::OutOfCanvas(cut.id.clone()));
            }
        }

        if self
            .cuts
            .windows(2)
            .any(|pair| pair[0].phase > pair[1].phase)
        {
            return Err(LayoutError::CutPhaseOrder);
        }
        Ok(())
    }

    /// Compact reproducibility tag over stable struct/Vec serialization.
    pub fn stable_fingerprint(&self) -> String {
        let bytes = serde_json::to_vec(self).expect("manifest serialization cannot fail");
        let mut hash = 0xcbf29ce484222325u64;
        for byte in bytes {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
        format!("{hash:016x}")
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum LayoutError {
    #[error("calibration run id is empty or contains unsupported characters")]
    InvalidRunId,
    #[error("baseline mapping id is empty or contains unsupported characters")]
    InvalidBaselineMappingId,
    #[error("unknown target manifest schema {0}")]
    UnknownSchema(u16),
    #[error("duplicate calibration target {0}")]
    DuplicateTarget(String),
    #[error("duplicate calibration fiducial {0}")]
    DuplicateFiducial(String),
    #[error("geometry for {0} is outside the 4x7 canvas")]
    OutOfCanvas(String),
    #[error("cut references unknown target {0}")]
    UnknownCutTarget(String),
    #[error("cut {0} contains no drawable segment")]
    EmptyCut(String),
    #[error("through-cut phases must follow kiss-cut phases")]
    CutPhaseOrder,
    #[error("flatbed target positions do not cover every sheet quadrant")]
    InsufficientQuadrantCoverage,
}

/// Original rendering of the documented seven-target manual geometry.
pub fn manual_calibration(identity: ManifestIdentity) -> Result<TargetManifest, LayoutError> {
    validate_identity(&identity)?;
    let mut manifest = base_manifest(
        MANUAL_CALIBRATION_REVISION,
        identity,
        CalibrationMethod::ManualEastBay,
        CalibrationPurpose::Calibration,
    );
    add_manual_background(&mut manifest);
    add_page_fiducials(&mut manifest);
    for (id, x, y) in [
        ("C1", 8.0, 14.0),
        ("C2", 78.0, 14.0),
        ("C3", 8.0, 78.0),
        ("C4", 43.0, 78.0),
        ("C5", 78.0, 78.0),
        ("C6", 8.0, 146.0),
        ("C7", 78.0, 146.0),
    ] {
        add_manual_rectangle(
            &mut manifest,
            id,
            MmRect::new(x, y, MANUAL_TARGET_MM, MANUAL_TARGET_MM),
        );
    }
    manifest.diagnostics.extend([
        TargetDiagnostic::PrintabilityInsets {
            values_mm: vec![0.0, 1.0, 2.0, 3.0, 5.0, 10.0],
        },
        TargetDiagnostic::PrintScaleBar {
            id: "H80".into(),
            start_mm: MmPoint::new(10.0, 62.0),
            end_mm: MmPoint::new(90.0, 62.0),
            expected_length_mm: 80.0,
        },
        TargetDiagnostic::PrintScaleBar {
            id: "V150".into(),
            start_mm: MmPoint::new(62.0, 10.0),
            end_mm: MmPoint::new(62.0, 160.0),
            expected_length_mm: 150.0,
        },
    ]);
    add_common_metadata(&mut manifest);
    manifest.validate_geometry()?;
    Ok(manifest)
}

pub fn manual_validation(identity: ManifestIdentity) -> Result<TargetManifest, LayoutError> {
    validate_identity(&identity)?;
    let mut manifest = base_manifest(
        MANUAL_VALIDATION_REVISION,
        identity,
        CalibrationMethod::ManualEastBay,
        CalibrationPurpose::Validation,
    );
    add_light_grid(&mut manifest);
    add_page_fiducials(&mut manifest);
    for (id, center) in [
        ("V1", MmPoint::new(15.0, 21.0)),
        ("V2", MmPoint::new(85.0, 21.0)),
        ("V3", MmPoint::new(50.0, 85.0)),
        ("V4", MmPoint::new(15.0, 153.0)),
        ("V5", MmPoint::new(85.0, 153.0)),
    ] {
        add_manual_rectangle(
            &mut manifest,
            id,
            MmRect::centered(center, MANUAL_TARGET_MM, MANUAL_TARGET_MM),
        );
    }
    add_common_metadata(&mut manifest);
    manifest.validate_geometry()?;
    Ok(manifest)
}

pub fn flatbed_calibration(identity: ManifestIdentity) -> Result<TargetManifest, LayoutError> {
    validate_identity(&identity)?;
    let mut manifest = base_manifest(
        FLATBED_CALIBRATION_REVISION,
        identity,
        CalibrationMethod::FlatbedScanner,
        CalibrationPurpose::Calibration,
    );
    add_page_fiducials(&mut manifest);
    let mut index = 1;
    for y in [18.0, 65.0, 112.0, 159.0] {
        for x in [15.0, 50.8, 86.6] {
            add_aperture_station(&mut manifest, &format!("A{index:02}"), MmPoint::new(x, y));
            index += 1;
        }
    }
    add_backing_sample(&mut manifest, MmPoint::new(50.8, 41.5));
    add_common_metadata(&mut manifest);
    manifest.validate_geometry()?;
    validate_quadrant_coverage(&manifest)?;
    Ok(manifest)
}

pub fn flatbed_validation(identity: ManifestIdentity) -> Result<TargetManifest, LayoutError> {
    validate_identity(&identity)?;
    let mut manifest = base_manifest(
        FLATBED_VALIDATION_REVISION,
        identity,
        CalibrationMethod::FlatbedScanner,
        CalibrationPurpose::Validation,
    );
    add_page_fiducials(&mut manifest);
    for (id, center) in [
        ("VA1", MmPoint::new(15.0, 25.0)),
        ("VA2", MmPoint::new(50.8, 25.0)),
        ("VA3", MmPoint::new(86.6, 25.0)),
        ("VA4", MmPoint::new(15.0, 150.0)),
        ("VA5", MmPoint::new(50.8, 150.0)),
        ("VA6", MmPoint::new(86.6, 150.0)),
    ] {
        add_aperture_station(&mut manifest, id, center);
    }
    add_kiss_check(
        &mut manifest,
        "K1",
        MmRect::new(9.0, 72.0, 22.0, 18.0),
        KissShape::RoundedRectangle,
    );
    add_kiss_check(
        &mut manifest,
        "K2",
        MmRect::new(39.8, 72.0, 22.0, 18.0),
        KissShape::Circle,
    );
    add_kiss_check(
        &mut manifest,
        "K3",
        MmRect::new(70.6, 72.0, 22.0, 18.0),
        KissShape::Diamond,
    );
    // Visual production-pressure checks must execute before structural cuts.
    manifest.cuts.sort_by_key(|cut| cut.phase);
    manifest
        .diagnostics
        .push(TargetDiagnostic::KissCutInspection {
            target_ids: vec!["K1".into(), "K2".into(), "K3".into()],
        });
    add_backing_sample(&mut manifest, MmPoint::new(50.8, 112.0));
    add_common_metadata(&mut manifest);
    manifest.validate_geometry()?;
    validate_quadrant_coverage(&manifest)?;
    Ok(manifest)
}

fn base_manifest(
    revision: &str,
    identity: ManifestIdentity,
    method: CalibrationMethod,
    purpose: CalibrationPurpose,
) -> TargetManifest {
    TargetManifest {
        schema_version: TARGET_MANIFEST_SCHEMA_VERSION,
        layout_revision: revision.into(),
        identity,
        method,
        purpose,
        canvas: CanvasSpec::pixcut_4x7(),
        targets: Vec::new(),
        fiducials: Vec::new(),
        print_primitives: Vec::new(),
        cuts: Vec::new(),
        diagnostics: Vec::new(),
    }
}

fn validate_identity(identity: &ManifestIdentity) -> Result<(), LayoutError> {
    fn valid(value: &str) -> bool {
        !value.is_empty()
            && value.len() <= 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-_.:".contains(&byte))
    }
    if !valid(&identity.run_id) {
        return Err(LayoutError::InvalidRunId);
    }
    if !valid(&identity.baseline_mapping_id) {
        return Err(LayoutError::InvalidBaselineMappingId);
    }
    Ok(())
}

fn add_light_grid(manifest: &mut TargetManifest) {
    let canvas = manifest.canvas;
    let mut x = 0.0;
    while x <= canvas.width_mm + 1e-9 {
        let major = ((x / 10.0).round() - x / 10.0).abs() < 1e-9;
        manifest.print_primitives.push(PrintPrimitive::Line {
            start_mm: MmPoint::new(x, 0.0),
            end_mm: MmPoint::new(x, canvas.height_mm),
            width_mm: if major { 0.18 } else { 0.09 },
            color: if major {
                Rgb8(186, 191, 201)
            } else {
                Rgb8(226, 229, 234)
            },
        });
        x += 5.0;
    }
    let mut y = 0.0;
    while y <= canvas.height_mm + 1e-9 {
        let major = ((y / 10.0).round() - y / 10.0).abs() < 1e-9;
        manifest.print_primitives.push(PrintPrimitive::Line {
            start_mm: MmPoint::new(0.0, y),
            end_mm: MmPoint::new(canvas.width_mm, y),
            width_mm: if major { 0.18 } else { 0.09 },
            color: if major {
                Rgb8(186, 191, 201)
            } else {
                Rgb8(226, 229, 234)
            },
        });
        y += 5.0;
    }
}

fn add_manual_background(manifest: &mut TargetManifest) {
    add_light_grid(manifest);
    for (inset, color) in [
        (0.0, Rgb8(20, 20, 28)),
        (1.0, Rgb8(196, 44, 67)),
        (2.0, Rgb8(42, 107, 201)),
        (3.0, Rgb8(28, 143, 99)),
        (5.0, Rgb8(181, 111, 23)),
        (10.0, Rgb8(92, 92, 104)),
    ] {
        manifest.print_primitives.push(PrintPrimitive::Rect {
            bounds_mm: MmRect::new(
                inset,
                inset,
                manifest.canvas.width_mm - 2.0 * inset,
                manifest.canvas.height_mm - 2.0 * inset,
            ),
            width_mm: 0.18,
            color,
            filled: false,
        });
    }
    add_scale_bar(
        manifest,
        "H80",
        MmPoint::new(10.0, 62.0),
        MmPoint::new(90.0, 62.0),
    );
    add_scale_bar(
        manifest,
        "V150",
        MmPoint::new(62.0, 10.0),
        MmPoint::new(62.0, 160.0),
    );
}

fn add_scale_bar(manifest: &mut TargetManifest, id: &str, start: MmPoint, end: MmPoint) {
    let ink = Rgb8(26, 23, 32);
    manifest.print_primitives.push(PrintPrimitive::Line {
        start_mm: start,
        end_mm: end,
        width_mm: 0.32,
        color: ink,
    });
    let horizontal = (start.y - end.y).abs() < f64::EPSILON;
    for point in [start, end] {
        let (tick_start, tick_end) = if horizontal {
            (
                MmPoint::new(point.x, point.y - 2.0),
                MmPoint::new(point.x, point.y + 2.0),
            )
        } else {
            (
                MmPoint::new(point.x - 2.0, point.y),
                MmPoint::new(point.x + 2.0, point.y),
            )
        };
        manifest.print_primitives.push(PrintPrimitive::Line {
            start_mm: tick_start,
            end_mm: tick_end,
            width_mm: 0.32,
            color: ink,
        });
        add_cross(manifest, point, 1.25, Rgb8(0, 108, 150), 0.2);
    }
    manifest.print_primitives.push(PrintPrimitive::Text {
        origin_mm: if horizontal {
            MmPoint::new((start.x + end.x) / 2.0 - 4.0, start.y + 2.5)
        } else {
            MmPoint::new(start.x + 2.5, (start.y + end.y) / 2.0)
        },
        value: id.into(),
        height_mm: 2.3,
        color: ink,
    });
}

fn add_manual_rectangle(manifest: &mut TargetManifest, id: &str, bounds: MmRect) {
    let center = bounds.center();
    manifest.targets.push(TargetStation {
        id: id.into(),
        kind: TargetKind::ManualRectangle,
        center_mm: center,
        print_bounds_mm: bounds,
        nominal_cut_bounds_mm: bounds,
        bridge_angles_degrees: Vec::new(),
        bridge_arc_mm: None,
    });
    manifest.print_primitives.extend([
        PrintPrimitive::Rect {
            bounds_mm: bounds,
            width_mm: 0.3,
            color: Rgb8(200, 40, 71),
            filled: false,
        },
        PrintPrimitive::Rect {
            bounds_mm: bounds.inset(1.0),
            width_mm: 0.16,
            color: Rgb8(34, 142, 101),
            filled: false,
        },
    ]);
    add_cross(manifest, center, 3.5, Rgb8(0, 105, 164), 0.26);
    manifest.print_primitives.push(PrintPrimitive::Text {
        origin_mm: MmPoint::new(center.x - 2.2, center.y - 1.8),
        value: id.into(),
        height_mm: 3.0,
        color: Rgb8(55, 51, 64),
    });
    manifest.print_primitives.push(PrintPrimitive::Text {
        origin_mm: MmPoint::new(bounds.origin.x + 0.8, bounds.max_y() - 2.1),
        value: "14X14".into(),
        height_mm: 1.25,
        color: Rgb8(55, 51, 64),
    });
    manifest.cuts.push(CutGeometry {
        id: format!("cut-{id}"),
        target_id: id.into(),
        mode: TargetCutMode::Kiss,
        phase: TargetCutPhase::ProductionKiss,
        pen_down_segments_mm: vec![closed_rectangle(bounds)],
    });
}

fn add_aperture_station(manifest: &mut TargetManifest, id: &str, center: MmPoint) {
    let patch = MmRect::centered(center, APERTURE_PATCH_MM, APERTURE_PATCH_MM);
    let radius = APERTURE_DIAMETER_MM / 2.0;
    let cut_bounds = MmRect::centered(center, APERTURE_DIAMETER_MM, APERTURE_DIAMETER_MM);
    let bridges = vec![0.0, 90.0, 180.0, 270.0];
    manifest.targets.push(TargetStation {
        id: id.into(),
        kind: TargetKind::FlatbedAperture,
        center_mm: center,
        print_bounds_mm: patch,
        nominal_cut_bounds_mm: cut_bounds,
        bridge_angles_degrees: bridges.clone(),
        bridge_arc_mm: Some(APERTURE_BRIDGE_ARC_MM),
    });
    manifest.print_primitives.push(PrintPrimitive::Rect {
        bounds_mm: patch,
        width_mm: 0.0,
        color: Rgb8(24, 31, 74),
        filled: true,
    });
    manifest.print_primitives.push(PrintPrimitive::Circle {
        center_mm: center,
        radius_mm: radius + 1.6,
        width_mm: 0.36,
        color: Rgb8(255, 222, 51),
        filled: false,
    });
    for angle in [0.0_f64, 90.0, 180.0, 270.0] {
        let radians = angle.to_radians();
        manifest.print_primitives.push(PrintPrimitive::Line {
            start_mm: MmPoint::new(
                center.x + (radius + 1.0) * radians.cos(),
                center.y + (radius + 1.0) * radians.sin(),
            ),
            end_mm: MmPoint::new(
                center.x + (radius + 3.0) * radians.cos(),
                center.y + (radius + 3.0) * radians.sin(),
            ),
            width_mm: 0.32,
            color: Rgb8(255, 222, 51),
        });
    }
    manifest.print_primitives.push(PrintPrimitive::Text {
        origin_mm: MmPoint::new(patch.origin.x + 1.0, patch.origin.y + 1.0),
        value: id.into(),
        height_mm: 2.0,
        color: Rgb8(255, 255, 255),
    });
    manifest.cuts.push(CutGeometry {
        id: format!("aperture-{id}"),
        target_id: id.into(),
        mode: TargetCutMode::Through,
        phase: TargetCutPhase::ApertureThrough,
        pen_down_segments_mm: bridged_circle(center, radius, &bridges, APERTURE_BRIDGE_ARC_MM),
    });
}

fn add_backing_sample(manifest: &mut TargetManifest, center: MmPoint) {
    let diameter = 6.0;
    manifest.print_primitives.push(PrintPrimitive::Circle {
        center_mm: center,
        radius_mm: diameter / 2.0 + 1.2,
        width_mm: 0.32,
        color: Rgb8(224, 52, 136),
        filled: false,
    });
    manifest.print_primitives.push(PrintPrimitive::Text {
        origin_mm: MmPoint::new(center.x - 4.0, center.y + 4.7),
        value: "BACKING".into(),
        height_mm: 1.4,
        color: Rgb8(40, 35, 47),
    });
    manifest.cuts.push(CutGeometry {
        id: "backing-control-aperture".into(),
        target_id: "BACKING".into(),
        mode: TargetCutMode::Through,
        phase: TargetCutPhase::ApertureThrough,
        pen_down_segments_mm: bridged_circle(
            center,
            diameter / 2.0,
            &[45.0, 135.0, 225.0, 315.0],
            0.7,
        ),
    });
    manifest.diagnostics.push(TargetDiagnostic::BackingSample {
        aperture_id: "backing-control-aperture".into(),
        center_mm: center,
        diameter_mm: diameter,
    });
}

fn add_common_metadata(manifest: &mut TargetManifest) {
    let lines = [
        format!("RUN {}", manifest.identity.run_id),
        format!("REV {}", manifest.layout_revision),
        format!("PROFILE {}", manifest.identity.profile_version),
        format!("BASE {}", manifest.identity.baseline_mapping_id),
    ];
    for (index, value) in lines.into_iter().enumerate() {
        manifest.print_primitives.push(PrintPrimitive::Text {
            origin_mm: MmPoint::new(12.0, 1.0 + index as f64 * 1.65),
            value,
            height_mm: 1.1,
            color: Rgb8(25, 22, 31),
        });
    }
    add_run_binding(manifest);
}

fn add_run_binding(manifest: &mut TargetManifest) {
    let digest_hex = run_binding_digest(manifest);
    let bits = run_binding_bits(&digest_hex).expect("generated binding digest is valid");
    let width = f64::from(RUN_BINDING_COLUMNS) * RUN_BINDING_CELL_MM;
    let height = f64::from(RUN_BINDING_ROWS) * RUN_BINDING_CELL_MM;
    manifest.print_primitives.push(PrintPrimitive::Rect {
        bounds_mm: MmRect::new(
            RUN_BINDING_ORIGIN_MM.x - 0.35,
            RUN_BINDING_ORIGIN_MM.y - 0.35,
            width + 0.7,
            height + 0.7,
        ),
        width_mm: 0.0,
        color: Rgb8(255, 255, 255),
        filled: true,
    });
    for (index, bit) in bits.into_iter().enumerate() {
        if bit {
            let column = index % usize::from(RUN_BINDING_COLUMNS);
            let row = index / usize::from(RUN_BINDING_COLUMNS);
            manifest.print_primitives.push(PrintPrimitive::Rect {
                bounds_mm: MmRect::new(
                    RUN_BINDING_ORIGIN_MM.x + column as f64 * RUN_BINDING_CELL_MM,
                    RUN_BINDING_ORIGIN_MM.y + row as f64 * RUN_BINDING_CELL_MM,
                    RUN_BINDING_CELL_MM,
                    RUN_BINDING_CELL_MM,
                ),
                width_mm: 0.0,
                color: Rgb8(12, 12, 16),
                filled: true,
            });
        }
    }
    manifest.diagnostics.push(TargetDiagnostic::RunBinding {
        origin_mm: RUN_BINDING_ORIGIN_MM,
        cell_mm: RUN_BINDING_CELL_MM,
        columns: RUN_BINDING_COLUMNS,
        rows: RUN_BINDING_ROWS,
        digest_hex,
    });
}

fn run_binding_digest(manifest: &TargetManifest) -> String {
    let purpose = match manifest.purpose {
        CalibrationPurpose::Calibration => "calibration",
        CalibrationPurpose::Validation => "validation",
    };
    let method = match manifest.method {
        CalibrationMethod::FlatbedScanner => "flatbed-scanner",
        CalibrationMethod::ManualEastBay => "manual-east-bay",
    };
    let source = format!(
        "{}\n{}\n{}\n{}\n{}\n{}\n{}",
        manifest.identity.run_id,
        manifest.identity.baseline_mapping_id,
        manifest.identity.profile_version,
        manifest.identity.candidate_generation,
        manifest.layout_revision,
        method,
        purpose
    );
    hex::encode(Sha1::digest(source.as_bytes()))
}

/// Returns the 48 cells printed in a run-binding mark: an invariant sync byte,
/// 32 identity digest bits, and an eight-bit check complement.
pub(crate) fn run_binding_bits(digest_hex: &str) -> Option<Vec<bool>> {
    let digest = hex::decode(digest_hex).ok()?;
    if digest.len() != 20 {
        return None;
    }
    let bytes = [
        RUN_BINDING_SYNC,
        digest[0],
        digest[1],
        digest[2],
        digest[3],
        !digest[0],
    ];
    Some(
        bytes
            .into_iter()
            .flat_map(|byte| (0..8).rev().map(move |shift| byte & (1 << shift) != 0))
            .collect(),
    )
}

fn add_page_fiducials(manifest: &mut TargetManifest) {
    for (id, center, kind) in [
        (
            "F-TL",
            MmPoint::new(5.0, 5.0),
            FiducialKind::NestedSquareNorth,
        ),
        ("F-TR", MmPoint::new(96.6, 5.0), FiducialKind::RingEast),
        ("F-BL", MmPoint::new(5.0, 172.8), FiducialKind::DiamondSouth),
        ("F-BR", MmPoint::new(96.6, 172.8), FiducialKind::BracketWest),
    ] {
        manifest.fiducials.push(Fiducial {
            id: id.into(),
            center_mm: center,
            kind,
            extent_mm: 5.0,
        });
        add_fiducial_primitives(manifest, center, kind);
    }
}

fn add_fiducial_primitives(manifest: &mut TargetManifest, center: MmPoint, kind: FiducialKind) {
    let ink = Rgb8(18, 18, 24);
    match kind {
        FiducialKind::NestedSquareNorth => {
            for size in [5.0, 2.6] {
                manifest.print_primitives.push(PrintPrimitive::Rect {
                    bounds_mm: MmRect::centered(center, size, size),
                    width_mm: 0.45,
                    color: ink,
                    filled: false,
                });
            }
            add_marker_tick(
                manifest,
                center,
                MmPoint::new(center.x, center.y - 4.0),
                ink,
            );
        }
        FiducialKind::RingEast => {
            manifest.print_primitives.extend([
                PrintPrimitive::Circle {
                    center_mm: center,
                    radius_mm: 2.5,
                    width_mm: 0.45,
                    color: ink,
                    filled: false,
                },
                PrintPrimitive::Circle {
                    center_mm: center,
                    radius_mm: 0.8,
                    width_mm: 0.0,
                    color: ink,
                    filled: true,
                },
            ]);
            add_marker_tick(
                manifest,
                center,
                MmPoint::new(center.x + 4.0, center.y),
                ink,
            );
        }
        FiducialKind::DiamondSouth => {
            let points = [
                MmPoint::new(center.x, center.y - 2.8),
                MmPoint::new(center.x + 2.8, center.y),
                MmPoint::new(center.x, center.y + 2.8),
                MmPoint::new(center.x - 2.8, center.y),
                MmPoint::new(center.x, center.y - 2.8),
            ];
            for pair in points.windows(2) {
                manifest.print_primitives.push(PrintPrimitive::Line {
                    start_mm: pair[0],
                    end_mm: pair[1],
                    width_mm: 0.45,
                    color: ink,
                });
            }
            add_marker_tick(
                manifest,
                center,
                MmPoint::new(center.x, center.y + 4.0),
                ink,
            );
        }
        FiducialKind::BracketWest => {
            let (x0, x1) = (center.x - 2.5, center.x + 2.5);
            let (y0, y1) = (center.y - 2.5, center.y + 2.5);
            for (start, end) in [
                (MmPoint::new(x0, y0), MmPoint::new(x0, y1)),
                (MmPoint::new(x0, y0), MmPoint::new(x1, y0)),
                (MmPoint::new(x0, y1), MmPoint::new(x1, y1)),
            ] {
                manifest.print_primitives.push(PrintPrimitive::Line {
                    start_mm: start,
                    end_mm: end,
                    width_mm: 0.5,
                    color: ink,
                });
            }
            add_marker_tick(
                manifest,
                center,
                MmPoint::new(center.x - 4.0, center.y),
                ink,
            );
        }
    }
}

fn add_marker_tick(manifest: &mut TargetManifest, center: MmPoint, endpoint: MmPoint, color: Rgb8) {
    let dx = endpoint.x - center.x;
    let dy = endpoint.y - center.y;
    manifest.print_primitives.push(PrintPrimitive::Line {
        start_mm: MmPoint::new(center.x + dx * 0.63, center.y + dy * 0.63),
        end_mm: endpoint,
        width_mm: 0.6,
        color,
    });
}

#[derive(Clone, Copy)]
enum KissShape {
    RoundedRectangle,
    Circle,
    Diamond,
}

fn add_kiss_check(manifest: &mut TargetManifest, id: &str, bounds: MmRect, shape: KissShape) {
    let center = bounds.center();
    let path = match shape {
        KissShape::RoundedRectangle => rounded_rectangle(bounds, 3.0, 8),
        KissShape::Circle => ellipse(bounds, 64),
        KissShape::Diamond => vec![
            MmPoint::new(center.x, bounds.origin.y),
            MmPoint::new(bounds.max_x(), center.y),
            MmPoint::new(center.x, bounds.max_y()),
            MmPoint::new(bounds.origin.x, center.y),
            MmPoint::new(center.x, bounds.origin.y),
        ],
    };
    manifest.targets.push(TargetStation {
        id: id.into(),
        kind: TargetKind::KissCutCheck,
        center_mm: center,
        print_bounds_mm: bounds,
        nominal_cut_bounds_mm: bounds,
        bridge_angles_degrees: Vec::new(),
        bridge_arc_mm: None,
    });
    let inset_bounds = bounds.inset(1.0);
    let inset_path = match shape {
        KissShape::RoundedRectangle => rounded_rectangle(inset_bounds, 2.3, 8),
        KissShape::Circle => ellipse(inset_bounds, 64),
        KissShape::Diamond => {
            let center = inset_bounds.center();
            vec![
                MmPoint::new(center.x, inset_bounds.origin.y),
                MmPoint::new(inset_bounds.max_x(), center.y),
                MmPoint::new(center.x, inset_bounds.max_y()),
                MmPoint::new(inset_bounds.origin.x, center.y),
                MmPoint::new(center.x, inset_bounds.origin.y),
            ]
        }
    };
    add_print_polyline(manifest, &path, 0.6, Rgb8(224, 52, 136));
    add_print_polyline(manifest, &inset_path, 0.22, Rgb8(33, 154, 178));
    manifest.print_primitives.push(PrintPrimitive::Text {
        origin_mm: MmPoint::new(center.x - 1.8, center.y - 1.3),
        value: id.into(),
        height_mm: 2.2,
        color: Rgb8(45, 39, 51),
    });
    manifest.cuts.push(CutGeometry {
        id: format!("kiss-check-{id}"),
        target_id: id.into(),
        mode: TargetCutMode::Kiss,
        phase: TargetCutPhase::ProductionKiss,
        pen_down_segments_mm: vec![path],
    });
}

fn add_print_polyline(
    manifest: &mut TargetManifest,
    points: &[MmPoint],
    width_mm: f64,
    color: Rgb8,
) {
    manifest
        .print_primitives
        .extend(points.windows(2).map(|pair| PrintPrimitive::Line {
            start_mm: pair[0],
            end_mm: pair[1],
            width_mm,
            color,
        }));
}

fn add_cross(
    manifest: &mut TargetManifest,
    center: MmPoint,
    arm_mm: f64,
    color: Rgb8,
    width_mm: f64,
) {
    manifest.print_primitives.extend([
        PrintPrimitive::Line {
            start_mm: MmPoint::new(center.x - arm_mm, center.y),
            end_mm: MmPoint::new(center.x + arm_mm, center.y),
            width_mm,
            color,
        },
        PrintPrimitive::Line {
            start_mm: MmPoint::new(center.x, center.y - arm_mm),
            end_mm: MmPoint::new(center.x, center.y + arm_mm),
            width_mm,
            color,
        },
        PrintPrimitive::Circle {
            center_mm: center,
            radius_mm: width_mm,
            width_mm: 0.0,
            color,
            filled: true,
        },
    ]);
}

fn closed_rectangle(bounds: MmRect) -> Vec<MmPoint> {
    vec![
        bounds.origin,
        MmPoint::new(bounds.max_x(), bounds.origin.y),
        MmPoint::new(bounds.max_x(), bounds.max_y()),
        MmPoint::new(bounds.origin.x, bounds.max_y()),
        bounds.origin,
    ]
}

fn bridged_circle(
    center: MmPoint,
    radius_mm: f64,
    bridge_centers_degrees: &[f64],
    bridge_arc_mm: f64,
) -> Vec<Vec<MmPoint>> {
    let half_gap = bridge_arc_mm / radius_mm / 2.0;
    let mut centers = bridge_centers_degrees
        .iter()
        .map(|degrees| degrees.to_radians().rem_euclid(TAU))
        .collect::<Vec<_>>();
    centers.sort_by(f64::total_cmp);
    (0..centers.len())
        .map(|index| {
            let start = centers[index] + half_gap;
            let mut end = centers[(index + 1) % centers.len()] - half_gap;
            if end <= start {
                end += TAU;
            }
            (0..=ARC_STEPS)
                .map(|step| {
                    let t = step as f64 / ARC_STEPS as f64;
                    let angle = start + (end - start) * t;
                    MmPoint::new(
                        center.x + radius_mm * angle.cos(),
                        center.y + radius_mm * angle.sin(),
                    )
                })
                .collect()
        })
        .collect()
}

fn ellipse(bounds: MmRect, steps: usize) -> Vec<MmPoint> {
    let center = bounds.center();
    (0..=steps)
        .map(|step| {
            let angle = TAU * step as f64 / steps as f64;
            MmPoint::new(
                center.x + bounds.width / 2.0 * angle.cos(),
                center.y + bounds.height / 2.0 * angle.sin(),
            )
        })
        .collect()
}

fn rounded_rectangle(bounds: MmRect, radius: f64, steps: usize) -> Vec<MmPoint> {
    let radius = radius.min(bounds.width / 2.0).min(bounds.height / 2.0);
    let corners = [
        (bounds.max_x() - radius, bounds.origin.y + radius, -90.0),
        (bounds.max_x() - radius, bounds.max_y() - radius, 0.0),
        (bounds.origin.x + radius, bounds.max_y() - radius, 90.0),
        (bounds.origin.x + radius, bounds.origin.y + radius, 180.0),
    ];
    let mut points = Vec::with_capacity(4 * (steps + 1) + 1);
    for (cx, cy, start) in corners {
        for step in 0..=steps {
            let angle = (start + 90.0 * step as f64 / steps as f64).to_radians();
            points.push(MmPoint::new(
                cx + radius * angle.cos(),
                cy + radius * angle.sin(),
            ));
        }
    }
    points.push(points[0]);
    points
}

fn validate_quadrant_coverage(manifest: &TargetManifest) -> Result<(), LayoutError> {
    let mid_x = manifest.canvas.width_mm / 2.0;
    let mid_y = manifest.canvas.height_mm / 2.0;
    let mut counts = [0usize; 4];
    for target in manifest
        .targets
        .iter()
        .filter(|target| target.kind == TargetKind::FlatbedAperture)
    {
        let x = usize::from(target.center_mm.x >= mid_x);
        let y = usize::from(target.center_mm.y >= mid_y);
        counts[y * 2 + x] += 1;
    }
    if counts.into_iter().any(|count| count == 0) {
        Err(LayoutError::InsufficientQuadrantCoverage)
    } else {
        Ok(())
    }
}

/// Render the manifest's original print artwork at its declared raster size.
pub fn render_print_raster(manifest: &TargetManifest) -> Result<RgbImage, LayoutError> {
    manifest.validate_geometry()?;
    let canvas = manifest.canvas;
    let mut image = RgbImage::from_pixel(canvas.width_px, canvas.height_px, Rgb([255, 255, 255]));
    for primitive in &manifest.print_primitives {
        render_primitive(&mut image, canvas, primitive);
    }
    Ok(image)
}

/// Render a non-measurement preview with direct cut segments overlaid.
pub fn render_preview(manifest: &TargetManifest) -> Result<RgbImage, LayoutError> {
    let mut image = render_print_raster(manifest)?;
    for cut in &manifest.cuts {
        let color = match cut.mode {
            TargetCutMode::Kiss => Rgb([25, 138, 103]),
            TargetCutMode::Through => Rgb([225, 51, 74]),
        };
        for segment in &cut.pen_down_segments_mm {
            for pair in segment.windows(2) {
                let [x0, y0] = manifest.canvas.mm_to_raster(pair[0]);
                let [x1, y1] = manifest.canvas.mm_to_raster(pair[1]);
                draw_line_segment_mut(
                    &mut image,
                    (x0 as f32, y0 as f32),
                    (x1 as f32, y1 as f32),
                    color,
                );
            }
        }
    }
    Ok(image)
}

fn render_primitive(image: &mut RgbImage, canvas: CanvasSpec, primitive: &PrintPrimitive) {
    match primitive {
        PrintPrimitive::Line {
            start_mm,
            end_mm,
            width_mm,
            color,
        } => draw_thick_line(
            image,
            canvas,
            *start_mm,
            *end_mm,
            *width_mm,
            (*color).into(),
        ),
        PrintPrimitive::Rect {
            bounds_mm,
            width_mm,
            color,
            filled,
        } => {
            let [x0, y0] = canvas.raster_pixel(bounds_mm.origin);
            let [x1, y1] = canvas.raster_pixel(MmPoint::new(bounds_mm.max_x(), bounds_mm.max_y()));
            let width = (x1 - x0).unsigned_abs().max(1);
            let height = (y1 - y0).unsigned_abs().max(1);
            let rect = Rect::at(x0.min(x1), y0.min(y1)).of_size(width, height);
            if *filled {
                draw_filled_rect_mut(image, rect, (*color).into());
            } else {
                let strokes = ((*width_mm * canvas.dots_per_mm()).round() as i32).max(1);
                for inset in 0..strokes {
                    let width = width.saturating_sub(2 * inset as u32);
                    let height = height.saturating_sub(2 * inset as u32);
                    if width > 0 && height > 0 {
                        draw_hollow_rect_mut(
                            image,
                            Rect::at(x0.min(x1) + inset, y0.min(y1) + inset).of_size(width, height),
                            (*color).into(),
                        );
                    }
                }
            }
        }
        PrintPrimitive::Circle {
            center_mm,
            radius_mm,
            width_mm,
            color,
            filled,
        } => {
            let [x, y] = canvas.raster_pixel(*center_mm);
            let radius = (*radius_mm * canvas.dots_per_mm()).round() as i32;
            if *filled {
                draw_filled_circle_mut(image, (x, y), radius.max(1), (*color).into());
            } else {
                let strokes = ((*width_mm * canvas.dots_per_mm()).round() as i32).max(1);
                for inset in 0..strokes {
                    if radius - inset > 0 {
                        draw_hollow_circle_mut(image, (x, y), radius - inset, (*color).into());
                    }
                }
            }
        }
        PrintPrimitive::Text {
            origin_mm,
            value,
            height_mm,
            color,
        } => render_dot_text(
            image,
            canvas,
            *origin_mm,
            value,
            *height_mm,
            (*color).into(),
        ),
    }
}

fn draw_thick_line(
    image: &mut RgbImage,
    canvas: CanvasSpec,
    start: MmPoint,
    end: MmPoint,
    width_mm: f64,
    color: Rgb<u8>,
) {
    let [x0, y0] = canvas.mm_to_raster(start);
    let [x1, y1] = canvas.mm_to_raster(end);
    let pixels = (width_mm * canvas.dots_per_mm()).round().max(1.0) as i32;
    let half = pixels / 2;
    let horizontalish = (x1 - x0).abs() >= (y1 - y0).abs();
    for offset in -half..=(pixels - half - 1) {
        let (ox, oy) = if horizontalish {
            (0.0, offset as f32)
        } else {
            (offset as f32, 0.0)
        };
        draw_line_segment_mut(
            image,
            (x0 as f32 + ox, y0 as f32 + oy),
            (x1 as f32 + ox, y1 as f32 + oy),
            color,
        );
    }
}

/// Small built-in dot font keeps generated artwork deterministic and avoids
/// depending on platform font discovery.
fn render_dot_text(
    image: &mut RgbImage,
    canvas: CanvasSpec,
    origin: MmPoint,
    value: &str,
    height_mm: f64,
    color: Rgb<u8>,
) {
    let cell = (height_mm * canvas.dots_per_mm() / 7.0).round().max(1.0) as i32;
    let [origin_x, origin_y] = canvas.raster_pixel(origin);
    let mut cursor_x = origin_x;
    for character in value.to_ascii_uppercase().chars() {
        for (row, bits) in glyph(character).iter().enumerate() {
            for column in 0..5 {
                if bits & (1 << (4 - column)) != 0 {
                    draw_filled_rect_mut(
                        image,
                        Rect::at(cursor_x + column * cell, origin_y + row as i32 * cell)
                            .of_size(cell as u32, cell as u32),
                        color,
                    );
                }
            }
        }
        cursor_x += 6 * cell;
    }
}

fn glyph(c: char) -> [u8; 7] {
    match c {
        'A' => [14, 17, 17, 31, 17, 17, 17],
        'B' => [30, 17, 17, 30, 17, 17, 30],
        'C' => [14, 17, 16, 16, 16, 17, 14],
        'D' => [30, 17, 17, 17, 17, 17, 30],
        'E' => [31, 16, 16, 30, 16, 16, 31],
        'F' => [31, 16, 16, 30, 16, 16, 16],
        'G' => [14, 17, 16, 23, 17, 17, 15],
        'H' => [17, 17, 17, 31, 17, 17, 17],
        'I' => [14, 4, 4, 4, 4, 4, 14],
        'J' => [7, 2, 2, 2, 18, 18, 12],
        'K' => [17, 18, 20, 24, 20, 18, 17],
        'L' => [16, 16, 16, 16, 16, 16, 31],
        'M' => [17, 27, 21, 21, 17, 17, 17],
        'N' => [17, 25, 21, 19, 17, 17, 17],
        'O' => [14, 17, 17, 17, 17, 17, 14],
        'P' => [30, 17, 17, 30, 16, 16, 16],
        'Q' => [14, 17, 17, 17, 21, 18, 13],
        'R' => [30, 17, 17, 30, 20, 18, 17],
        'S' => [15, 16, 16, 14, 1, 1, 30],
        'T' => [31, 4, 4, 4, 4, 4, 4],
        'U' => [17, 17, 17, 17, 17, 17, 14],
        'V' => [17, 17, 17, 17, 17, 10, 4],
        'W' => [17, 17, 17, 21, 21, 21, 10],
        'X' => [17, 17, 10, 4, 10, 17, 17],
        'Y' => [17, 17, 10, 4, 4, 4, 4],
        'Z' => [31, 1, 2, 4, 8, 16, 31],
        '0' => [14, 17, 19, 21, 25, 17, 14],
        '1' => [4, 12, 4, 4, 4, 4, 14],
        '2' => [14, 17, 1, 2, 4, 8, 31],
        '3' => [30, 1, 1, 14, 1, 1, 30],
        '4' => [2, 6, 10, 18, 31, 2, 2],
        '5' => [31, 16, 16, 30, 1, 1, 30],
        '6' => [14, 16, 16, 30, 17, 17, 14],
        '7' => [31, 1, 2, 4, 8, 8, 8],
        '8' => [14, 17, 17, 14, 17, 17, 14],
        '9' => [14, 17, 17, 15, 1, 1, 14],
        '-' => [0, 0, 0, 31, 0, 0, 0],
        '_' => [0, 0, 0, 0, 0, 0, 31],
        '.' => [0, 0, 0, 0, 0, 6, 6],
        ':' => [0, 6, 6, 0, 6, 6, 0],
        ' ' => [0; 7],
        _ => [31, 17, 1, 2, 4, 0, 4],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(run_id: &str) -> ManifestIdentity {
        ManifestIdentity::stock(run_id)
    }

    #[test]
    fn physical_canvas_is_exactly_4x7_at_300_dpi() {
        let canvas = CanvasSpec::pixcut_4x7();
        assert_eq!(canvas.width_px, 1200);
        assert_eq!(canvas.height_px, 2100);
        assert!((canvas.width_mm - 101.6).abs() < 1e-12);
        assert!((canvas.height_mm - 177.8).abs() < 1e-12);
        assert_eq!(
            canvas.raster_pixel(MmPoint::new(101.6, 177.8)),
            [1200, 2100]
        );
        assert!((canvas.dots_per_mm() - 11.811_023_622_047_244).abs() < 1e-12);
    }

    #[test]
    fn manual_layout_has_exact_documented_geometry() {
        let manifest = manual_calibration(identity("manual-001")).unwrap();
        let expected = [
            ("C1", [8.0, 14.0], [15.0, 21.0]),
            ("C2", [78.0, 14.0], [85.0, 21.0]),
            ("C3", [8.0, 78.0], [15.0, 85.0]),
            ("C4", [43.0, 78.0], [50.0, 85.0]),
            ("C5", [78.0, 78.0], [85.0, 85.0]),
            ("C6", [8.0, 146.0], [15.0, 153.0]),
            ("C7", [78.0, 146.0], [85.0, 153.0]),
        ];
        assert_eq!(manifest.targets.len(), expected.len());
        for (target, (id, origin, center)) in manifest.targets.iter().zip(expected) {
            assert_eq!(target.id, id);
            assert_eq!(
                target.print_bounds_mm.origin,
                MmPoint::new(origin[0], origin[1])
            );
            assert_eq!(target.center_mm, MmPoint::new(center[0], center[1]));
            assert_eq!(target.nominal_cut_bounds_mm.width, 14.0);
            assert_eq!(target.nominal_cut_bounds_mm.height, 14.0);
        }
        assert!(manifest.cuts.iter().all(|cut| {
            cut.mode == TargetCutMode::Kiss
                && cut.phase == TargetCutPhase::ProductionKiss
                && cut.pen_down_segments_mm[0].first() == cut.pen_down_segments_mm[0].last()
        }));
    }

    #[test]
    fn manual_diagnostics_include_insets_h80_and_v150() {
        let diagnostics = manual_calibration(identity("manual-diag"))
            .unwrap()
            .diagnostics;
        assert!(diagnostics.contains(&TargetDiagnostic::PrintabilityInsets {
            values_mm: vec![0.0, 1.0, 2.0, 3.0, 5.0, 10.0],
        }));
        assert!(diagnostics.contains(&TargetDiagnostic::PrintScaleBar {
            id: "H80".into(),
            start_mm: MmPoint::new(10.0, 62.0),
            end_mm: MmPoint::new(90.0, 62.0),
            expected_length_mm: 80.0,
        }));
        assert!(diagnostics.contains(&TargetDiagnostic::PrintScaleBar {
            id: "V150".into(),
            start_mm: MmPoint::new(62.0, 10.0),
            end_mm: MmPoint::new(62.0, 160.0),
            expected_length_mm: 150.0,
        }));
    }

    #[test]
    fn flatbed_layout_has_twelve_distributed_bridged_apertures() {
        let manifest = flatbed_calibration(identity("flatbed-001")).unwrap();
        assert_eq!(manifest.targets.len(), 12);
        assert_eq!(manifest.fiducials.len(), 4);
        let aperture_cuts = manifest
            .cuts
            .iter()
            .filter(|cut| cut.target_id != "BACKING")
            .collect::<Vec<_>>();
        assert_eq!(aperture_cuts.len(), 12);
        for (target, cut) in manifest.targets.iter().zip(aperture_cuts) {
            assert_eq!(target.bridge_angles_degrees, vec![0.0, 90.0, 180.0, 270.0]);
            assert_eq!(target.bridge_arc_mm, Some(0.8));
            assert_eq!(cut.mode, TargetCutMode::Through);
            assert_eq!(cut.pen_down_segments_mm.len(), 4);
            assert!(
                cut.pen_down_segments_mm
                    .iter()
                    .all(|points| points.len() == 21)
            );
        }
        validate_quadrant_coverage(&manifest).unwrap();
    }

    #[test]
    fn validation_layouts_have_required_targets_and_phase_order() {
        let manual = manual_validation(identity("manual-validation")).unwrap();
        assert_eq!(manual.targets.len(), 5);

        let flatbed = flatbed_validation(identity("flatbed-validation")).unwrap();
        assert_eq!(
            flatbed
                .targets
                .iter()
                .filter(|target| target.kind == TargetKind::FlatbedAperture)
                .count(),
            6
        );
        assert_eq!(
            flatbed
                .targets
                .iter()
                .filter(|target| target.kind == TargetKind::KissCutCheck)
                .count(),
            3
        );
        assert!(
            flatbed
                .cuts
                .windows(2)
                .all(|pair| pair[0].phase <= pair[1].phase)
        );
        validate_quadrant_coverage(&flatbed).unwrap();
    }

    #[test]
    fn generation_fingerprint_and_rendering_are_deterministic() {
        let a = flatbed_calibration(identity("stable-run-7")).unwrap();
        let b = flatbed_calibration(identity("stable-run-7")).unwrap();
        assert_eq!(a, b);
        assert_eq!(a.stable_fingerprint(), b.stable_fingerprint());
        assert_ne!(
            a.stable_fingerprint(),
            flatbed_calibration(identity("stable-run-8"))
                .unwrap()
                .stable_fingerprint()
        );
        let raster_a = render_print_raster(&a).unwrap();
        let raster_b = render_print_raster(&b).unwrap();
        assert_eq!(raster_a.dimensions(), (1200, 2100));
        assert_eq!(raster_a.as_raw(), raster_b.as_raw());
        assert!(raster_a.pixels().any(|pixel| pixel.0 != [255, 255, 255]));
        assert_ne!(render_preview(&a).unwrap().as_raw(), raster_a.as_raw());
    }

    #[test]
    fn invalid_identity_and_duplicate_geometry_are_rejected() {
        assert_eq!(
            manual_calibration(ManifestIdentity::stock("bad run")),
            Err(LayoutError::InvalidRunId)
        );
        let mut manifest = manual_calibration(identity("duplicate-check")).unwrap();
        manifest.targets.push(manifest.targets[0].clone());
        assert_eq!(
            manifest.validate_geometry(),
            Err(LayoutError::DuplicateTarget("C1".into()))
        );
    }

    #[test]
    fn bridge_gaps_are_real_pen_up_discontinuities() {
        let manifest = flatbed_calibration(identity("bridge-check")).unwrap();
        let cut = manifest
            .cuts
            .iter()
            .find(|cut| cut.target_id == "A01")
            .unwrap();
        for adjacent in cut.pen_down_segments_mm.windows(2) {
            assert_ne!(adjacent[0].last(), adjacent[1].first());
        }
        assert_ne!(
            cut.pen_down_segments_mm.last().unwrap().last(),
            cut.pen_down_segments_mm.first().unwrap().first()
        );
    }
}
