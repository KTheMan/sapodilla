//! Local-first sticker studio primitives.
//!
//! This module intentionally contains no UI or device code.  Keeping layout,
//! document interchange, background removal, and cut preparation as ordinary
//! Rust makes the same behavior available to the native and WebAssembly apps.

use std::collections::VecDeque;

use crate::toolpath::CutMode;
use anyhow::{Context, bail};
use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use egui::{Pos2, Vec2};
use geo::{Coord, LineString};
use image::{Rgba, RgbaImage};
use serde::{Deserialize, Serialize};
use svgtypes::{
    Align, AspectRatio, Length, LengthUnit, NumberListParser, PathParser, PathSegment, Transform,
    ViewBox,
};

pub const DOCUMENT_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DocumentKind {
    /// A reusable single sticker with its cut paths.
    Sticker,
    /// A complete laid-out sheet.
    #[default]
    Sheet,
    /// A reusable sheet where artwork can be replaced without moving cuts.
    Template,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SavedImage {
    /// Stable document-local identity used by cutlines and template slots.
    ///
    /// Older documents deserialize this as an empty string. Call
    /// [`StudioDocument::ensure_object_ids`] before creating relationships.
    #[serde(default)]
    pub id: String,
    pub name: String,
    /// PNG data. Base64 keeps the JSON-based interchange format portable.
    pub png: String,
    pub offset: [f32; 2],
    pub scale: [f32; 2],
    pub rotation_degrees: f32,
    pub cutting_enabled: bool,
    pub locked: bool,
    pub visible: bool,
}

impl SavedImage {
    #[allow(clippy::too_many_arguments)]
    pub fn from_png(
        name: impl Into<String>,
        png: &[u8],
        offset: Pos2,
        scale: Vec2,
        rotation_degrees: f32,
        cutting_enabled: bool,
        locked: bool,
        visible: bool,
    ) -> Self {
        Self {
            id: String::new(),
            name: name.into(),
            png: BASE64.encode(png),
            offset: [offset.x, offset.y],
            scale: [scale.x, scale.y],
            rotation_degrees,
            cutting_enabled,
            locked,
            visible,
        }
    }

    pub fn png_bytes(&self) -> anyhow::Result<Vec<u8>> {
        BASE64
            .decode(&self.png)
            .context("saved image contains invalid base64 PNG data")
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StudioDocument {
    pub version: u32,
    pub kind: DocumentKind,
    pub canvas_size: [f32; 2],
    pub background: [u8; 3],
    pub images: Vec<SavedImage>,
    pub cut_paths: Vec<Vec<[f32; 2]>>,
    /// Relationship metadata for entries in `cut_paths`. This is separate from
    /// the geometry so version-one documents and existing editing code remain
    /// wire-compatible.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cutline_metadata: Vec<CutlineMetadata>,
    /// Replaceable regions in template documents.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub template_placeholders: Vec<TemplatePlaceholder>,
    pub material: MaterialProfile,
    #[serde(default)]
    pub settings: DocumentSettings,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind", content = "id")]
pub enum CutlineOwner {
    Image(String),
    TemplatePlaceholder(String),
}

/// Document metadata associated with one legacy `cut_paths` entry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CutlineMetadata {
    pub id: String,
    pub cut_path_index: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<CutlineOwner>,
    #[serde(default)]
    pub cut_mode: CutMode,
    /// Template-owned cut geometry can be used for production but not edited.
    #[serde(default)]
    pub locked: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PlaceholderFit {
    /// Scale uniformly until the whole replacement is visible.
    Contain,
    /// Scale uniformly until the placeholder is completely covered.
    #[default]
    Cover,
    /// Scale independently on each axis.
    Stretch,
}

/// A stable, replaceable artwork region in a reusable template.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TemplatePlaceholder {
    pub id: String,
    pub name: String,
    /// `[x, y, width, height]` in canvas coordinates.
    pub bounds: [f32; 4],
    #[serde(default)]
    pub rotation_degrees: f32,
    #[serde(default)]
    pub fit: PlaceholderFit,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assigned_image_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DocumentSettings {
    pub selected_device: usize,
    pub selected_mode: usize,
    pub selected_canvas_size: usize,
    pub copies: usize,
    pub cut_buffer: f32,
    pub cut_minimum_length: f32,
    pub cut_smoothing: usize,
    pub cut_simplify: f32,
    pub cut_internal: bool,
    pub cut_white_transparent: bool,
    pub perf_cut: bool,
    pub perf_dash_mm: f32,
    pub perf_gap_mm: f32,
    pub peel_tabs: bool,
    pub pack_gap_mm: f32,
    pub pack_allow_rotation: bool,
    /// Add a short lead-in/lead-out ramp at closed kiss-cut seams.
    #[serde(default)]
    pub overcut_enabled: bool,
    #[serde(default = "default_overcut_steps")]
    pub overcut_steps: usize,
    #[serde(default = "default_overcut_angle")]
    pub overcut_maximum_angle_degrees: f32,
    #[serde(default = "default_overcut_reach_mm")]
    pub overcut_reach_mm: f32,
    #[serde(default = "default_true")]
    pub overcut_snap_to_pixels: bool,
}

const fn default_overcut_steps() -> usize {
    3
}

fn default_overcut_angle() -> f32 {
    45.0
}

fn default_overcut_reach_mm() -> f32 {
    1.27
}

const fn default_true() -> bool {
    true
}

impl Default for DocumentSettings {
    fn default() -> Self {
        Self {
            selected_device: 0,
            selected_mode: 0,
            selected_canvas_size: 0,
            copies: 1,
            cut_buffer: 300.0 / 25.4,
            cut_minimum_length: 75.0,
            cut_smoothing: 2,
            cut_simplify: 1.5,
            cut_internal: false,
            cut_white_transparent: true,
            perf_cut: false,
            perf_dash_mm: 1.5,
            perf_gap_mm: 0.5,
            peel_tabs: false,
            pack_gap_mm: 2.0,
            pack_allow_rotation: true,
            overcut_enabled: false,
            overcut_steps: default_overcut_steps(),
            overcut_maximum_angle_degrees: default_overcut_angle(),
            overcut_reach_mm: default_overcut_reach_mm(),
            overcut_snap_to_pixels: true,
        }
    }
}

impl StudioDocument {
    pub fn new(kind: DocumentKind, canvas_size: Vec2, background: [u8; 3]) -> Self {
        Self {
            version: DOCUMENT_VERSION,
            kind,
            canvas_size: [canvas_size.x, canvas_size.y],
            background,
            images: Vec::new(),
            cut_paths: Vec::new(),
            cutline_metadata: Vec::new(),
            template_placeholders: Vec::new(),
            material: MaterialProfile::default(),
            settings: DocumentSettings::default(),
        }
    }

    pub fn to_json(&self) -> anyhow::Result<Vec<u8>> {
        serde_json::to_vec_pretty(self).context("could not encode studio document")
    }

    pub fn from_json(data: &[u8]) -> anyhow::Result<Self> {
        let mut document: Self =
            serde_json::from_slice(data).context("could not decode studio document")?;
        if document.version != DOCUMENT_VERSION {
            bail!(
                "unsupported studio document version {} (expected {})",
                document.version,
                DOCUMENT_VERSION
            );
        }
        document.ensure_object_ids();
        if document
            .canvas_size
            .iter()
            .any(|value| !value.is_finite() || *value <= 0.0)
        {
            bail!("studio document has an invalid canvas size");
        }
        if document.cut_paths.iter().any(|path| {
            path.len() < 2
                || path
                    .iter()
                    .flatten()
                    .any(|coordinate| !coordinate.is_finite())
        }) {
            bail!("studio document contains an invalid cut path");
        }
        if document.images.iter().any(|image| {
            image
                .offset
                .iter()
                .chain(image.scale.iter())
                .any(|value| !value.is_finite())
                || image.scale.iter().any(|value| *value <= 0.0)
                || !image.rotation_degrees.is_finite()
        }) {
            bail!("studio document contains an invalid image transform");
        }
        let mut relationship_ids = std::collections::HashSet::new();
        let mut relationship_indexes = std::collections::HashSet::new();
        if document.cutline_metadata.iter().any(|metadata| {
            metadata.id.is_empty()
                || !relationship_ids.insert(metadata.id.as_str())
                || !relationship_indexes.insert(metadata.cut_path_index)
                || metadata.cut_path_index >= document.cut_paths.len()
        }) {
            bail!("studio document contains invalid cutline metadata");
        }
        let image_ids = document
            .images
            .iter()
            .filter(|image| !image.id.is_empty())
            .map(|image| image.id.as_str())
            .collect::<std::collections::HashSet<_>>();
        if image_ids.len()
            != document
                .images
                .iter()
                .filter(|image| !image.id.is_empty())
                .count()
        {
            bail!("studio document contains duplicate image identifiers");
        }
        let placeholder_ids = document
            .template_placeholders
            .iter()
            .map(|placeholder| placeholder.id.as_str())
            .collect::<std::collections::HashSet<_>>();
        if placeholder_ids.len() != document.template_placeholders.len()
            || document.template_placeholders.iter().any(|placeholder| {
                placeholder.id.is_empty()
                    || placeholder.bounds.iter().any(|value| !value.is_finite())
                    || placeholder.bounds[2] <= 0.0
                    || placeholder.bounds[3] <= 0.0
                    || !placeholder.rotation_degrees.is_finite()
                    || placeholder
                        .assigned_image_id
                        .as_deref()
                        .is_some_and(|id| !image_ids.contains(id))
            })
        {
            bail!("studio document contains an invalid template placeholder");
        }
        if document.cutline_metadata.iter().any(|metadata| {
            metadata.owner.as_ref().is_some_and(|owner| match owner {
                CutlineOwner::Image(id) => !image_ids.contains(id.as_str()),
                CutlineOwner::TemplatePlaceholder(id) => !placeholder_ids.contains(id.as_str()),
            })
        }) {
            bail!("studio document contains a cutline with an unknown owner");
        }
        let settings = &document.settings;
        if [
            settings.cut_buffer,
            settings.cut_minimum_length,
            settings.cut_simplify,
            settings.perf_dash_mm,
            settings.perf_gap_mm,
            settings.pack_gap_mm,
            settings.overcut_maximum_angle_degrees,
            settings.overcut_reach_mm,
        ]
        .iter()
        .any(|value| !value.is_finite())
        {
            bail!("studio document contains invalid settings");
        }
        if !(1..=10).contains(&settings.copies)
            || settings.cut_smoothing > 10
            || !(-10_000.0..=10_000.0).contains(&settings.cut_buffer)
            || !(0.0..=100_000.0).contains(&settings.cut_minimum_length)
            || !(0.0..=5.0).contains(&settings.cut_simplify)
            || !(0.25..=8.0).contains(&settings.perf_dash_mm)
            || !(0.1..=4.0).contains(&settings.perf_gap_mm)
            || !(0.0..=10.0).contains(&settings.pack_gap_mm)
            || !(1..=12).contains(&settings.overcut_steps)
            || !(0.0..=90.0).contains(&settings.overcut_maximum_angle_degrees)
            || !(0.0..=10.0).contains(&settings.overcut_reach_mm)
            || document.material.name.trim().is_empty()
            || document.material.name.len() > 128
            || document.material.blade_pressure > 100
            || document.material.perf_pressure > 100
            || document.material.passes > 4
            || !(1..=10).contains(&document.material.speed)
        {
            bail!("studio document settings are outside supported bounds");
        }
        Ok(document)
    }

    /// Fill missing legacy IDs with deterministic, document-local identifiers.
    pub fn ensure_object_ids(&mut self) {
        let mut used = self
            .images
            .iter()
            .filter(|image| !image.id.is_empty())
            .map(|image| image.id.clone())
            .collect::<std::collections::HashSet<_>>();
        for (index, image) in self.images.iter_mut().enumerate() {
            if image.id.is_empty() {
                image.id = unique_document_id("image", index, &mut used);
            }
        }
        let mut used = self
            .cutline_metadata
            .iter()
            .filter(|metadata| !metadata.id.is_empty())
            .map(|metadata| metadata.id.clone())
            .collect::<std::collections::HashSet<_>>();
        for (index, metadata) in self.cutline_metadata.iter_mut().enumerate() {
            if metadata.id.is_empty() {
                metadata.id = unique_document_id("cutline", index, &mut used);
            }
        }
    }

    /// Create or update the explicit owner for a cut path.
    pub fn set_cutline_owner(
        &mut self,
        cut_path_index: usize,
        owner: Option<CutlineOwner>,
    ) -> anyhow::Result<()> {
        if cut_path_index >= self.cut_paths.len() {
            bail!("cut path index {cut_path_index} is out of bounds");
        }
        self.ensure_object_ids();
        if let Some(owner) = &owner {
            let exists = match owner {
                CutlineOwner::Image(id) => self.images.iter().any(|image| image.id == *id),
                CutlineOwner::TemplatePlaceholder(id) => self
                    .template_placeholders
                    .iter()
                    .any(|placeholder| placeholder.id == *id),
            };
            if !exists {
                bail!("cutline owner does not exist in the document");
            }
        }
        if let Some(metadata) = self
            .cutline_metadata
            .iter_mut()
            .find(|metadata| metadata.cut_path_index == cut_path_index)
        {
            metadata.owner = owner;
        } else {
            let mut used = self
                .cutline_metadata
                .iter()
                .map(|metadata| metadata.id.clone())
                .collect::<std::collections::HashSet<_>>();
            self.cutline_metadata.push(CutlineMetadata {
                id: unique_document_id("cutline", cut_path_index, &mut used),
                cut_path_index,
                owner,
                cut_mode: CutMode::Kiss,
                locked: false,
            });
        }
        Ok(())
    }

    /// Replace the artwork assigned to a template placeholder without changing
    /// its geometry or any cutlines owned by that placeholder.
    pub fn assign_placeholder_image(
        &mut self,
        placeholder_id: &str,
        image_id: Option<&str>,
    ) -> anyhow::Result<()> {
        if image_id.is_some_and(|id| !self.images.iter().any(|image| image.id == id)) {
            bail!("replacement image does not exist in the document");
        }
        let placeholder = self
            .template_placeholders
            .iter_mut()
            .find(|placeholder| placeholder.id == placeholder_id)
            .context("template placeholder does not exist in the document")?;
        placeholder.assigned_image_id = image_id.map(str::to_owned);
        Ok(())
    }

    pub fn extension(kind: DocumentKind) -> &'static str {
        match kind {
            DocumentKind::Sticker => "stix",
            DocumentKind::Sheet => "stixcut",
            DocumentKind::Template => "stixtpl",
        }
    }
}

fn unique_document_id(
    prefix: &str,
    index: usize,
    used: &mut std::collections::HashSet<String>,
) -> String {
    let mut suffix = index + 1;
    loop {
        let candidate = format!("{prefix}-{suffix}");
        if used.insert(candidate.clone()) {
            return candidate;
        }
        suffix += 1;
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MaterialProfile {
    pub name: String,
    /// Kiss-cut pressure.
    pub blade_pressure: u8,
    #[serde(default = "default_perf_pressure")]
    pub perf_pressure: u8,
    pub passes: u8,
    pub speed: u8,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ImageAdjustments {
    pub brightness: i32,
    pub contrast: f32,
    pub saturation: f32,
    pub hue_degrees: i32,
}

/// Apply non-destructive-preview style color controls to an RGBA image.
pub fn adjust_image(image: &RgbaImage, adjustment: ImageAdjustments) -> RgbaImage {
    let mut output = image::imageops::brighten(image, adjustment.brightness);
    output = image::imageops::contrast(&output, adjustment.contrast);
    output = image::imageops::huerotate(&output, adjustment.hue_degrees);
    if adjustment.saturation.abs() > f32::EPSILON {
        let factor = (1.0 + adjustment.saturation / 100.0).max(0.0);
        for pixel in output.pixels_mut() {
            let luminance = 0.2126 * f32::from(pixel[0])
                + 0.7152 * f32::from(pixel[1])
                + 0.0722 * f32::from(pixel[2]);
            for channel in &mut pixel.0[..3] {
                *channel = (luminance + (f32::from(*channel) - luminance) * factor)
                    .clamp(0.0, 255.0) as u8;
            }
        }
    }
    output
}

/// Rotate an image around its center, expanding the output so no pixels clip.
pub fn rotate_image(image: &RgbaImage, degrees: f32) -> RgbaImage {
    let normalized = degrees.rem_euclid(360.0);
    if normalized.abs() < 0.001 {
        return image.clone();
    }
    if (normalized - 90.0).abs() < 0.001 {
        return image::imageops::rotate90(image);
    }
    if (normalized - 180.0).abs() < 0.001 {
        return image::imageops::rotate180(image);
    }
    if (normalized - 270.0).abs() < 0.001 {
        return image::imageops::rotate270(image);
    }

    let radians = normalized.to_radians();
    let (sin, cos) = radians.sin_cos();
    let width = image.width() as f32;
    let height = image.height() as f32;
    let out_width = (width * cos.abs() + height * sin.abs()).ceil() as u32;
    let out_height = (width * sin.abs() + height * cos.abs()).ceil() as u32;
    let source_center = Vec2::new((width - 1.0) / 2.0, (height - 1.0) / 2.0);
    let target_center = Vec2::new(
        (out_width as f32 - 1.0) / 2.0,
        (out_height as f32 - 1.0) / 2.0,
    );
    RgbaImage::from_fn(out_width, out_height, |x, y| {
        let dx = x as f32 - target_center.x;
        let dy = y as f32 - target_center.y;
        let sx = cos * dx + sin * dy + source_center.x;
        let sy = -sin * dx + cos * dy + source_center.y;
        if sx >= 0.0 && sy >= 0.0 && sx < width && sy < height {
            *image.get_pixel(
                sx.round().clamp(0.0, width - 1.0) as u32,
                sy.round().clamp(0.0, height - 1.0) as u32,
            )
        } else {
            Rgba([0, 0, 0, 0])
        }
    })
}

impl Default for MaterialProfile {
    fn default() -> Self {
        Self {
            name: "Liene Sticker".into(),
            blade_pressure: 42,
            perf_pressure: default_perf_pressure(),
            passes: 1,
            speed: 5,
        }
    }
}

impl MaterialProfile {
    pub fn built_ins() -> Vec<Self> {
        vec![
            Self {
                name: "Liene Photo".into(),
                blade_pressure: 0,
                perf_pressure: 0,
                passes: 0,
                speed: 5,
            },
            Self::default(),
            Self {
                name: "Oracal 651 White".into(),
                blade_pressure: 55,
                perf_pressure: 65,
                passes: 1,
                speed: 4,
            },
            Self {
                name: "Oracal 651 Clear".into(),
                blade_pressure: 58,
                perf_pressure: 68,
                passes: 1,
                speed: 4,
            },
        ]
    }
}

fn default_perf_pressure() -> u8 {
    53
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PackItem {
    pub index: usize,
    pub size: Vec2,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PackedItem {
    pub index: usize,
    pub offset: Pos2,
    pub rotated: bool,
}

/// Deterministic MaxRects/Best-Short-Side-Fit packing for sticker sheets.
pub fn auto_pack(
    items: &[PackItem],
    bounds: Vec2,
    gap: f32,
    allow_rotation: bool,
) -> Vec<PackedItem> {
    #[derive(Clone, Copy, Debug)]
    struct FreeRect {
        x: f32,
        y: f32,
        w: f32,
        h: f32,
    }
    impl FreeRect {
        fn right(self) -> f32 {
            self.x + self.w
        }
        fn bottom(self) -> f32 {
            self.y + self.h
        }
        fn intersects(self, other: Self) -> bool {
            self.x < other.right()
                && self.right() > other.x
                && self.y < other.bottom()
                && self.bottom() > other.y
        }
        fn contains(self, other: Self) -> bool {
            other.x >= self.x
                && other.y >= self.y
                && other.right() <= self.right()
                && other.bottom() <= self.bottom()
        }
    }

    let mut sorted = items.to_vec();
    sorted.sort_by(|a, b| {
        (b.size.x * b.size.y)
            .total_cmp(&(a.size.x * a.size.y))
            .then(a.index.cmp(&b.index))
    });

    let gap = gap.max(0.0);
    let mut free = vec![FreeRect {
        x: 0.0,
        y: 0.0,
        w: bounds.x + gap,
        h: bounds.y + gap,
    }];
    let mut packed = Vec::new();

    for item in sorted {
        let orientations = if allow_rotation && item.size.x != item.size.y {
            [
                (item.size, false),
                (Vec2::new(item.size.y, item.size.x), true),
            ]
        } else {
            [(item.size, false), (item.size, false)]
        };

        let mut best: Option<(usize, Vec2, bool, f32, f32)> = None;
        for (rect_index, rect) in free.iter().copied().enumerate() {
            for (size, rotated) in orientations {
                let inflated = size + Vec2::splat(gap);
                if inflated.x <= rect.w && inflated.y <= rect.h {
                    let leftover_x = rect.w - inflated.x;
                    let leftover_y = rect.h - inflated.y;
                    let short = leftover_x.min(leftover_y);
                    let long = leftover_x.max(leftover_y);
                    if best.is_none_or(|candidate| (short, long) < (candidate.3, candidate.4)) {
                        best = Some((rect_index, size, rotated, short, long));
                    }
                }
            }
        }
        let Some((rect_index, size, rotated, _, _)) = best else {
            continue;
        };
        // Every candidate is anchored to its free rectangle's top-left.
        let chosen = free[rect_index];
        let used = FreeRect {
            x: chosen.x,
            y: chosen.y,
            w: size.x + gap,
            h: size.y + gap,
        };
        packed.push(PackedItem {
            index: item.index,
            offset: Pos2::new(chosen.x, chosen.y),
            rotated,
        });

        let mut split = Vec::new();
        for rect in free.drain(..) {
            if !rect.intersects(used) {
                split.push(rect);
                continue;
            }
            if used.x > rect.x {
                split.push(FreeRect {
                    x: rect.x,
                    y: rect.y,
                    w: used.x - rect.x,
                    h: rect.h,
                });
            }
            if used.right() < rect.right() {
                split.push(FreeRect {
                    x: used.right(),
                    y: rect.y,
                    w: rect.right() - used.right(),
                    h: rect.h,
                });
            }
            if used.y > rect.y {
                split.push(FreeRect {
                    x: rect.x,
                    y: rect.y,
                    w: rect.w,
                    h: used.y - rect.y,
                });
            }
            if used.bottom() < rect.bottom() {
                split.push(FreeRect {
                    x: rect.x,
                    y: used.bottom(),
                    w: rect.w,
                    h: rect.bottom() - used.bottom(),
                });
            }
        }
        split.retain(|rect| rect.w > 0.0 && rect.h > 0.0);
        free = split
            .iter()
            .copied()
            .enumerate()
            .filter_map(|(index, rect)| {
                (!split
                    .iter()
                    .enumerate()
                    .any(|(other_index, other)| index != other_index && other.contains(rect)))
                .then_some(rect)
            })
            .collect();
    }

    packed.sort_by_key(|item| item.index);
    packed
}

/// Make the edge-connected background transparent using a sampled corner color.
///
/// Unlike simply deleting every near-white pixel, flood fill preserves matching
/// colors enclosed inside the subject. `tolerance` is the maximum Euclidean RGB
/// distance (0–441); `feather` creates a soft transition outside that radius.
pub fn remove_background(image: &RgbaImage, tolerance: u16, feather: u16) -> RgbaImage {
    if image.width() == 0 || image.height() == 0 {
        return image.clone();
    }

    let corners = [
        image.get_pixel(0, 0),
        image.get_pixel(image.width() - 1, 0),
        image.get_pixel(0, image.height() - 1),
        image.get_pixel(image.width() - 1, image.height() - 1),
    ];
    let background = [
        (corners.iter().map(|p| u32::from(p[0])).sum::<u32>() / 4) as u8,
        (corners.iter().map(|p| u32::from(p[1])).sum::<u32>() / 4) as u8,
        (corners.iter().map(|p| u32::from(p[2])).sum::<u32>() / 4) as u8,
    ];
    let distance = |p: &Rgba<u8>| -> u16 {
        let dr = i32::from(p[0]) - i32::from(background[0]);
        let dg = i32::from(p[1]) - i32::from(background[1]);
        let db = i32::from(p[2]) - i32::from(background[2]);
        ((dr * dr + dg * dg + db * db) as f32).sqrt() as u16
    };

    let width = image.width() as usize;
    let height = image.height() as usize;
    let mut visited = vec![false; width * height];
    let mut queue = VecDeque::new();
    for x in 0..image.width() {
        queue.push_back((x, 0));
        queue.push_back((x, image.height() - 1));
    }
    for y in 0..image.height() {
        queue.push_back((0, y));
        queue.push_back((image.width() - 1, y));
    }

    let limit = tolerance.saturating_add(feather);
    while let Some((x, y)) = queue.pop_front() {
        let index = y as usize * width + x as usize;
        if visited[index] || distance(image.get_pixel(x, y)) > limit {
            continue;
        }
        visited[index] = true;
        if x > 0 {
            queue.push_back((x - 1, y));
        }
        if x + 1 < image.width() {
            queue.push_back((x + 1, y));
        }
        if y > 0 {
            queue.push_back((x, y - 1));
        }
        if y + 1 < image.height() {
            queue.push_back((x, y + 1));
        }
    }

    let mut result = image.clone();
    for y in 0..image.height() {
        for x in 0..image.width() {
            if !visited[y as usize * width + x as usize] {
                continue;
            }
            let pixel = result.get_pixel_mut(x, y);
            let d = distance(pixel);
            pixel[3] = if d <= tolerance || feather == 0 {
                0
            } else {
                (((d - tolerance) as f32 / feather as f32) * f32::from(pixel[3])) as u8
            };
        }
    }
    result
}

/// Parse SVG path data into cuttable polylines.
///
/// Lines, cubic/quadratic curves, smooth curves, and elliptical arcs are
/// supported. Curves are sampled into a configurable number of straight
/// segments so editing and device output stay deterministic.
#[allow(dead_code)]
pub fn parse_svg_path(data: &str, curve_steps: usize) -> anyhow::Result<Vec<LineString<f32>>> {
    parse_svg_path_sampling(data, CurveSampling::Fixed(curve_steps.max(2)))
}

/// Parse SVG path data with deterministic adaptive subdivision.
///
/// Every Bézier subdivision is bounded by `tolerance` in SVG user units. Arc
/// sampling uses the same tolerance as a maximum radial sagitta.
#[allow(dead_code)]
pub fn parse_svg_path_with_tolerance(
    data: &str,
    tolerance: f32,
) -> anyhow::Result<Vec<LineString<f32>>> {
    if !tolerance.is_finite() || tolerance <= 0.0 {
        bail!("SVG curve tolerance must be finite and greater than zero");
    }
    parse_svg_path_sampling(data, CurveSampling::Tolerance(tolerance))
}

#[allow(dead_code)]
#[derive(Clone, Copy)]
enum CurveSampling {
    Fixed(usize),
    Tolerance(f32),
}

fn parse_svg_path_sampling(
    data: &str,
    sampling: CurveSampling,
) -> anyhow::Result<Vec<LineString<f32>>> {
    let mut paths = Vec::new();
    let mut points: Vec<Coord<f32>> = Vec::new();
    let mut current = Coord { x: 0.0, y: 0.0 };
    let mut start = current;
    let mut cubic_control: Option<Coord<f32>> = None;
    let mut quadratic_control: Option<Coord<f32>> = None;

    let finish = |points: &mut Vec<Coord<f32>>, paths: &mut Vec<LineString<f32>>| {
        if points.len() >= 2 {
            paths.push(LineString::new(std::mem::take(points)));
        } else {
            points.clear();
        }
    };

    for segment in PathParser::from(data) {
        match segment.context("invalid SVG path data")? {
            PathSegment::MoveTo { abs, x, y } => {
                finish(&mut points, &mut paths);
                current = absolute(abs, current, x, y);
                start = current;
                points.push(current);
                cubic_control = None;
                quadratic_control = None;
            }
            PathSegment::LineTo { abs, x, y } => {
                ensure_contour(&mut points, current);
                current = absolute(abs, current, x, y);
                points.push(current);
                cubic_control = None;
                quadratic_control = None;
            }
            PathSegment::HorizontalLineTo { abs, x } => {
                ensure_contour(&mut points, current);
                current.x = if abs { x as f32 } else { current.x + x as f32 };
                points.push(current);
                cubic_control = None;
                quadratic_control = None;
            }
            PathSegment::VerticalLineTo { abs, y } => {
                ensure_contour(&mut points, current);
                current.y = if abs { y as f32 } else { current.y + y as f32 };
                points.push(current);
                cubic_control = None;
                quadratic_control = None;
            }
            PathSegment::CurveTo {
                abs,
                x1,
                y1,
                x2,
                y2,
                x,
                y,
            } => {
                ensure_contour(&mut points, current);
                let p0 = current;
                let p1 = absolute(abs, current, x1, y1);
                let p2 = absolute(abs, current, x2, y2);
                let p3 = absolute(abs, current, x, y);
                sample_cubic_mode(&mut points, p0, p1, p2, p3, sampling);
                current = p3;
                cubic_control = Some(p2);
                quadratic_control = None;
            }
            PathSegment::SmoothCurveTo { abs, x2, y2, x, y } => {
                ensure_contour(&mut points, current);
                let p0 = current;
                let p1 = cubic_control
                    .map(|control| reflect(control, current))
                    .unwrap_or(current);
                let p2 = absolute(abs, current, x2, y2);
                let p3 = absolute(abs, current, x, y);
                sample_cubic_mode(&mut points, p0, p1, p2, p3, sampling);
                current = p3;
                cubic_control = Some(p2);
                quadratic_control = None;
            }
            PathSegment::Quadratic { abs, x1, y1, x, y } => {
                ensure_contour(&mut points, current);
                let p0 = current;
                let p1 = absolute(abs, current, x1, y1);
                let p2 = absolute(abs, current, x, y);
                sample_quadratic_mode(&mut points, p0, p1, p2, sampling);
                current = p2;
                cubic_control = None;
                quadratic_control = Some(p1);
            }
            PathSegment::SmoothQuadratic { abs, x, y } => {
                ensure_contour(&mut points, current);
                let p0 = current;
                let p1 = quadratic_control
                    .map(|control| reflect(control, current))
                    .unwrap_or(current);
                let p2 = absolute(abs, current, x, y);
                sample_quadratic_mode(&mut points, p0, p1, p2, sampling);
                current = p2;
                cubic_control = None;
                quadratic_control = Some(p1);
            }
            PathSegment::EllipticalArc {
                abs,
                rx,
                ry,
                x_axis_rotation,
                large_arc,
                sweep,
                x,
                y,
            } => {
                ensure_contour(&mut points, current);
                let end = absolute(abs, current, x, y);
                sample_arc(
                    &mut points,
                    current,
                    end,
                    rx,
                    ry,
                    x_axis_rotation,
                    large_arc,
                    sweep,
                    sampling,
                );
                current = end;
                cubic_control = None;
                quadratic_control = None;
            }
            PathSegment::ClosePath { .. } => {
                if current != start {
                    points.push(start);
                }
                current = start;
                finish(&mut points, &mut paths);
                cubic_control = None;
                quadratic_control = None;
            }
        }
    }
    finish(&mut points, &mut paths);
    Ok(paths)
}

/// Import SVG path and basic-shape geometry in document order.
///
/// The root `viewBox`, rendered `width`/`height`, `preserveAspectRatio`, and
/// transforms on every ancestor are composed before geometry is returned.
pub fn parse_svg(data: &str, curve_steps: usize) -> anyhow::Result<Vec<LineString<f32>>> {
    parse_svg_sampling(data, CurveSampling::Fixed(curve_steps.max(2)))
}

/// Import an SVG using deterministic, tolerance-aware curve subdivision.
#[allow(dead_code)]
pub fn parse_svg_with_tolerance(
    data: &str,
    tolerance: f32,
) -> anyhow::Result<Vec<LineString<f32>>> {
    if !tolerance.is_finite() || tolerance <= 0.0 {
        bail!("SVG curve tolerance must be finite and greater than zero");
    }
    parse_svg_sampling(data, CurveSampling::Tolerance(tolerance))
}

fn parse_svg_sampling(data: &str, sampling: CurveSampling) -> anyhow::Result<Vec<LineString<f32>>> {
    let document = roxmltree::Document::parse(data).context("invalid SVG document")?;
    let root = document.root_element();
    if root.tag_name().name() != "svg" {
        bail!("SVG document root is not an <svg> element");
    }
    let viewport_transform = root_viewport_transform(root)?;
    let viewport_user_size = root_user_size(root)?;
    let mut paths = Vec::new();
    for node in document.descendants().filter(|node| node.is_element()) {
        let name = node.tag_name().name();
        let hidden_definition = node.ancestors().skip(1).any(|ancestor| {
            matches!(
                ancestor.tag_name().name(),
                "defs" | "symbol" | "clipPath" | "mask" | "marker" | "pattern"
            )
        });
        if name == "use" && !hidden_definition {
            let href = node
                .attribute("href")
                .or_else(|| node.attribute(("http://www.w3.org/1999/xlink", "href")));
            let Some(reference) = href.and_then(|href| href.strip_prefix('#')) else {
                continue;
            };
            let Some(target) = document
                .descendants()
                .find(|candidate| candidate.attribute("id") == Some(reference))
            else {
                continue;
            };
            let mut transform = accumulated_transform(node, viewport_transform)?;
            let x = svg_length(node, "x", 0.0, viewport_user_size.0)?;
            let y = svg_length(node, "y", 0.0, viewport_user_size.1)?;
            transform = multiply_transform(transform, Transform::new(1.0, 0.0, 0.0, 1.0, x, y));
            collect_svg_instance(target, transform, sampling, viewport_user_size, &mut paths)?;
            continue;
        }
        if !matches!(
            name,
            "path" | "rect" | "circle" | "ellipse" | "line" | "polyline" | "polygon"
        ) || hidden_definition
        {
            continue;
        }
        if node
            .ancestors()
            .any(|ancestor| ancestor.attribute("display") == Some("none"))
        {
            continue;
        }
        let transform = accumulated_transform(node, viewport_transform)?;
        for mut path in basic_shape_paths(node, sampling, viewport_user_size)? {
            for point in &mut path.0 {
                *point = transform_coord(transform, *point);
            }
            paths.push(path);
        }
    }
    if paths.is_empty() {
        bail!("SVG does not contain any supported geometry elements");
    }
    Ok(paths)
}

fn accumulated_transform(
    node: roxmltree::Node<'_, '_>,
    initial: Transform,
) -> anyhow::Result<Transform> {
    node.ancestors()
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .try_fold(initial, |combined, ancestor| {
            let Some(value) = ancestor.attribute("transform") else {
                return Ok(combined);
            };
            let local: Transform = value.parse().context("invalid SVG transform")?;
            Ok(multiply_transform(combined, local))
        })
}

fn collect_svg_instance(
    node: roxmltree::Node<'_, '_>,
    parent_transform: Transform,
    sampling: CurveSampling,
    viewport: (f64, f64),
    paths: &mut Vec<LineString<f32>>,
) -> anyhow::Result<()> {
    if node.attribute("display") == Some("none") {
        return Ok(());
    }
    let transform = if let Some(value) = node.attribute("transform") {
        multiply_transform(
            parent_transform,
            value.parse().context("invalid SVG transform")?,
        )
    } else {
        parent_transform
    };
    if matches!(
        node.tag_name().name(),
        "path" | "rect" | "circle" | "ellipse" | "line" | "polyline" | "polygon"
    ) {
        for mut path in basic_shape_paths(node, sampling, viewport)? {
            for point in &mut path.0 {
                *point = transform_coord(transform, *point);
            }
            paths.push(path);
        }
    } else {
        for child in node.children().filter(|child| child.is_element()) {
            collect_svg_instance(child, transform, sampling, viewport, paths)?;
        }
    }
    Ok(())
}

fn basic_shape_paths(
    node: roxmltree::Node<'_, '_>,
    sampling: CurveSampling,
    viewport: (f64, f64),
) -> anyhow::Result<Vec<LineString<f32>>> {
    let x = |name: &str, default: f64| svg_length(node, name, default, viewport.0);
    let y = |name: &str, default: f64| svg_length(node, name, default, viewport.1);
    let radial_basis = (viewport.0.hypot(viewport.1)) / std::f64::consts::SQRT_2;
    let radial = |name: &str, default: f64| svg_length(node, name, default, radial_basis);
    let path = match node.tag_name().name() {
        "path" => {
            let Some(data) = node.attribute("d") else {
                return Ok(Vec::new());
            };
            return parse_svg_path_sampling(data, sampling);
        }
        "line" => LineString::new(vec![
            coord(x("x1", 0.0)?, y("y1", 0.0)?),
            coord(x("x2", 0.0)?, y("y2", 0.0)?),
        ]),
        "polyline" | "polygon" => {
            let Some(value) = node.attribute("points") else {
                return Ok(Vec::new());
            };
            let numbers = NumberListParser::from(value)
                .collect::<Result<Vec<_>, _>>()
                .context("invalid SVG points list")?;
            if numbers.len() % 2 != 0 {
                bail!("SVG points list contains an unmatched coordinate");
            }
            let mut points = numbers
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| coord(pair[0], pair[1]))
                .collect::<Vec<_>>();
            if points.len() < 2 {
                return Ok(Vec::new());
            }
            if node.tag_name().name() == "polygon" && points.first() != points.last() {
                points.push(points[0]);
            }
            LineString::new(points)
        }
        "circle" => {
            let cx = x("cx", 0.0)?;
            let cy = y("cy", 0.0)?;
            let radius = radial("r", 0.0)?;
            if radius <= 0.0 {
                return Ok(Vec::new());
            }
            sampled_ellipse(cx, cy, radius, radius, sampling)
        }
        "ellipse" => {
            let cx = x("cx", 0.0)?;
            let cy = y("cy", 0.0)?;
            let rx = x("rx", 0.0)?;
            let ry = y("ry", 0.0)?;
            if rx <= 0.0 || ry <= 0.0 {
                return Ok(Vec::new());
            }
            sampled_ellipse(cx, cy, rx, ry, sampling)
        }
        "rect" => {
            let left = x("x", 0.0)?;
            let top = y("y", 0.0)?;
            let width = x("width", 0.0)?;
            let height = y("height", 0.0)?;
            if width <= 0.0 || height <= 0.0 {
                return Ok(Vec::new());
            }
            let rx_attribute = node.attribute("rx");
            let ry_attribute = node.attribute("ry");
            let rx = match (rx_attribute, ry_attribute) {
                (Some(_), _) => x("rx", 0.0)?,
                (None, Some(_)) => y("ry", 0.0)?,
                (None, None) => 0.0,
            }
            .clamp(0.0, width / 2.0);
            let ry = match (rx_attribute, ry_attribute) {
                (_, Some(_)) => y("ry", 0.0)?,
                (Some(_), None) => x("rx", 0.0)?,
                (None, None) => 0.0,
            }
            .clamp(0.0, height / 2.0);
            sampled_rect(left, top, width, height, rx, ry, sampling)
        }
        _ => return Ok(Vec::new()),
    };
    Ok((path.0.len() >= 2).then_some(path).into_iter().collect())
}

fn coord(x: f64, y: f64) -> Coord<f32> {
    Coord {
        x: x as f32,
        y: y as f32,
    }
}

fn svg_length(
    node: roxmltree::Node<'_, '_>,
    name: &str,
    default: f64,
    percent_basis: f64,
) -> anyhow::Result<f64> {
    let Some(value) = node.attribute(name) else {
        return Ok(default);
    };
    let length: Length = value
        .parse()
        .with_context(|| format!("invalid SVG {name} length"))?;
    let pixels = match length.unit {
        LengthUnit::None | LengthUnit::Px => length.number,
        LengthUnit::In => length.number * 96.0,
        LengthUnit::Cm => length.number * 96.0 / 2.54,
        LengthUnit::Mm => length.number * 96.0 / 25.4,
        LengthUnit::Pt => length.number * 96.0 / 72.0,
        LengthUnit::Pc => length.number * 16.0,
        LengthUnit::Percent => length.number * percent_basis / 100.0,
        LengthUnit::Em | LengthUnit::Ex => bail!("SVG font-relative lengths are not supported"),
    };
    if !pixels.is_finite() {
        bail!("SVG {name} length is not finite");
    }
    Ok(pixels)
}

fn root_user_size(root: roxmltree::Node<'_, '_>) -> anyhow::Result<(f64, f64)> {
    if let Some(value) = root.attribute("viewBox") {
        let view_box: ViewBox = value.parse().context("invalid SVG viewBox")?;
        return Ok((view_box.w, view_box.h));
    }
    Ok((
        svg_length(root, "width", 300.0, 300.0)?,
        svg_length(root, "height", 150.0, 150.0)?,
    ))
}

fn root_viewport_transform(root: roxmltree::Node<'_, '_>) -> anyhow::Result<Transform> {
    let Some(value) = root.attribute("viewBox") else {
        return Ok(Transform::default());
    };
    let view_box: ViewBox = value.parse().context("invalid SVG viewBox")?;
    let width = svg_length(root, "width", view_box.w, view_box.w)?;
    let height = svg_length(root, "height", view_box.h, view_box.h)?;
    if width <= 0.0 || height <= 0.0 {
        bail!("SVG viewport has a negative or zero size");
    }
    let aspect: AspectRatio = root
        .attribute("preserveAspectRatio")
        .unwrap_or("xMidYMid meet")
        .parse()
        .context("invalid SVG preserveAspectRatio")?;
    let mut sx = width / view_box.w;
    let mut sy = height / view_box.h;
    let (offset_x, offset_y) = if aspect.align == Align::None {
        (0.0, 0.0)
    } else {
        let uniform = if aspect.slice { sx.max(sy) } else { sx.min(sy) };
        sx = uniform;
        sy = uniform;
        let spare_x = width - view_box.w * uniform;
        let spare_y = height - view_box.h * uniform;
        alignment_offset(aspect.align, spare_x, spare_y)
    };
    Ok(Transform::new(
        sx,
        0.0,
        0.0,
        sy,
        offset_x - view_box.x * sx,
        offset_y - view_box.y * sy,
    ))
}

fn alignment_offset(align: Align, spare_x: f64, spare_y: f64) -> (f64, f64) {
    let x = match align {
        Align::XMinYMin | Align::XMinYMid | Align::XMinYMax | Align::None => 0.0,
        Align::XMidYMin | Align::XMidYMid | Align::XMidYMax => spare_x / 2.0,
        Align::XMaxYMin | Align::XMaxYMid | Align::XMaxYMax => spare_x,
    };
    let y = match align {
        Align::XMinYMin | Align::XMidYMin | Align::XMaxYMin | Align::None => 0.0,
        Align::XMinYMid | Align::XMidYMid | Align::XMaxYMid => spare_y / 2.0,
        Align::XMinYMax | Align::XMidYMax | Align::XMaxYMax => spare_y,
    };
    (x, y)
}

fn sampled_ellipse(cx: f64, cy: f64, rx: f64, ry: f64, sampling: CurveSampling) -> LineString<f32> {
    let count = match sampling {
        CurveSampling::Fixed(steps) => steps.saturating_mul(4),
        CurveSampling::Tolerance(tolerance) => {
            segments_for_arc(rx.max(ry), std::f64::consts::TAU, tolerance)
        }
    }
    .clamp(8, 65_536);
    let mut points = Vec::with_capacity(count + 1);
    for step in 0..=count {
        let angle = std::f64::consts::TAU * step as f64 / count as f64;
        points.push(coord(cx + rx * angle.cos(), cy + ry * angle.sin()));
    }
    LineString::new(points)
}

fn sampled_rect(
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    rx: f64,
    ry: f64,
    sampling: CurveSampling,
) -> LineString<f32> {
    if rx == 0.0 || ry == 0.0 {
        return LineString::new(vec![
            coord(x, y),
            coord(x + width, y),
            coord(x + width, y + height),
            coord(x, y + height),
            coord(x, y),
        ]);
    }
    let steps = match sampling {
        CurveSampling::Fixed(steps) => steps,
        CurveSampling::Tolerance(tolerance) => {
            segments_for_arc(rx.max(ry), std::f64::consts::FRAC_PI_2, tolerance)
        }
    }
    .clamp(1, 16_384);
    let mut points = Vec::with_capacity(steps.saturating_mul(4) + 5);
    let corners = [
        (x + width - rx, y + ry, -std::f64::consts::FRAC_PI_2),
        (x + width - rx, y + height - ry, 0.0),
        (x + rx, y + height - ry, std::f64::consts::FRAC_PI_2),
        (x + rx, y + ry, std::f64::consts::PI),
    ];
    points.push(coord(x + rx, y));
    for (cx, cy, start_angle) in corners {
        for step in 0..=steps {
            let angle = start_angle + std::f64::consts::FRAC_PI_2 * step as f64 / steps as f64;
            let point = coord(cx + rx * angle.cos(), cy + ry * angle.sin());
            if points.last() != Some(&point) {
                points.push(point);
            }
        }
    }
    if points.first() != points.last() {
        points.push(points[0]);
    }
    LineString::new(points)
}

fn segments_for_arc(radius: f64, sweep_angle: f64, tolerance: f32) -> usize {
    if radius <= 0.0 || f64::from(tolerance) >= radius {
        return 1;
    }
    let cosine = (1.0 - f64::from(tolerance) / radius).clamp(-1.0, 1.0);
    let max_angle = (2.0 * cosine.acos()).max(1.0e-6);
    (sweep_angle.abs() / max_angle).ceil() as usize
}

fn sample_cubic(
    points: &mut Vec<Coord<f32>>,
    p0: Coord<f32>,
    p1: Coord<f32>,
    p2: Coord<f32>,
    p3: Coord<f32>,
    steps: usize,
) {
    for step in 1..=steps {
        let t = step as f32 / steps as f32;
        let mt = 1.0 - t;
        points.push(Coord {
            x: mt.powi(3) * p0.x
                + 3.0 * mt.powi(2) * t * p1.x
                + 3.0 * mt * t * t * p2.x
                + t.powi(3) * p3.x,
            y: mt.powi(3) * p0.y
                + 3.0 * mt.powi(2) * t * p1.y
                + 3.0 * mt * t * t * p2.y
                + t.powi(3) * p3.y,
        });
    }
}

fn sample_cubic_mode(
    points: &mut Vec<Coord<f32>>,
    p0: Coord<f32>,
    p1: Coord<f32>,
    p2: Coord<f32>,
    p3: Coord<f32>,
    sampling: CurveSampling,
) {
    match sampling {
        CurveSampling::Fixed(steps) => sample_cubic(points, p0, p1, p2, p3, steps),
        CurveSampling::Tolerance(tolerance) => {
            sample_cubic_adaptive(points, p0, p1, p2, p3, tolerance, 0)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn sample_cubic_adaptive(
    points: &mut Vec<Coord<f32>>,
    p0: Coord<f32>,
    p1: Coord<f32>,
    p2: Coord<f32>,
    p3: Coord<f32>,
    tolerance: f32,
    depth: u8,
) {
    if depth >= 20
        || (point_line_distance(p1, p0, p3) <= tolerance
            && point_line_distance(p2, p0, p3) <= tolerance)
    {
        points.push(p3);
        return;
    }
    let p01 = midpoint(p0, p1);
    let p12 = midpoint(p1, p2);
    let p23 = midpoint(p2, p3);
    let p012 = midpoint(p01, p12);
    let p123 = midpoint(p12, p23);
    let split = midpoint(p012, p123);
    sample_cubic_adaptive(points, p0, p01, p012, split, tolerance, depth + 1);
    sample_cubic_adaptive(points, split, p123, p23, p3, tolerance, depth + 1);
}

fn ensure_contour(points: &mut Vec<Coord<f32>>, current: Coord<f32>) {
    if points.is_empty() {
        points.push(current);
    }
}

fn sample_quadratic(
    points: &mut Vec<Coord<f32>>,
    p0: Coord<f32>,
    p1: Coord<f32>,
    p2: Coord<f32>,
    steps: usize,
) {
    for step in 1..=steps {
        let t = step as f32 / steps as f32;
        let mt = 1.0 - t;
        points.push(Coord {
            x: mt * mt * p0.x + 2.0 * mt * t * p1.x + t * t * p2.x,
            y: mt * mt * p0.y + 2.0 * mt * t * p1.y + t * t * p2.y,
        });
    }
}

fn sample_quadratic_mode(
    points: &mut Vec<Coord<f32>>,
    p0: Coord<f32>,
    p1: Coord<f32>,
    p2: Coord<f32>,
    sampling: CurveSampling,
) {
    match sampling {
        CurveSampling::Fixed(steps) => sample_quadratic(points, p0, p1, p2, steps),
        CurveSampling::Tolerance(tolerance) => {
            sample_quadratic_adaptive(points, p0, p1, p2, tolerance, 0)
        }
    }
}

fn sample_quadratic_adaptive(
    points: &mut Vec<Coord<f32>>,
    p0: Coord<f32>,
    p1: Coord<f32>,
    p2: Coord<f32>,
    tolerance: f32,
    depth: u8,
) {
    if depth >= 20 || point_line_distance(p1, p0, p2) <= tolerance {
        points.push(p2);
        return;
    }
    let p01 = midpoint(p0, p1);
    let p12 = midpoint(p1, p2);
    let split = midpoint(p01, p12);
    sample_quadratic_adaptive(points, p0, p01, split, tolerance, depth + 1);
    sample_quadratic_adaptive(points, split, p12, p2, tolerance, depth + 1);
}

fn midpoint(left: Coord<f32>, right: Coord<f32>) -> Coord<f32> {
    Coord {
        x: (left.x + right.x) / 2.0,
        y: (left.y + right.y) / 2.0,
    }
}

fn point_line_distance(point: Coord<f32>, start: Coord<f32>, end: Coord<f32>) -> f32 {
    let dx = end.x - start.x;
    let dy = end.y - start.y;
    let length = dx.hypot(dy);
    if length <= f32::EPSILON {
        return (point.x - start.x).hypot(point.y - start.y);
    }
    ((point.x - start.x) * dy - (point.y - start.y) * dx).abs() / length
}

fn reflect(control: Coord<f32>, around: Coord<f32>) -> Coord<f32> {
    Coord {
        x: 2.0 * around.x - control.x,
        y: 2.0 * around.y - control.y,
    }
}

#[allow(clippy::too_many_arguments)]
fn sample_arc(
    points: &mut Vec<Coord<f32>>,
    start: Coord<f32>,
    end: Coord<f32>,
    rx: f64,
    ry: f64,
    rotation_degrees: f64,
    large_arc: bool,
    sweep: bool,
    sampling: CurveSampling,
) {
    if start == end {
        return;
    }
    let (mut rx, mut ry) = (rx.abs(), ry.abs());
    if rx == 0.0 || ry == 0.0 {
        points.push(end);
        return;
    }

    // SVG 2 endpoint-to-center arc conversion.
    let phi = rotation_degrees.rem_euclid(360.0).to_radians();
    let (sin_phi, cos_phi) = phi.sin_cos();
    let half_dx = (f64::from(start.x) - f64::from(end.x)) / 2.0;
    let half_dy = (f64::from(start.y) - f64::from(end.y)) / 2.0;
    let x1 = cos_phi * half_dx + sin_phi * half_dy;
    let y1 = -sin_phi * half_dx + cos_phi * half_dy;
    let radius_scale = x1 * x1 / (rx * rx) + y1 * y1 / (ry * ry);
    if radius_scale > 1.0 {
        let scale = radius_scale.sqrt();
        rx *= scale;
        ry *= scale;
    }

    let rx2 = rx * rx;
    let ry2 = ry * ry;
    let x12 = x1 * x1;
    let y12 = y1 * y1;
    let denominator = rx2 * y12 + ry2 * x12;
    let coefficient = if denominator == 0.0 {
        0.0
    } else {
        let sign = if large_arc == sweep { -1.0 } else { 1.0 };
        sign * ((rx2 * ry2 - rx2 * y12 - ry2 * x12) / denominator)
            .max(0.0)
            .sqrt()
    };
    let center_x_local = coefficient * rx * y1 / ry;
    let center_y_local = coefficient * -ry * x1 / rx;
    let center_x = cos_phi * center_x_local - sin_phi * center_y_local
        + (f64::from(start.x) + f64::from(end.x)) / 2.0;
    let center_y = sin_phi * center_x_local
        + cos_phi * center_y_local
        + (f64::from(start.y) + f64::from(end.y)) / 2.0;

    let start_vector = ((x1 - center_x_local) / rx, (y1 - center_y_local) / ry);
    let end_vector = ((-x1 - center_x_local) / rx, (-y1 - center_y_local) / ry);
    let vector_angle =
        |u: (f64, f64), v: (f64, f64)| (u.0 * v.1 - u.1 * v.0).atan2(u.0 * v.0 + u.1 * v.1);
    let start_angle = vector_angle((1.0, 0.0), start_vector);
    let mut sweep_angle = vector_angle(start_vector, end_vector);
    if !sweep && sweep_angle > 0.0 {
        sweep_angle -= std::f64::consts::TAU;
    } else if sweep && sweep_angle < 0.0 {
        sweep_angle += std::f64::consts::TAU;
    }

    let steps = match sampling {
        CurveSampling::Fixed(steps) => steps,
        CurveSampling::Tolerance(tolerance) => segments_for_arc(rx.max(ry), sweep_angle, tolerance),
    }
    .clamp(1, 65_536);
    for step in 1..=steps {
        let angle = start_angle + sweep_angle * step as f64 / steps as f64;
        let (sin_angle, cos_angle) = angle.sin_cos();
        points.push(Coord {
            x: (center_x + cos_phi * rx * cos_angle - sin_phi * ry * sin_angle) as f32,
            y: (center_y + sin_phi * rx * cos_angle + cos_phi * ry * sin_angle) as f32,
        });
    }
    // Avoid tiny endpoint drift from trigonometry.
    if let Some(last) = points.last_mut() {
        *last = end;
    }
}

fn multiply_transform(left: Transform, right: Transform) -> Transform {
    Transform::new(
        left.a * right.a + left.c * right.b,
        left.b * right.a + left.d * right.b,
        left.a * right.c + left.c * right.d,
        left.b * right.c + left.d * right.d,
        left.a * right.e + left.c * right.f + left.e,
        left.b * right.e + left.d * right.f + left.f,
    )
}

fn transform_coord(transform: Transform, point: Coord<f32>) -> Coord<f32> {
    Coord {
        x: (transform.a * f64::from(point.x) + transform.c * f64::from(point.y) + transform.e)
            as f32,
        y: (transform.b * f64::from(point.x) + transform.d * f64::from(point.y) + transform.f)
            as f32,
    }
}

fn absolute(abs: bool, current: Coord<f32>, x: f64, y: f64) -> Coord<f32> {
    if abs {
        Coord {
            x: x as f32,
            y: y as f32,
        }
    } else {
        Coord {
            x: current.x + x as f32,
            y: current.y + y as f32,
        }
    }
}

/// Split a contour into alternating cut dashes for perf-cut output.
pub fn perf_cut(path: &LineString<f32>, dash: f32, gap: f32) -> Vec<LineString<f32>> {
    let mut output = Vec::new();
    if dash <= 0.0 || path.0.len() < 2 {
        return output;
    }
    let period = dash + gap.max(0.0);
    let mut distance_on_path: f32 = 0.0;
    for segment in path.lines() {
        let dx = segment.end.x - segment.start.x;
        let dy = segment.end.y - segment.start.y;
        let length = dx.hypot(dy);
        if length == 0.0 {
            continue;
        }
        let mut local = 0.0;
        while local < length {
            let phase = distance_on_path.rem_euclid(period);
            let cutting = phase < dash;
            let until_change = if cutting {
                dash - phase
            } else {
                period - phase
            };
            let end = (local + until_change).min(length);
            if cutting && end > local {
                let point = |at: f32| Coord {
                    x: segment.start.x + dx * at / length,
                    y: segment.start.y + dy * at / length,
                };
                output.push(LineString::new(vec![point(local), point(end)]));
            }
            let consumed = end - local;
            local = end;
            distance_on_path += consumed;
            if consumed <= f32::EPSILON {
                break;
            }
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn document_round_trip_and_version_check() {
        let mut document =
            StudioDocument::new(DocumentKind::Sticker, Vec2::new(12.0, 34.0), [1, 2, 3]);
        document.images.push(SavedImage::from_png(
            "a",
            b"png",
            Pos2::new(2.0, 3.0),
            Vec2::ONE,
            15.0,
            true,
            false,
            true,
        ));
        document.ensure_object_ids();
        assert_eq!(
            StudioDocument::from_json(&document.to_json().unwrap()).unwrap(),
            document
        );
        let bad = document.to_json().unwrap();
        let bad = String::from_utf8(bad)
            .unwrap()
            .replace("\"version\": 1", "\"version\": 99");
        assert!(StudioDocument::from_json(bad.as_bytes()).is_err());
        let mut invalid = document;
        invalid.cut_paths.push(vec![[0.0, 0.0]]);
        assert!(StudioDocument::from_json(&serde_json::to_vec(&invalid).unwrap()).is_err());
    }

    #[test]
    fn legacy_documents_default_new_relationship_fields() {
        let legacy = br#"{
            "version":1,"kind":"sheet","canvas_size":[10.0,20.0],
            "background":[255,255,255],"images":[],"cut_paths":[],
            "material":{"name":"Vinyl","blade_pressure":10,"passes":1,"speed":5}
        }"#;
        let document = StudioDocument::from_json(legacy).unwrap();
        assert!(document.cutline_metadata.is_empty());
        assert!(document.template_placeholders.is_empty());
    }

    #[test]
    fn cutline_ownership_and_placeholder_replacement_round_trip() {
        let mut document =
            StudioDocument::new(DocumentKind::Template, Vec2::new(100.0, 100.0), [255; 3]);
        document.images.push(SavedImage::from_png(
            "art",
            b"png",
            Pos2::ZERO,
            Vec2::ONE,
            0.0,
            true,
            false,
            true,
        ));
        document.ensure_object_ids();
        let image_id = document.images[0].id.clone();
        document.template_placeholders.push(TemplatePlaceholder {
            id: "hero".into(),
            name: "Hero artwork".into(),
            bounds: [10.0, 20.0, 30.0, 40.0],
            rotation_degrees: 0.0,
            fit: PlaceholderFit::Contain,
            assigned_image_id: None,
        });
        document.cut_paths.push(vec![[0.0, 0.0], [1.0, 1.0]]);
        document
            .set_cutline_owner(0, Some(CutlineOwner::TemplatePlaceholder("hero".into())))
            .unwrap();
        document.cutline_metadata[0].locked = true;
        document
            .assign_placeholder_image("hero", Some(&image_id))
            .unwrap();

        let decoded = StudioDocument::from_json(&document.to_json().unwrap()).unwrap();
        assert_eq!(decoded, document);
        assert_eq!(
            decoded.template_placeholders[0]
                .assigned_image_id
                .as_deref(),
            Some(image_id.as_str())
        );
        assert!(decoded.cutline_metadata[0].locked);
    }

    #[test]
    fn hostile_document_cannot_request_unbounded_cut_smoothing() {
        let mut document =
            StudioDocument::new(DocumentKind::Sheet, Vec2::new(100.0, 100.0), [255; 3]);
        document.settings.cut_smoothing = usize::MAX;
        let encoded = document.to_json().unwrap();
        let error = StudioDocument::from_json(&encoded).unwrap_err();
        assert!(error.to_string().contains("outside supported bounds"));
    }

    #[test]
    fn pack_is_bounded_and_deterministic() {
        let items = (0..4)
            .map(|index| PackItem {
                index,
                size: Vec2::new(40.0, 30.0),
            })
            .collect::<Vec<_>>();
        let first = auto_pack(&items, Vec2::new(100.0, 100.0), 5.0, true);
        assert_eq!(first, auto_pack(&items, Vec2::new(100.0, 100.0), 5.0, true));
        assert_eq!(first.len(), 4);
        assert!(
            first
                .iter()
                .all(|item| item.offset.x >= 0.0 && item.offset.y >= 0.0)
        );
        for (left_index, left) in first.iter().enumerate() {
            let left_size = if left.rotated {
                Vec2::new(30.0, 40.0)
            } else {
                Vec2::new(40.0, 30.0)
            };
            assert!(left.offset.x + left_size.x <= 100.0 && left.offset.y + left_size.y <= 100.0);
            let left_rect = egui::Rect::from_min_size(left.offset, left_size + Vec2::splat(4.99));
            for right in first.iter().skip(left_index + 1) {
                let right_size = if right.rotated {
                    Vec2::new(30.0, 40.0)
                } else {
                    Vec2::new(40.0, 30.0)
                };
                let right_rect = egui::Rect::from_min_size(right.offset, right_size);
                assert!(
                    !left_rect.intersects(right_rect),
                    "packed items violate the requested gap"
                );
            }
        }
    }

    #[test]
    fn background_removal_preserves_enclosed_matching_color() {
        let mut image = RgbaImage::from_pixel(5, 5, Rgba([255, 255, 255, 255]));
        for x in 1..4 {
            for y in 1..4 {
                image.put_pixel(x, y, Rgba([0, 0, 0, 255]));
            }
        }
        image.put_pixel(2, 2, Rgba([255, 255, 255, 255]));
        let result = remove_background(&image, 10, 0);
        assert_eq!(result.get_pixel(0, 0)[3], 0);
        assert_eq!(result.get_pixel(2, 2)[3], 255);
    }

    #[test]
    fn svg_lines_and_curves_are_imported() {
        let paths = parse_svg_path("M 0 0 L 10 0 Q 15 0 15 5 Z", 4).unwrap();
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0].0.first(), paths[0].0.last());
        assert_eq!(paths[0].0.len(), 7);
    }

    #[test]
    fn svg_smooth_curves_reflect_the_previous_control_point() {
        let cubic = parse_svg_path("M0 0 C0 10 10 10 10 0 S20 -10 20 0", 2).unwrap();
        assert_eq!(cubic[0].0.len(), 5);
        assert_coord_near(cubic[0].0[3], 15.0, -7.5);

        let quadratic = parse_svg_path("M0 0 Q10 10 20 0 T40 0", 2).unwrap();
        assert_eq!(quadratic[0].0.len(), 5);
        assert_coord_near(quadratic[0].0[3], 30.0, -5.0);

        let reset = parse_svg_path("M0 0 Q10 10 20 0 L30 0 T40 0", 2).unwrap();
        assert_coord_near(reset[0].0[4], 32.5, 0.0);
    }

    #[test]
    fn svg_drawing_command_after_close_starts_at_subpath_origin() {
        let paths = parse_svg_path("M0 0 L10 0 Z L20 0", 4).unwrap();
        assert_eq!(paths.len(), 2);
        assert_eq!(
            paths[1].0,
            vec![Coord { x: 0.0, y: 0.0 }, Coord { x: 20.0, y: 0.0 }]
        );

        let curve = parse_svg_path("M0 0 L1 0 Z C0 10 10 10 10 0", 2).unwrap();
        assert_eq!(curve.len(), 2);
        assert_eq!(curve[1].0.len(), 3);
        assert_coord_near(curve[1].0[0], 0.0, 0.0);
    }

    #[test]
    fn svg_elliptical_arcs_honor_sweep_and_degenerate_radii() {
        let clockwise = parse_svg_path("M0 0 A10 10 0 0 1 20 0", 4).unwrap();
        assert_eq!(clockwise[0].0.len(), 5);
        assert_coord_near(clockwise[0].0[2], 10.0, -10.0);
        assert_coord_near(*clockwise[0].0.last().unwrap(), 20.0, 0.0);

        let counterclockwise = parse_svg_path("M0 0 A10 10 0 0 0 20 0", 4).unwrap();
        assert_coord_near(counterclockwise[0].0[2], 10.0, 10.0);

        let line = parse_svg_path("M1 2 A0 10 45 1 1 5 6", 8).unwrap();
        assert_eq!(
            line[0].0,
            vec![Coord { x: 1.0, y: 2.0 }, Coord { x: 5.0, y: 6.0 }]
        );
    }

    #[test]
    fn svg_document_imports_multiple_paths() {
        let svg = r#"<svg xmlns="http://www.w3.org/2000/svg"><path d="M0 0 L1 1"/><path d="M2 2 L3 3"/></svg>"#;
        assert_eq!(parse_svg(svg, 4).unwrap().len(), 2);
    }

    #[test]
    fn svg_basic_shapes_are_imported_in_document_order() {
        let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="100">
            <rect x="1" y="2" width="10" height="20"/>
            <circle cx="20" cy="20" r="5"/>
            <ellipse cx="30" cy="40" rx="4" ry="2"/>
            <line x1="1" y1="3" x2="5" y2="7"/>
            <polyline points="0,0 5,5 10,0"/>
            <polygon points="20,0 25,5 30,0"/>
        </svg>"#;
        let paths = parse_svg(svg, 3).unwrap();
        assert_eq!(paths.len(), 6);
        assert_eq!(paths[0].0.len(), 5);
        assert_eq!(paths[1].0.len(), 13);
        assert_eq!(paths[2].0.first(), paths[2].0.last());
        assert_eq!(paths[3].0, vec![coord(1.0, 3.0), coord(5.0, 7.0)]);
        assert_eq!(paths[4].0.len(), 3);
        assert_eq!(paths[5].0.first(), paths[5].0.last());
    }

    #[test]
    fn svg_rounded_rect_inherits_and_clamps_radii() {
        let svg = r#"<svg xmlns="http://www.w3.org/2000/svg">
            <rect x="0" y="0" width="10" height="4" rx="20"/>
        </svg>"#;
        let paths = parse_svg(svg, 2).unwrap();
        assert_eq!(paths[0].0.first(), paths[0].0.last());
        assert!(paths[0].0.len() > 5);
        assert_coord_near(paths[0].0[0], 5.0, 0.0);
    }

    #[test]
    fn svg_viewbox_alignment_and_physical_dimensions_are_applied() {
        let centered = r#"<svg xmlns="http://www.w3.org/2000/svg" width="200" height="100" viewBox="0 0 100 100">
            <line x1="0" y1="0" x2="100" y2="100"/>
        </svg>"#;
        let path = &parse_svg(centered, 2).unwrap()[0];
        assert_coord_near(path.0[0], 50.0, 0.0);
        assert_coord_near(path.0[1], 150.0, 100.0);

        let stretched = r#"<svg xmlns="http://www.w3.org/2000/svg" width="25.4mm" height="96px" viewBox="0 0 10 20" preserveAspectRatio="none">
            <line x1="0" y1="0" x2="10" y2="20"/>
        </svg>"#;
        let path = &parse_svg(stretched, 2).unwrap()[0];
        assert_coord_near(path.0[1], 96.0, 96.0);
    }

    #[test]
    fn svg_tolerance_sampling_is_deterministic_and_refines_curves() {
        let data = "M0 0 C0 100 100 100 100 0 A50 50 0 0 1 200 0";
        let coarse = parse_svg_path_with_tolerance(data, 10.0).unwrap();
        let fine = parse_svg_path_with_tolerance(data, 0.1).unwrap();
        assert!(fine[0].0.len() > coarse[0].0.len());
        assert_eq!(fine, parse_svg_path_with_tolerance(data, 0.1).unwrap());

        let circle = r#"<svg xmlns="http://www.w3.org/2000/svg"><circle r="50"/></svg>"#;
        let coarse = parse_svg_with_tolerance(circle, 5.0).unwrap();
        let fine = parse_svg_with_tolerance(circle, 0.1).unwrap();
        assert!(fine[0].0.len() > coarse[0].0.len());
        assert!(parse_svg_with_tolerance(circle, 0.0).is_err());
    }

    #[test]
    fn svg_transform_lists_compose_through_ancestors() {
        let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" transform="translate(10 20)">
            <g transform="scale(2)">
                <path transform="rotate(90)" d="M1 0 L2 0"/>
            </g>
        </svg>"#;
        let paths = parse_svg(svg, 4).unwrap();
        assert_coord_near(paths[0].0[0], 10.0, 22.0);
        assert_coord_near(paths[0].0[1], 10.0, 24.0);

        let list = r#"<svg xmlns="http://www.w3.org/2000/svg">
            <path transform="translate(10 0) scale(2)" d="M1 1 L2 1"/>
        </svg>"#;
        let paths = parse_svg(list, 4).unwrap();
        assert_coord_near(paths[0].0[0], 12.0, 2.0);
        assert_coord_near(paths[0].0[1], 14.0, 2.0);
    }

    #[test]
    fn svg_use_instantiates_defs_geometry_at_rendered_position_only() {
        let svg = r##"<svg xmlns="http://www.w3.org/2000/svg">
            <defs><path id="piece" d="M1 2 L11 2"/></defs>
            <use href="#piece" x="100" y="20"/>
        </svg>"##;
        let paths = parse_svg(svg, 4).unwrap();
        assert_eq!(paths.len(), 1);
        assert_coord_near(paths[0].0[0], 101.0, 22.0);
        assert_coord_near(paths[0].0[1], 111.0, 22.0);
    }

    fn assert_coord_near(actual: Coord<f32>, x: f32, y: f32) {
        assert!((actual.x - x).abs() < 0.001, "x: {} != {x}", actual.x);
        assert!((actual.y - y).abs() < 0.001, "y: {} != {y}", actual.y);
    }

    #[test]
    fn perf_cut_splits_path() {
        let path = LineString::from(vec![(0.0, 0.0), (30.0, 0.0)]);
        let segments = perf_cut(&path, 5.0, 5.0);
        assert_eq!(segments.len(), 3);
        assert!((segments.iter().map(|p| p.0[1].x - p.0[0].x).sum::<f32>() - 15.0).abs() < 0.001);
    }
}
