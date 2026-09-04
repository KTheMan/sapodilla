use std::io::Cursor;

use image::{DynamicImage, ImageFormat, ImageReader, Limits, RgbImage};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::{Fiducial, TargetDiagnostic, TargetKind, TargetManifest, run_binding_bits};

// Hard limits keep malformed imports from turning analysis into an allocation hazard.
const MAX_SCAN_PIXELS: u64 = 60_000_000;
const MIN_SCAN_DIMENSION: u32 = 300;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ScanOrientation {
    Degrees0,
    Degrees90,
    Degrees180,
    Degrees270,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Affine2d {
    pub matrix: [[f64; 2]; 2],
    pub translation: [f64; 2],
}

impl Affine2d {
    pub const IDENTITY: Self = Self {
        matrix: [[1.0, 0.0], [0.0, 1.0]],
        translation: [0.0, 0.0],
    };

    pub fn apply(self, point: [f64; 2]) -> [f64; 2] {
        [
            self.matrix[0][0] * point[0] + self.matrix[0][1] * point[1] + self.translation[0],
            self.matrix[1][0] * point[0] + self.matrix[1][1] * point[1] + self.translation[1],
        ]
    }

    pub fn inverse(self) -> Option<Self> {
        let det = self.matrix[0][0] * self.matrix[1][1] - self.matrix[0][1] * self.matrix[1][0];
        if !det.is_finite() || det.abs() < 1e-12 {
            return None;
        }
        let matrix = [
            [self.matrix[1][1] / det, -self.matrix[0][1] / det],
            [-self.matrix[1][0] / det, self.matrix[0][0] / det],
        ];
        Some(Self {
            matrix,
            translation: [
                -(matrix[0][0] * self.translation[0] + matrix[0][1] * self.translation[1]),
                -(matrix[1][0] * self.translation[0] + matrix[1][1] * self.translation[1]),
            ],
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScanAnalysisConfig {
    pub backing_color_distance: f64,
    pub minimum_boundary_points: usize,
    pub maximum_circle_rms_mm: f64,
    pub accepted_confidence: f64,
}

impl Default for ScanAnalysisConfig {
    fn default() -> Self {
        Self {
            backing_color_distance: 72.0,
            minimum_boundary_points: 36,
            maximum_circle_rms_mm: 0.22,
            accepted_confidence: 0.72,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ScanFailureReason {
    RegionOutsideScan,
    RetainedSlug,
    LowContrast,
    InsufficientBoundary,
    ExcessiveCircleResidual,
    ImplausibleRadius,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", content = "reason", rename_all = "kebab-case")]
pub enum ScanTargetStatus {
    Accepted,
    Review(ScanFailureReason),
    Missing(ScanFailureReason),
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct CenterCovariance {
    pub xx_mm2: f64,
    pub xy_mm2: f64,
    pub yy_mm2: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ApertureDetection {
    pub target_id: String,
    pub status: ScanTargetStatus,
    pub expected_center_mm: [f64; 2],
    pub observed_center_mm: Option<[f64; 2]>,
    pub radius_mm: Option<f64>,
    pub circle_rms_mm: Option<f64>,
    pub confidence: f64,
    pub covariance: Option<CenterCovariance>,
    pub boundary_points_used: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScanAnalysisReport {
    pub format: ScanImageFormat,
    pub scan_dimensions_px: [u32; 2],
    pub orientation: ScanOrientation,
    /// Maps scanner pixels into printed millimeters.
    pub scanner_to_print: Affine2d,
    pub backing_rgb: [f64; 3],
    pub fiducial_rms_px: f64,
    /// SHA-1 identity encoded on and decoded from this physical sheet.
    #[serde(default)]
    pub run_binding_sha1: String,
    pub targets: Vec<ApertureDetection>,
}

impl ScanAnalysisReport {
    pub fn accepted_count(&self) -> usize {
        self.targets
            .iter()
            .filter(|target| target.status == ScanTargetStatus::Accepted)
            .count()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ScanImageFormat {
    Png,
    Jpeg,
}

#[derive(Debug, Error)]
pub enum ScanAnalysisError {
    #[error("scan must be a PNG or JPEG image")]
    UnsupportedFormat,
    #[error("scan image could not be decoded: {0}")]
    Decode(#[source] image::ImageError),
    #[error("scan dimensions are too small")]
    TooSmall,
    #[error("scan contains too many pixels")]
    TooLarge,
    #[error("manifest does not contain the required four asymmetric fiducials")]
    MissingFiducials,
    #[error("manifest does not contain Flatbed aperture targets")]
    MissingTargets,
    #[error("sheet orientation or fiducials could not be resolved")]
    UnresolvedOrientation,
    #[error("scanner-to-print mapping is singular")]
    SingularRectification,
    #[error("backing control aperture could not be sampled")]
    MissingBackingSample,
    #[error("manifest does not contain a valid scanner-readable run binding")]
    MissingRunBinding,
    #[error("scanner-readable run binding has insufficient contrast")]
    UnreadableRunBinding,
    #[error("scan belongs to a different calibration run or purpose")]
    RunBindingMismatch,
}

pub fn analyze_flatbed_scan(
    encoded: &[u8],
    manifest: &TargetManifest,
    config: ScanAnalysisConfig,
) -> Result<ScanAnalysisReport, ScanAnalysisError> {
    let guessed = image::guess_format(encoded).map_err(|_| ScanAnalysisError::UnsupportedFormat)?;
    let format = match guessed {
        ImageFormat::Png => ScanImageFormat::Png,
        ImageFormat::Jpeg => ScanImageFormat::Jpeg,
        _ => return Err(ScanAnalysisError::UnsupportedFormat),
    };
    // Inspect the decoder header before asking it to allocate the output buffer.
    // `load_from_memory_with_format` performs this check too late for hostile,
    // highly-compressed images whose declared dimensions are enormous.
    let dimensions = ImageReader::with_format(Cursor::new(encoded), guessed)
        .into_dimensions()
        .map_err(ScanAnalysisError::Decode)?;
    validate_scan_dimensions(dimensions.0, dimensions.1)?;

    let mut reader = ImageReader::with_format(Cursor::new(encoded), guessed);
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_SCAN_PIXELS.min(u64::from(u32::MAX)) as u32);
    limits.max_image_height = Some(MAX_SCAN_PIXELS.min(u64::from(u32::MAX)) as u32);
    // Covers the largest supported decoded representation (RGBA16). The
    // independently checked pixel product remains the authoritative limit.
    limits.max_alloc = Some(MAX_SCAN_PIXELS * 8);
    reader.limits(limits);
    let decoded = reader.decode().map_err(ScanAnalysisError::Decode)?;
    analyze_decoded(decoded, format, manifest, config)
}

fn validate_scan_dimensions(width: u32, height: u32) -> Result<(), ScanAnalysisError> {
    if width.min(height) < MIN_SCAN_DIMENSION {
        return Err(ScanAnalysisError::TooSmall);
    }
    if u64::from(width) * u64::from(height) > MAX_SCAN_PIXELS {
        return Err(ScanAnalysisError::TooLarge);
    }
    Ok(())
}

fn analyze_decoded(
    decoded: DynamicImage,
    format: ScanImageFormat,
    manifest: &TargetManifest,
    config: ScanAnalysisConfig,
) -> Result<ScanAnalysisReport, ScanAnalysisError> {
    let image = decoded.to_rgb8();
    validate_scan_dimensions(image.width(), image.height())?;
    let targets: Vec<_> = manifest
        .targets
        .iter()
        .filter(|target| target.kind == TargetKind::FlatbedAperture)
        .collect();
    if targets.is_empty() {
        return Err(ScanAnalysisError::MissingTargets);
    }
    if manifest.fiducials.len() != 4 {
        return Err(ScanAnalysisError::MissingFiducials);
    }

    let located = locate_fiducials(&image, manifest)?;
    let print_to_scanner = fit_affine(
        &manifest
            .fiducials
            .iter()
            .map(|fiducial| [fiducial.center_mm.x, fiducial.center_mm.y])
            .collect::<Vec<_>>(),
        &located.centers,
    )
    .ok_or(ScanAnalysisError::SingularRectification)?;
    let scanner_to_print = print_to_scanner
        .inverse()
        .ok_or(ScanAnalysisError::SingularRectification)?;
    let fiducial_rms_px = point_fit_rms(
        print_to_scanner,
        &manifest
            .fiducials
            .iter()
            .map(|fiducial| [fiducial.center_mm.x, fiducial.center_mm.y])
            .collect::<Vec<_>>(),
        &located.centers,
    );
    let run_binding_sha1 = verify_run_binding(&image, manifest, print_to_scanner)?;
    let backing_rgb = sample_backing(&image, manifest, print_to_scanner)
        .ok_or(ScanAnalysisError::MissingBackingSample)?;

    let targets = targets
        .into_iter()
        .map(|target| {
            detect_aperture(
                &image,
                target,
                print_to_scanner,
                scanner_to_print,
                backing_rgb,
                config,
            )
        })
        .collect();

    Ok(ScanAnalysisReport {
        format,
        scan_dimensions_px: [image.width(), image.height()],
        orientation: located.orientation,
        scanner_to_print,
        backing_rgb,
        fiducial_rms_px,
        run_binding_sha1,
        targets,
    })
}

fn verify_run_binding(
    image: &RgbImage,
    manifest: &TargetManifest,
    print_to_scanner: Affine2d,
) -> Result<String, ScanAnalysisError> {
    let binding = manifest.diagnostics.iter().find_map(|diagnostic| {
        if let TargetDiagnostic::RunBinding {
            origin_mm,
            cell_mm,
            columns,
            rows,
            digest_hex,
        } = diagnostic
        {
            Some((*origin_mm, *cell_mm, *columns, *rows, digest_hex))
        } else {
            None
        }
    });
    let (origin, cell_mm, columns, rows, digest_hex) =
        binding.ok_or(ScanAnalysisError::MissingRunBinding)?;
    let expected = run_binding_bits(digest_hex).ok_or(ScanAnalysisError::MissingRunBinding)?;
    if columns != 8 || rows != 6 || expected.len() != usize::from(columns) * usize::from(rows) {
        return Err(ScanAnalysisError::MissingRunBinding);
    }

    let mut luminance = Vec::with_capacity(expected.len());
    for index in 0..expected.len() {
        let column = index % usize::from(columns);
        let row = index / usize::from(columns);
        let center = [
            origin.x + (column as f64 + 0.5) * cell_mm,
            origin.y + (row as f64 + 0.5) * cell_mm,
        ];
        let mut sum = 0.0;
        let mut count = 0.0;
        for oy in [-0.18, 0.0, 0.18] {
            for ox in [-0.18, 0.0, 0.18] {
                let point =
                    print_to_scanner.apply([center[0] + ox * cell_mm, center[1] + oy * cell_mm]);
                if let Some(rgb) = sample_rgb(image, point[0], point[1]) {
                    sum += rgb[0] * 0.2126 + rgb[1] * 0.7152 + rgb[2] * 0.0722;
                    count += 1.0;
                }
            }
        }
        if count == 0.0 {
            return Err(ScanAnalysisError::UnreadableRunBinding);
        }
        luminance.push(sum / count);
    }

    // The invariant sync byte has four dark and four light cells, which gives
    // each scan its own threshold without trusting the run-specific payload.
    let (mut black_sum, mut black_count, mut white_sum, mut white_count) = (0.0, 0.0, 0.0, 0.0);
    for (&value, &bit) in luminance.iter().zip(&expected).take(8) {
        if bit {
            black_sum += value;
            black_count += 1.0;
        } else {
            white_sum += value;
            white_count += 1.0;
        }
    }
    let black = black_sum / black_count;
    let white = white_sum / white_count;
    if !black.is_finite() || !white.is_finite() || white - black < 45.0 {
        return Err(ScanAnalysisError::UnreadableRunBinding);
    }
    let threshold = (black + white) / 2.0;
    let observed = luminance
        .into_iter()
        .map(|value| value < threshold)
        .collect::<Vec<_>>();
    let sync_errors = observed
        .iter()
        .zip(&expected)
        .take(8)
        .filter(|(actual, wanted)| actual != wanted)
        .count();
    let payload_errors = observed
        .iter()
        .zip(&expected)
        .skip(8)
        .filter(|(actual, wanted)| actual != wanted)
        .count();
    if sync_errors > 1 || payload_errors > 2 {
        return Err(ScanAnalysisError::RunBindingMismatch);
    }
    Ok(digest_hex.clone())
}

struct LocatedFiducials {
    orientation: ScanOrientation,
    centers: Vec<[f64; 2]>,
}

fn locate_fiducials(
    image: &RgbImage,
    manifest: &TargetManifest,
) -> Result<LocatedFiducials, ScanAnalysisError> {
    let reference =
        super::render_print_raster(manifest).map_err(|_| ScanAnalysisError::MissingFiducials)?;
    let canvas = [manifest.canvas.width_mm, manifest.canvas.height_mm];
    let coarse = DarkIntegral::new(image);
    let mut best: Option<(f64, LocatedFiducials)> = None;
    // Scanner-bed aspect ratio does not reveal sheet orientation when the scan
    // is intentionally uncropped, so every right-angle orientation is viable.
    for orientation in [
        ScanOrientation::Degrees0,
        ScanOrientation::Degrees90,
        ScanOrientation::Degrees180,
        ScanOrientation::Degrees270,
    ] {
        let mut initial_candidates =
            global_page_mapping_candidates(image, manifest, canvas, orientation, &coarse)
                .into_iter()
                .map(|mapping| {
                    let score = manifest
                        .fiducials
                        .iter()
                        .map(|fiducial| {
                            let center =
                                mapping.apply([fiducial.center_mm.x, fiducial.center_mm.y]);
                            fiducial_template_score(
                                image, &reference, manifest, fiducial, mapping, center,
                            )
                        })
                        .sum::<f64>();
                    (score, mapping)
                })
                .collect::<Vec<_>>();
        // The inexpensive integral-image pass may return adjacent scale/offset
        // hypotheses. Rank them with the asymmetric templates before doing the
        // substantially more expensive pixel-level neighborhood searches.
        initial_candidates.sort_by(|left, right| right.0.total_cmp(&left.0));
        for (_, initial) in initial_candidates.into_iter().take(1) {
            let mut centers = Vec::with_capacity(4);
            let mut total_score = 0.0;
            for fiducial in &manifest.fiducials {
                let approximate = initial.apply([fiducial.center_mm.x, fiducial.center_mm.y]);
                let Some((center, score)) =
                    template_search(image, &reference, manifest, fiducial, initial, approximate)
                else {
                    centers.clear();
                    break;
                };
                centers.push(center);
                total_score += score;
            }
            if centers.len() == 4 {
                let expected: Vec<_> = manifest
                    .fiducials
                    .iter()
                    .map(|fiducial| [fiducial.center_mm.x, fiducial.center_mm.y])
                    .collect();
                for _ in 0..2 {
                    let Some(refined_mapping) = fit_affine(&expected, &centers) else {
                        centers.clear();
                        break;
                    };
                    total_score = 0.0;
                    for (center, fiducial) in centers.iter_mut().zip(&manifest.fiducials) {
                        let approximate =
                            refined_mapping.apply([fiducial.center_mm.x, fiducial.center_mm.y]);
                        let (refined, score) = template_refine(
                            image,
                            &reference,
                            manifest,
                            fiducial,
                            refined_mapping,
                            approximate,
                        );
                        *center = refined;
                        total_score += score;
                    }
                }
            }
            if centers.len() == 4
                && best
                    .as_ref()
                    .is_none_or(|(best_score, _)| total_score > *best_score)
            {
                best = Some((
                    total_score,
                    LocatedFiducials {
                        orientation,
                        centers,
                    },
                ));
            }
        }
    }
    let Some((score, located)) = best else {
        return Err(ScanAnalysisError::UnresolvedOrientation);
    };
    if score < 0.45 * 4.0 {
        return Err(ScanAnalysisError::UnresolvedOrientation);
    }
    Ok(located)
}

/// A max-pooled dark-pixel mask with a summed-area table. It makes a global
/// page search proportional to a small, fixed analysis grid rather than to the
/// potentially 60-megapixel source image.
struct DarkIntegral {
    width: usize,
    height: usize,
    divisor: u32,
    sums: Vec<u32>,
}

impl DarkIntegral {
    fn new(image: &RgbImage) -> Self {
        const MAX_COARSE_EDGE: u32 = 1_400;
        let divisor = image
            .width()
            .max(image.height())
            .div_ceil(MAX_COARSE_EDGE)
            .max(1);
        let width = image.width().div_ceil(divisor) as usize;
        let height = image.height().div_ceil(divisor) as usize;
        let mut mask = vec![false; width * height];
        for (x, y, pixel) in image.enumerate_pixels() {
            if is_dark_neutral(pixel.0.map(f64::from)) {
                mask[(y / divisor) as usize * width + (x / divisor) as usize] = true;
            }
        }
        let stride = width + 1;
        let mut sums = vec![0_u32; stride * (height + 1)];
        for y in 0..height {
            let mut row = 0_u32;
            for x in 0..width {
                row += u32::from(mask[y * width + x]);
                sums[(y + 1) * stride + x + 1] = sums[y * stride + x + 1] + row;
            }
        }
        Self {
            width,
            height,
            divisor,
            sums,
        }
    }

    fn dark_fraction(&self, center: [f64; 2], half_extent: f64) -> f64 {
        let divisor = f64::from(self.divisor);
        let left = ((center[0] - half_extent) / divisor).floor() as i32;
        let top = ((center[1] - half_extent) / divisor).floor() as i32;
        let right = ((center[0] + half_extent) / divisor).ceil() as i32;
        let bottom = ((center[1] + half_extent) / divisor).ceil() as i32;
        let x0 = left.clamp(0, self.width as i32) as usize;
        let y0 = top.clamp(0, self.height as i32) as usize;
        let x1 = right.clamp(0, self.width as i32) as usize;
        let y1 = bottom.clamp(0, self.height as i32) as usize;
        if x0 >= x1 || y0 >= y1 {
            return 0.0;
        }
        let stride = self.width + 1;
        let count = self.sums[y1 * stride + x1] + self.sums[y0 * stride + x0]
            - self.sums[y0 * stride + x1]
            - self.sums[y1 * stride + x0];
        f64::from(count) / ((x1 - x0) * (y1 - y0)) as f64
    }
}

fn global_page_mapping_candidates(
    image: &RgbImage,
    manifest: &TargetManifest,
    canvas: [f64; 2],
    orientation: ScanOrientation,
    coarse: &DarkIntegral,
) -> Vec<Affine2d> {
    let (oriented_width_mm, oriented_height_mm) = match orientation {
        ScanOrientation::Degrees0 | ScanOrientation::Degrees180 => (canvas[0], canvas[1]),
        ScanOrientation::Degrees90 | ScanOrientation::Degrees270 => (canvas[1], canvas[0]),
    };
    let maximum_scale = (f64::from(image.width()) / oriented_width_mm)
        .min(f64::from(image.height()) / oriented_height_mm);
    // A 4x7 sheet occupies about 47% of the limiting axis on a Letter/A4
    // flatbed. The wider range also permits unusually generous scan margins.
    let minimum_scale = (maximum_scale * 0.34).max(1.0);
    if minimum_scale > maximum_scale {
        return Vec::new();
    }

    const SCALE_STEPS: usize = 34;
    const CANDIDATES_PER_SCALE: usize = 1;
    let mut candidates = Vec::with_capacity(SCALE_STEPS * CANDIDATES_PER_SCALE);
    for scale_index in 0..SCALE_STEPS {
        let fraction = scale_index as f64 / (SCALE_STEPS - 1) as f64;
        let scale = minimum_scale + (maximum_scale - minimum_scale) * fraction;
        let page_width = oriented_width_mm * scale;
        let page_height = oriented_height_mm * scale;
        let maximum_x = (f64::from(image.width()) - page_width).max(0.0);
        let maximum_y = (f64::from(image.height()) - page_height).max(0.0);
        let step = (scale * 1.25).max(f64::from(coarse.divisor) * 2.0);
        let x_steps = (maximum_x / step).ceil() as usize;
        let y_steps = (maximum_y / step).ceil() as usize;
        let mut scale_candidates: Vec<(f64, Affine2d)> = Vec::with_capacity(CANDIDATES_PER_SCALE);
        for yi in 0..=y_steps {
            let top = (yi as f64 * step).min(maximum_y);
            for xi in 0..=x_steps {
                let left = (xi as f64 * step).min(maximum_x);
                let mapping = page_mapping_at(scale, canvas, orientation, [left, top]);
                let fractions = manifest.fiducials.iter().map(|fiducial| {
                    let center = mapping.apply([fiducial.center_mm.x, fiducial.center_mm.y]);
                    coarse.dark_fraction(center, scale * fiducial.extent_mm * 0.72)
                });
                // Requiring all corners to contribute avoids dense text or a
                // single dark object dominating the global seed score.
                let mut minimum = 1.0_f64;
                let mut sum = 0.0;
                for fraction in fractions {
                    minimum = minimum.min(fraction);
                    sum += fraction;
                }
                let score = minimum * 2.0 + sum;
                if scale_candidates.len() < CANDIDATES_PER_SCALE
                    || score
                        > scale_candidates
                            .last()
                            .map_or(f64::NEG_INFINITY, |item| item.0)
                {
                    scale_candidates.push((score, mapping));
                    scale_candidates.sort_by(|left, right| right.0.total_cmp(&left.0));
                    scale_candidates.truncate(CANDIDATES_PER_SCALE);
                }
            }
        }
        candidates.extend(scale_candidates);
    }
    candidates.into_iter().map(|(_, mapping)| mapping).collect()
}

fn page_mapping_at(
    scale: f64,
    canvas: [f64; 2],
    orientation: ScanOrientation,
    top_left: [f64; 2],
) -> Affine2d {
    match orientation {
        ScanOrientation::Degrees0 => Affine2d {
            matrix: [[scale, 0.0], [0.0, scale]],
            translation: top_left,
        },
        ScanOrientation::Degrees90 => Affine2d {
            matrix: [[0.0, -scale], [scale, 0.0]],
            translation: [top_left[0] + canvas[1] * scale, top_left[1]],
        },
        ScanOrientation::Degrees180 => Affine2d {
            matrix: [[-scale, 0.0], [0.0, -scale]],
            translation: [
                top_left[0] + canvas[0] * scale,
                top_left[1] + canvas[1] * scale,
            ],
        },
        ScanOrientation::Degrees270 => Affine2d {
            matrix: [[0.0, scale], [-scale, 0.0]],
            translation: [top_left[0], top_left[1] + canvas[0] * scale],
        },
    }
}

fn template_refine(
    image: &RgbImage,
    reference: &RgbImage,
    manifest: &TargetManifest,
    fiducial: &Fiducial,
    mapping: Affine2d,
    approximate: [f64; 2],
) -> ([f64; 2], f64) {
    let mut best = (approximate, f64::NEG_INFINITY);
    let center = [approximate[0].round() as i32, approximate[1].round() as i32];
    for y in center[1] - 4..=center[1] + 4 {
        for x in center[0] - 4..=center[0] + 4 {
            let score = fiducial_template_score(
                image,
                reference,
                manifest,
                fiducial,
                mapping,
                [x as f64, y as f64],
            );
            if score > best.1 {
                best = ([x as f64, y as f64], score);
            }
        }
    }
    best
}

fn template_search(
    image: &RgbImage,
    reference: &RgbImage,
    manifest: &TargetManifest,
    fiducial: &Fiducial,
    mapping: Affine2d,
    approximate: [f64; 2],
) -> Option<([f64; 2], f64)> {
    let px_per_mm = mapping.matrix[0][0]
        .hypot(mapping.matrix[1][0])
        .max(mapping.matrix[0][1].hypot(mapping.matrix[1][1]));
    // The coarse page seed intentionally models only scale, offset, and a
    // right-angle orientation. Leave enough room here for ordinary scanner
    // skew and small desk rotation before the four located centers are used
    // to fit the full affine rectification.
    let search_radius = (px_per_mm * 20.0).clamp(24.0, 480.0) as i32;
    let mut best: ([f64; 2], f64) = ([0.0, 0.0], f64::NEG_INFINITY);
    for (step, local_radius) in [(8_i32, search_radius), (2, 10), (1, 3)] {
        let (cx, cy, radius) = if step == 8 {
            (
                approximate[0].round() as i32,
                approximate[1].round() as i32,
                local_radius,
            )
        } else {
            (
                best.0[0].round() as i32,
                best.0[1].round() as i32,
                local_radius,
            )
        };
        let mut local_best = best;
        for y in (cy - radius..=cy + radius).step_by(step as usize) {
            for x in (cx - radius..=cx + radius).step_by(step as usize) {
                let score = fiducial_template_score(
                    image,
                    reference,
                    manifest,
                    fiducial,
                    mapping,
                    [x as f64, y as f64],
                );
                if score > local_best.1 {
                    local_best = ([x as f64, y as f64], score);
                }
            }
        }
        best = local_best;
    }
    best.1.is_finite().then_some(best)
}

fn fiducial_template_score(
    image: &RgbImage,
    reference: &RgbImage,
    manifest: &TargetManifest,
    fiducial: &Fiducial,
    mapping: Affine2d,
    center: [f64; 2],
) -> f64 {
    let mut true_positive = 0.0;
    let mut false_positive = 0.0;
    let mut false_negative = 0.0;
    for yi in -8..=8 {
        for xi in -8..=8 {
            let local = [xi as f64 * 0.52, yi as f64 * 0.52];
            let reference_mm = [
                fiducial.center_mm.x + local[0],
                fiducial.center_mm.y + local[1],
            ];
            let expected = sample_rgb(
                reference,
                reference_mm[0] * manifest.canvas.dots_per_mm(),
                reference_mm[1] * manifest.canvas.dots_per_mm(),
            )
            .is_some_and(is_dark_neutral);
            let delta = [
                mapping.matrix[0][0] * local[0] + mapping.matrix[0][1] * local[1],
                mapping.matrix[1][0] * local[0] + mapping.matrix[1][1] * local[1],
            ];
            let observed = sample_rgb(image, center[0] + delta[0], center[1] + delta[1])
                .is_some_and(is_dark_neutral);
            match (expected, observed) {
                (true, true) => true_positive += 1.0,
                (false, true) => false_positive += 1.0,
                (true, false) => false_negative += 1.0,
                (false, false) => {}
            }
        }
    }
    2.0 * true_positive / (2.0 * true_positive + false_positive + false_negative + 1e-9)
}

fn is_dark_neutral(rgb: [f64; 3]) -> bool {
    let maximum = rgb.into_iter().fold(f64::NEG_INFINITY, f64::max);
    let minimum = rgb.into_iter().fold(f64::INFINITY, f64::min);
    (rgb[0] + rgb[1] + rgb[2]) / 3.0 < 125.0 && maximum - minimum < 45.0
}

fn sample_backing(
    image: &RgbImage,
    manifest: &TargetManifest,
    print_to_scanner: Affine2d,
) -> Option<[f64; 3]> {
    let (center, diameter) = manifest.diagnostics.iter().find_map(|diagnostic| {
        if let TargetDiagnostic::BackingSample {
            center_mm,
            diameter_mm,
            ..
        } = diagnostic
        {
            Some((*center_mm, *diameter_mm))
        } else {
            None
        }
    })?;
    let center_px = print_to_scanner.apply([center.x, center.y]);
    let scale = local_pixels_per_mm(print_to_scanner);
    let radius = diameter * 0.28 * scale;
    let mut channels = [Vec::new(), Vec::new(), Vec::new()];
    let bounds = radius.ceil() as i32;
    for dy in -bounds..=bounds {
        for dx in -bounds..=bounds {
            if (dx as f64).hypot(dy as f64) <= radius
                && let Some(rgb) =
                    sample_rgb(image, center_px[0] + dx as f64, center_px[1] + dy as f64)
            {
                for channel in 0..3 {
                    channels[channel].push(rgb[channel]);
                }
            }
        }
    }
    if channels[0].len() < 16 {
        return None;
    }
    Some(channels.map(|mut values| {
        values.sort_by(f64::total_cmp);
        values[values.len() / 2]
    }))
}

fn locate_local_print_center(
    image: &RgbImage,
    predicted: [f64; 2],
    pixels_per_mm: f64,
) -> Option<[f64; 2]> {
    let radius = (pixels_per_mm * 9.0).ceil() as i32;
    let mut sum = [0.0, 0.0];
    let mut count = 0_usize;
    for dy in -radius..=radius {
        for dx in -radius..=radius {
            let x = predicted[0] + f64::from(dx);
            let y = predicted[1] + f64::from(dy);
            let Some(rgb) = sample_rgb(image, x, y) else {
                continue;
            };
            // The target generator deliberately uses a saturated yellow local
            // ring and symmetric ticks. Ratios tolerate ordinary scanner casts
            // and JPEG artifacts while rejecting the navy station patch.
            if rgb[0] > 145.0
                && rgb[1] > 125.0
                && rgb[2] < 165.0
                && (rgb[0] + rgb[1]) / 2.0 - rgb[2] > 55.0
            {
                sum[0] += x;
                sum[1] += y;
                count += 1;
            }
        }
    }
    (count >= 20).then_some([sum[0] / count as f64, sum[1] / count as f64])
}

fn local_backing_fraction(
    image: &RgbImage,
    center: [f64; 2],
    pixels_per_mm: f64,
    backing: [f64; 3],
    threshold: f64,
) -> f64 {
    let radius = (pixels_per_mm * 1.2).max(2.0);
    let bounds = radius.ceil() as i32;
    let mut matching = 0_usize;
    let mut total = 0_usize;
    for dy in -bounds..=bounds {
        for dx in -bounds..=bounds {
            if f64::from(dx).hypot(f64::from(dy)) > radius {
                continue;
            }
            if let Some(rgb) =
                sample_rgb(image, center[0] + f64::from(dx), center[1] + f64::from(dy))
            {
                total += 1;
                matching += usize::from(color_distance(rgb, backing) <= threshold);
            }
        }
    }
    matching as f64 / total.max(1) as f64
}

fn detect_aperture(
    image: &RgbImage,
    target: &super::TargetStation,
    print_to_scanner: Affine2d,
    scanner_to_print: Affine2d,
    backing: [f64; 3],
    config: ScanAnalysisConfig,
) -> ApertureDetection {
    let expected = [target.center_mm.x, target.center_mm.y];
    let predicted_px = print_to_scanner.apply(expected);
    let expected_radius = target.nominal_cut_bounds_mm.width / 2.0;
    let scale = local_pixels_per_mm(print_to_scanner);
    let roi_radius = expected_radius * scale * 1.45;
    if predicted_px[0] - roi_radius < 0.0
        || predicted_px[1] - roi_radius < 0.0
        || predicted_px[0] + roi_radius >= f64::from(image.width())
        || predicted_px[1] + roi_radius >= f64::from(image.height())
    {
        return failed_detection(
            target,
            ScanTargetStatus::Missing(ScanFailureReason::RegionOutsideScan),
        );
    }

    let local_print_center =
        locate_local_print_center(image, predicted_px, scale).unwrap_or(predicted_px);
    let backing_fraction = local_backing_fraction(
        image,
        local_print_center,
        scale,
        backing,
        config.backing_color_distance,
    );
    if backing_fraction < 0.2 {
        return failed_detection(
            target,
            ScanTargetStatus::Missing(ScanFailureReason::RetainedSlug),
        );
    }

    let bridge_half_angles: Vec<f64> = target
        .bridge_angles_degrees
        .iter()
        .map(|_| target.bridge_arc_mm.unwrap_or(0.0) / expected_radius * 0.5 + 10_f64.to_radians())
        .collect();
    let mut boundary = Vec::new();
    let samples = 180;
    for index in 0..samples {
        let angle = std::f64::consts::TAU * index as f64 / samples as f64;
        if target
            .bridge_angles_degrees
            .iter()
            .zip(&bridge_half_angles)
            .any(|(bridge, half)| angle_distance(angle, bridge.to_radians()) <= *half)
        {
            continue;
        }
        let mut last_backing = None;
        let steps = (expected_radius * scale * 1.45).ceil() as usize;
        for step in 1..=steps {
            let radius = step as f64;
            let pixel = [
                local_print_center[0] + radius * angle.cos(),
                local_print_center[1] + radius * angle.sin(),
            ];
            let Some(rgb) = sample_rgb(image, pixel[0], pixel[1]) else {
                break;
            };
            if color_distance(rgb, backing) <= config.backing_color_distance {
                last_backing = Some(pixel);
            } else if radius >= expected_radius * scale * 0.65 && last_backing.is_some() {
                break;
            }
        }
        if let Some(pixel) = last_backing {
            boundary.push(scanner_to_print.apply(pixel));
        }
    }
    if boundary.len() < config.minimum_boundary_points {
        let reason = if backing_fraction < 0.45 {
            ScanFailureReason::LowContrast
        } else {
            ScanFailureReason::InsufficientBoundary
        };
        return failed_detection(target, ScanTargetStatus::Missing(reason));
    }

    let Some(fit) = robust_circle_fit(&boundary) else {
        return failed_detection(
            target,
            ScanTargetStatus::Review(ScanFailureReason::InsufficientBoundary),
        );
    };
    let radius_error = (fit.radius_mm - expected_radius).abs();
    let coverage = boundary.len() as f64 / samples as f64;
    let confidence = (coverage / 0.72).min(1.0)
        * (-fit.rms_mm / config.maximum_circle_rms_mm.max(0.01)).exp()
        * (-radius_error / 0.8).exp();
    // Registration is local: printed ticks/ring define the intended center.
    // The page affine supplies units and orientation but any small global
    // fiducial bias cancels instead of being mistaken for cutter error.
    let mapped_local_center = scanner_to_print.apply(local_print_center);
    let observed_center = [
        fit.center_mm[0] + expected[0] - mapped_local_center[0],
        fit.center_mm[1] + expected[1] - mapped_local_center[1],
    ];
    let status = if !(expected_radius * 0.75..=expected_radius * 1.25).contains(&fit.radius_mm) {
        ScanTargetStatus::Review(ScanFailureReason::ImplausibleRadius)
    } else if fit.rms_mm > config.maximum_circle_rms_mm {
        ScanTargetStatus::Review(ScanFailureReason::ExcessiveCircleResidual)
    } else if confidence < config.accepted_confidence {
        ScanTargetStatus::Review(ScanFailureReason::LowContrast)
    } else {
        ScanTargetStatus::Accepted
    };
    ApertureDetection {
        target_id: target.id.clone(),
        status,
        expected_center_mm: expected,
        observed_center_mm: Some(observed_center),
        radius_mm: Some(fit.radius_mm),
        circle_rms_mm: Some(fit.rms_mm),
        confidence,
        covariance: Some(fit.covariance),
        boundary_points_used: fit.points_used,
    }
}

fn failed_detection(target: &super::TargetStation, status: ScanTargetStatus) -> ApertureDetection {
    ApertureDetection {
        target_id: target.id.clone(),
        status,
        expected_center_mm: [target.center_mm.x, target.center_mm.y],
        observed_center_mm: None,
        radius_mm: None,
        circle_rms_mm: None,
        confidence: 0.0,
        covariance: None,
        boundary_points_used: 0,
    }
}

struct CircleFit {
    center_mm: [f64; 2],
    radius_mm: f64,
    rms_mm: f64,
    covariance: CenterCovariance,
    points_used: usize,
}

fn robust_circle_fit(points: &[[f64; 2]]) -> Option<CircleFit> {
    let mut included = vec![true; points.len()];
    let mut parameters = algebraic_circle_fit(points, &included)?;
    for _ in 0..3 {
        let residuals: Vec<f64> = points
            .iter()
            .map(|point| {
                (point[0] - parameters.0[0]).hypot(point[1] - parameters.0[1]) - parameters.1
            })
            .collect();
        let mut absolute: Vec<f64> = residuals.iter().map(|value| value.abs()).collect();
        absolute.sort_by(f64::total_cmp);
        let median = absolute[absolute.len() / 2];
        let threshold = (median * 3.5).max(0.08);
        let next: Vec<bool> = residuals
            .iter()
            .map(|value| value.abs() <= threshold)
            .collect();
        if next == included {
            break;
        }
        included = next;
        parameters = algebraic_circle_fit(points, &included)?;
    }
    let residuals: Vec<f64> = points
        .iter()
        .zip(&included)
        .filter(|(_, included)| **included)
        .map(|(point, _)| {
            (point[0] - parameters.0[0]).hypot(point[1] - parameters.0[1]) - parameters.1
        })
        .collect();
    if residuals.len() < 3 {
        return None;
    }
    let rms =
        (residuals.iter().map(|value| value * value).sum::<f64>() / residuals.len() as f64).sqrt();
    let center_variance = (rms * rms / (residuals.len() as f64 / 2.0).max(1.0)).max(1e-12);
    Some(CircleFit {
        center_mm: parameters.0,
        radius_mm: parameters.1,
        rms_mm: rms,
        covariance: CenterCovariance {
            xx_mm2: center_variance,
            xy_mm2: 0.0,
            yy_mm2: center_variance,
        },
        points_used: residuals.len(),
    })
}

fn algebraic_circle_fit(points: &[[f64; 2]], included: &[bool]) -> Option<([f64; 2], f64)> {
    let accepted: Vec<_> = points
        .iter()
        .zip(included)
        .filter_map(|(point, included)| included.then_some(*point))
        .collect();
    if accepted.len() < 3 {
        return None;
    }
    let mean = accepted
        .iter()
        .fold([0.0, 0.0], |mut sum, point| {
            sum[0] += point[0];
            sum[1] += point[1];
            sum
        })
        .map(|sum| sum / accepted.len() as f64);
    let mut normal = [[0.0; 4]; 3];
    for point in &accepted {
        let x = point[0] - mean[0];
        let y = point[1] - mean[1];
        let row = [x, y, 1.0];
        let output = -(x * x + y * y);
        for i in 0..3 {
            for j in 0..3 {
                normal[i][j] += row[i] * row[j];
            }
            normal[i][3] += row[i] * output;
        }
    }
    let coefficients = solve_3x3(normal)?;
    let local_center = [-coefficients[0] / 2.0, -coefficients[1] / 2.0];
    let radius_squared = local_center[0].powi(2) + local_center[1].powi(2) - coefficients[2];
    (radius_squared > 0.0 && radius_squared.is_finite()).then_some((
        [local_center[0] + mean[0], local_center[1] + mean[1]],
        radius_squared.sqrt(),
    ))
}

fn solve_3x3(mut matrix: [[f64; 4]; 3]) -> Option<[f64; 3]> {
    for column in 0..3 {
        let pivot = (column..3).max_by(|a, b| {
            matrix[*a][column]
                .abs()
                .total_cmp(&matrix[*b][column].abs())
        })?;
        matrix.swap(column, pivot);
        if matrix[column][column].abs() < 1e-12 {
            return None;
        }
        let pivot_values = matrix[column];
        for row in column + 1..3 {
            let factor = matrix[row][column] / matrix[column][column];
            for (index, value) in matrix[row].iter_mut().enumerate().skip(column) {
                *value -= factor * pivot_values[index];
            }
        }
    }
    let mut output = [0.0; 3];
    for row in (0..3).rev() {
        let known: f64 = (row + 1..3)
            .map(|column| matrix[row][column] * output[column])
            .sum();
        output[row] = (matrix[row][3] - known) / matrix[row][row];
    }
    output.into_iter().all(f64::is_finite).then_some(output)
}

fn fit_affine(source: &[[f64; 2]], destination: &[[f64; 2]]) -> Option<Affine2d> {
    if source.len() != destination.len() || source.len() < 3 {
        return None;
    }
    let mut normal_x = [[0.0; 4]; 3];
    let mut normal_y = [[0.0; 4]; 3];
    for (source, destination) in source.iter().zip(destination) {
        let row = [source[0], source[1], 1.0];
        for i in 0..3 {
            for j in 0..3 {
                normal_x[i][j] += row[i] * row[j];
                normal_y[i][j] += row[i] * row[j];
            }
            normal_x[i][3] += row[i] * destination[0];
            normal_y[i][3] += row[i] * destination[1];
        }
    }
    let x = solve_3x3(normal_x)?;
    let y = solve_3x3(normal_y)?;
    Some(Affine2d {
        matrix: [[x[0], x[1]], [y[0], y[1]]],
        translation: [x[2], y[2]],
    })
}

fn point_fit_rms(mapping: Affine2d, source: &[[f64; 2]], destination: &[[f64; 2]]) -> f64 {
    (source
        .iter()
        .zip(destination)
        .map(|(source, destination)| {
            let predicted = mapping.apply(*source);
            (predicted[0] - destination[0]).powi(2) + (predicted[1] - destination[1]).powi(2)
        })
        .sum::<f64>()
        / source.len() as f64)
        .sqrt()
}

fn local_pixels_per_mm(mapping: Affine2d) -> f64 {
    let x = mapping.matrix[0][0].hypot(mapping.matrix[1][0]);
    let y = mapping.matrix[0][1].hypot(mapping.matrix[1][1]);
    (x + y) / 2.0
}

fn sample_rgb(image: &RgbImage, x: f64, y: f64) -> Option<[f64; 3]> {
    if x < 0.0 || y < 0.0 || x >= f64::from(image.width()) || y >= f64::from(image.height()) {
        return None;
    }
    let pixel = image.get_pixel(
        x.round().clamp(0.0, f64::from(image.width() - 1)) as u32,
        y.round().clamp(0.0, f64::from(image.height() - 1)) as u32,
    );
    Some(pixel.0.map(f64::from))
}

fn color_distance(left: [f64; 3], right: [f64; 3]) -> f64 {
    (left[0] - right[0]).hypot((left[1] - right[1]).hypot(left[2] - right[2]))
}

fn angle_distance(left: f64, right: f64) -> f64 {
    let difference = (left - right).rem_euclid(std::f64::consts::TAU);
    difference.min(std::f64::consts::TAU - difference)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ExtendedColorType, ImageEncoder, Rgb};

    use crate::calibration::{
        ManifestIdentity, TargetDiagnostic, flatbed_calibration, flatbed_validation,
        render_print_raster,
    };

    #[derive(Clone, Copy)]
    struct FixtureOptions {
        scale: f64,
        rotation_degrees: f64,
        shear: f64,
        upside_down: bool,
        color_cast: bool,
        dirty_backing: bool,
        liner_noise: bool,
        torn_bridge: bool,
        missing_index: Option<usize>,
        cut_offset_mm: [f64; 2],
        clipped_top_mm: f64,
    }

    impl Default for FixtureOptions {
        fn default() -> Self {
            Self {
                scale: 12.0,
                rotation_degrees: 0.0,
                shear: 0.0,
                upside_down: false,
                color_cast: false,
                dirty_backing: false,
                liner_noise: false,
                torn_bridge: false,
                missing_index: None,
                cut_offset_mm: [0.0, 0.0],
                clipped_top_mm: 0.0,
            }
        }
    }

    fn fixture(manifest: &TargetManifest, options: FixtureOptions) -> RgbImage {
        let raster = render_print_raster(manifest).unwrap();
        let angle = options.rotation_degrees.to_radians();
        let (sin, cos) = angle.sin_cos();
        let sign = if options.upside_down { -1.0 } else { 1.0 };
        let linear = [
            [
                sign * options.scale * cos,
                sign * (-options.scale * sin + options.shear),
            ],
            [
                sign * options.scale * sin,
                sign * options.scale * cos * 1.004,
            ],
        ];
        let corners = [
            [0.0, 0.0],
            [manifest.canvas.width_mm, 0.0],
            [0.0, manifest.canvas.height_mm],
            [manifest.canvas.width_mm, manifest.canvas.height_mm],
        ];
        let transformed = corners.map(|point| {
            [
                linear[0][0] * point[0] + linear[0][1] * point[1],
                linear[1][0] * point[0] + linear[1][1] * point[1],
            ]
        });
        let minimum = [
            transformed
                .iter()
                .map(|point| point[0])
                .fold(f64::INFINITY, f64::min),
            transformed
                .iter()
                .map(|point| point[1])
                .fold(f64::INFINITY, f64::min),
        ];
        let maximum = [
            transformed
                .iter()
                .map(|point| point[0])
                .fold(f64::NEG_INFINITY, f64::max),
            transformed
                .iter()
                .map(|point| point[1])
                .fold(f64::NEG_INFINITY, f64::max),
        ];
        let margin = 24.0;
        let print_to_scan = Affine2d {
            matrix: linear,
            translation: [margin - minimum[0], margin - minimum[1]],
        };
        let scan_to_print = print_to_scan.inverse().unwrap();
        let width = (maximum[0] - minimum[0] + 2.0 * margin).ceil() as u32;
        let height = (maximum[1] - minimum[1] + 2.0 * margin).ceil() as u32;
        let backing = [238_u8, 244, 232];
        let backing_control = manifest
            .diagnostics
            .iter()
            .find_map(|diagnostic| {
                if let TargetDiagnostic::BackingSample {
                    center_mm,
                    diameter_mm,
                    ..
                } = diagnostic
                {
                    Some(([center_mm.x, center_mm.y], diameter_mm / 2.0))
                } else {
                    None
                }
            })
            .unwrap();
        let mut output = RgbImage::from_pixel(width, height, Rgb([250, 250, 247]));
        for y in 0..height {
            for x in 0..width {
                let mm = scan_to_print.apply([f64::from(x), f64::from(y)]);
                if mm[0] < 0.0
                    || mm[1] < 0.0
                    || mm[0] >= manifest.canvas.width_mm
                    || mm[1] >= manifest.canvas.height_mm
                {
                    continue;
                }
                let raster_point = manifest
                    .canvas
                    .mm_to_raster(crate::calibration::MmPoint::new(mm[0], mm[1]));
                let rx = raster_point[0]
                    .round()
                    .clamp(0.0, f64::from(raster.width() - 1)) as u32;
                let ry = raster_point[1]
                    .round()
                    .clamp(0.0, f64::from(raster.height() - 1)) as u32;
                let mut color = raster.get_pixel(rx, ry).0;
                if mm[1] < options.clipped_top_mm {
                    color = [255, 255, 255];
                }
                let mut in_hole = false;
                for (index, target) in manifest
                    .targets
                    .iter()
                    .filter(|target| target.kind == TargetKind::FlatbedAperture)
                    .enumerate()
                {
                    if options.missing_index == Some(index) {
                        continue;
                    }
                    let center = [
                        target.center_mm.x + options.cut_offset_mm[0],
                        target.center_mm.y + options.cut_offset_mm[1],
                    ];
                    let dx = mm[0] - center[0];
                    let dy = mm[1] - center[1];
                    let radius = target.nominal_cut_bounds_mm.width / 2.0;
                    if dx.hypot(dy) <= radius {
                        in_hole = true;
                        if options.torn_bridge {
                            let angle = dy.atan2(dx);
                            if angle_distance(angle, 32_f64.to_radians()) < 5_f64.to_radians()
                                && dx.hypot(dy) > radius - 0.9
                            {
                                in_hole = false;
                            }
                        }
                    }
                }
                if (mm[0] - backing_control.0[0]).hypot(mm[1] - backing_control.0[1])
                    <= backing_control.1
                {
                    in_hole = true;
                }
                if in_hole {
                    color = backing;
                    if options.liner_noise && ((mm[0] + mm[1] * 0.33) * 4.0).rem_euclid(29.0) < 0.25
                    {
                        color = [150, 157, 147];
                    }
                    if options.dirty_backing && (u64::from(x) * 17 + u64::from(y) * 31) % 1301 < 3 {
                        color = [95, 88, 82];
                    }
                }
                if options.color_cast {
                    color = [
                        (f64::from(color[0]) * 0.93 + 8.0).min(255.0) as u8,
                        (f64::from(color[1]) * 0.97 + 3.0).min(255.0) as u8,
                        (f64::from(color[2]) * 1.02).min(255.0) as u8,
                    ];
                }
                output.put_pixel(x, y, Rgb(color));
            }
        }
        output
    }

    fn encode_png(image: &RgbImage) -> Vec<u8> {
        let mut bytes = Vec::new();
        image::codecs::png::PngEncoder::new(&mut bytes)
            .write_image(
                image.as_raw(),
                image.width(),
                image.height(),
                ExtendedColorType::Rgb8,
            )
            .unwrap();
        bytes
    }

    fn encode_jpeg(image: &RgbImage) -> Vec<u8> {
        let mut bytes = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, 96)
            .encode(
                image.as_raw(),
                image.width(),
                image.height(),
                ExtendedColorType::Rgb8,
            )
            .unwrap();
        bytes
    }

    fn place_on_bed(image: &RgbImage, dimensions: [u32; 2], offset: [u32; 2]) -> RgbImage {
        assert!(offset[0] + image.width() <= dimensions[0]);
        assert!(offset[1] + image.height() <= dimensions[1]);
        let mut bed = RgbImage::from_pixel(dimensions[0], dimensions[1], Rgb([246, 246, 243]));
        image::imageops::replace(&mut bed, image, i64::from(offset[0]), i64::from(offset[1]));
        bed
    }

    fn rewrite_png_dimensions(mut bytes: Vec<u8>, width: u32, height: u32) -> Vec<u8> {
        // PNG signature (8), length (4), then the IHDR type/data. Recompute
        // the chunk CRC so the decoder accepts the deliberately huge header.
        bytes[16..20].copy_from_slice(&width.to_be_bytes());
        bytes[20..24].copy_from_slice(&height.to_be_bytes());
        let mut crc = 0xffff_ffff_u32;
        for &byte in &bytes[12..29] {
            crc ^= u32::from(byte);
            for _ in 0..8 {
                crc = (crc >> 1) ^ (0xedb8_8320_u32 & (0_u32.wrapping_sub(crc & 1)));
            }
        }
        bytes[29..33].copy_from_slice(&(!crc).to_be_bytes());
        bytes
    }

    fn manifest() -> TargetManifest {
        flatbed_calibration(ManifestIdentity::stock("original-synthetic-scan")).unwrap()
    }

    #[test]
    fn rejects_non_png_jpeg_and_undersized_images() {
        let manifest = manifest();
        assert!(matches!(
            analyze_flatbed_scan(b"BMfake-bitmap", &manifest, ScanAnalysisConfig::default()),
            Err(ScanAnalysisError::UnsupportedFormat)
        ));
        let tiny = RgbImage::from_pixel(100, 100, Rgb([255, 255, 255]));
        assert!(matches!(
            analyze_flatbed_scan(&encode_png(&tiny), &manifest, ScanAnalysisConfig::default()),
            Err(ScanAnalysisError::TooSmall)
        ));
    }

    #[test]
    fn rejects_oversized_png_from_header_before_pixel_decode() {
        let manifest = manifest();
        let tiny = RgbImage::from_pixel(1, 1, Rgb([255, 255, 255]));
        let oversized_header = rewrite_png_dimensions(encode_png(&tiny), 10_000, 10_000);
        assert!(matches!(
            analyze_flatbed_scan(&oversized_header, &manifest, ScanAnalysisConfig::default()),
            Err(ScanAnalysisError::TooLarge)
        ));
    }

    #[test]
    fn uncropped_full_bed_scan_with_asymmetric_margins_is_localized() {
        let manifest = manifest();
        let sheet = fixture(
            &manifest,
            FixtureOptions {
                scale: 12.0,
                cut_offset_mm: [0.24, -0.18],
                ..FixtureOptions::default()
            },
        );
        let bed = place_on_bed(&sheet, [2_700, 3_200], [610, 360]);
        let report =
            analyze_flatbed_scan(&encode_png(&bed), &manifest, ScanAnalysisConfig::default())
                .unwrap();
        assert_eq!(report.orientation, ScanOrientation::Degrees0);
        assert!(report.accepted_count() >= 8, "{:#?}", report.targets);
    }

    #[test]
    fn uncropped_bed_resolves_ninety_and_two_seventy_degree_sheets() {
        let manifest = manifest();
        for (rotation_degrees, expected, dimensions, offset) in [
            (90.0, ScanOrientation::Degrees90, [3_300, 2_800], [240, 800]),
            (
                270.0,
                ScanOrientation::Degrees270,
                [3_300, 2_800],
                [780, 260],
            ),
        ] {
            let sheet = fixture(
                &manifest,
                FixtureOptions {
                    scale: 12.0,
                    rotation_degrees,
                    ..FixtureOptions::default()
                },
            );
            let bed = place_on_bed(&sheet, dimensions, offset);
            let report =
                analyze_flatbed_scan(&encode_png(&bed), &manifest, ScanAnalysisConfig::default())
                    .unwrap_or_else(|error| {
                        panic!("{rotation_degrees} degree scan failed: {error}")
                    });
            assert_eq!(report.orientation, expected);
            assert!(
                report.accepted_count() >= 8,
                "rotation {rotation_degrees}: {:#?}",
                report.targets
            );
        }
    }

    #[test]
    fn clean_600_dpi_fixture_localizes_below_one_print_pixel() {
        let manifest = manifest();
        let image = fixture(
            &manifest,
            FixtureOptions {
                scale: 600.0 / 25.4,
                cut_offset_mm: [0.31, -0.22],
                ..FixtureOptions::default()
            },
        );
        let report = analyze_flatbed_scan(
            &encode_png(&image),
            &manifest,
            ScanAnalysisConfig::default(),
        )
        .unwrap();
        assert_eq!(report.format, ScanImageFormat::Png);
        assert_eq!(report.orientation, ScanOrientation::Degrees0);
        assert!(report.accepted_count() >= 8, "{:#?}", report.targets);
        let maximum_center_error = report
            .targets
            .iter()
            .filter(|target| target.status == ScanTargetStatus::Accepted)
            .map(|target| {
                let actual = target.observed_center_mm.unwrap();
                (actual[0] - target.expected_center_mm[0] - 0.31)
                    .hypot(actual[1] - target.expected_center_mm[1] + 0.22)
            })
            .fold(0.0_f64, f64::max);
        assert!(
            maximum_center_error < 0.0847,
            "center error {maximum_center_error}"
        );
        assert_eq!(report.run_binding_sha1.len(), 40);
    }

    #[test]
    fn rejects_scan_from_a_different_run_or_purpose() {
        let printed = manifest();
        let image = fixture(&printed, FixtureOptions::default());
        let encoded = encode_png(&image);

        let wrong_run =
            flatbed_calibration(ManifestIdentity::stock("different-physical-run")).unwrap();
        assert!(matches!(
            analyze_flatbed_scan(&encoded, &wrong_run, ScanAnalysisConfig::default()),
            Err(ScanAnalysisError::RunBindingMismatch)
        ));

        let wrong_purpose = flatbed_validation(ManifestIdentity::stock(
            "original-synthetic-scan-validation",
        ))
        .unwrap();
        assert!(matches!(
            analyze_flatbed_scan(&encoded, &wrong_purpose, ScanAnalysisConfig::default()),
            Err(ScanAnalysisError::RunBindingMismatch)
        ));

        let mut next_candidate_identity = ManifestIdentity::stock("original-synthetic-scan");
        next_candidate_identity.candidate_generation = 1;
        let next_candidate = flatbed_calibration(next_candidate_identity).unwrap();
        assert!(matches!(
            analyze_flatbed_scan(&encoded, &next_candidate, ScanAnalysisConfig::default()),
            Err(ScanAnalysisError::RunBindingMismatch)
        ));
    }

    #[test]
    fn run_binding_survives_known_top_imaging_strip_loss() {
        let manifest = manifest();
        let image = fixture(
            &manifest,
            FixtureOptions {
                clipped_top_mm: 1.5,
                ..FixtureOptions::default()
            },
        );
        let report = analyze_flatbed_scan(
            &encode_png(&image),
            &manifest,
            ScanAnalysisConfig::default(),
        )
        .unwrap();
        assert_eq!(report.run_binding_sha1.len(), 40);
        assert!(report.accepted_count() >= 8);
    }

    #[test]
    fn skew_color_cast_dirty_backing_liner_noise_and_torn_bridge_remain_reviewable() {
        let manifest = manifest();
        let image = fixture(
            &manifest,
            FixtureOptions {
                scale: 12.0,
                rotation_degrees: 1.4,
                shear: 0.12,
                color_cast: true,
                dirty_backing: true,
                liner_noise: true,
                torn_bridge: true,
                cut_offset_mm: [-0.27, 0.34],
                ..FixtureOptions::default()
            },
        );
        let report = analyze_flatbed_scan(
            &encode_jpeg(&image),
            &manifest,
            ScanAnalysisConfig::default(),
        )
        .unwrap();
        assert_eq!(report.format, ScanImageFormat::Jpeg);
        assert!(report.accepted_count() >= 8, "{:#?}", report.targets);
        assert!(
            report.targets.iter().all(|target| target.status
                != ScanTargetStatus::Missing(ScanFailureReason::RetainedSlug))
        );
    }

    #[test]
    fn asymmetric_fiducials_resolve_upside_down_orientation_and_missing_slug() {
        let manifest = manifest();
        let image = fixture(
            &manifest,
            FixtureOptions {
                upside_down: true,
                missing_index: Some(3),
                ..FixtureOptions::default()
            },
        );
        let report = analyze_flatbed_scan(
            &encode_png(&image),
            &manifest,
            ScanAnalysisConfig::default(),
        )
        .unwrap();
        assert_eq!(report.orientation, ScanOrientation::Degrees180);
        assert_eq!(
            report.targets[3].status,
            ScanTargetStatus::Missing(ScanFailureReason::RetainedSlug)
        );
        assert!(report.targets.iter().enumerate().filter(|(index, target)| *index != 3 && target.status == ScanTargetStatus::Accepted).count() >= 8);
    }
}
