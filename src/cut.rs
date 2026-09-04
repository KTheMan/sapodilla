use std::collections::HashMap;

use egui::{Pos2, Vec2};
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use geo::{
    BoundingRect, Buffer, ChaikinSmoothing, Contains, Coord, Euclidean, Intersects, LineString,
    MultiPolygon, Polygon, Rect, Scale, SimplifyVwPreserve, Validation, Winding, coord,
    line_measures::LengthMeasurable,
};
use image::{
    GrayImage, Luma,
    imageops::{self, FilterType},
};
use imageproc::contours::BorderType;
use tracing::{debug, error, trace, warn};

use crate::{app::LoadedImage, protocol::CanvasSize, spawn_blocking};

#[derive(Debug)]
pub enum CutAction {
    Progress { completed: usize, total: usize },
    Done(CutResult),
}

#[derive(Debug)]
pub struct CutResult {
    pub has_intersections: bool,
    pub off_canvas: bool,
    pub line_strings: Vec<LineString<f32>>,
}

#[derive(Clone)]
pub struct CutTuning {
    pub buffer: f32,
    pub minimum_length: f32,
    pub smoothing: usize,
    pub simplify: f32,
    pub internal: bool,
    pub white_transparent: bool,
}

/// Lead-in/lead-out geometry around a closed contour seam.
///
/// The PixCut host generator approaches and leaves the seam on short ramps so
/// the blade does not leave a connected tab where the contour closes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OvercutSettings {
    pub enabled: bool,
    pub steps: usize,
    pub maximum_angle_degrees: f32,
    pub reach_pixels: f32,
    pub snap_to_pixels: bool,
}

impl Default for OvercutSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            steps: 3,
            maximum_angle_degrees: 45.0,
            reach_pixels: 15.0,
            snap_to_pixels: true,
        }
    }
}

/// Close a contour and add mirrored approach/departure ramps at its seam.
pub fn apply_overcut(path: &LineString<f32>, settings: OvercutSettings) -> LineString<f32> {
    let mut contour = path.0.clone();
    if contour.len() >= 2 && contour.first() == contour.last() {
        contour.pop();
    }
    if contour.len() < 2 {
        return LineString::new(contour);
    }

    let seam = contour[0];
    let previous = *contour.last().expect("contour has at least two points");
    let next = contour[1];
    let closing = normalize(Coord {
        x: seam.x - previous.x,
        y: seam.y - previous.y,
    });
    let reverse_outgoing = normalize(Coord {
        x: seam.x - next.x,
        y: seam.y - next.y,
    });

    let mut closed = contour;
    closed.push(seam);
    if !settings.enabled
        || settings.steps == 0
        || settings.reach_pixels <= 0.0
        || closing == (Coord { x: 0.0, y: 0.0 })
    {
        return LineString::new(closed);
    }

    let approach_direction = Coord {
        x: -closing.x,
        y: -closing.y,
    };
    let mut approach = overcut_ramp(seam, approach_direction, reverse_outgoing, settings);
    approach.reverse();
    let departure = overcut_ramp(seam, closing, reverse_outgoing, settings);

    approach.extend(closed);
    approach.extend(departure);
    LineString::new(approach)
}

fn normalize(vector: Coord<f32>) -> Coord<f32> {
    let length = vector.x.hypot(vector.y);
    if length <= f32::EPSILON {
        Coord { x: 0.0, y: 0.0 }
    } else {
        Coord {
            x: vector.x / length,
            y: vector.y / length,
        }
    }
}

fn overcut_ramp(
    seam: Coord<f32>,
    along: Coord<f32>,
    toward: Coord<f32>,
    settings: OvercutSettings,
) -> Vec<Coord<f32>> {
    let projection = along.x * toward.x + along.y * toward.y;
    let rejected = Coord {
        x: toward.x - along.x * projection,
        y: toward.y - along.y * projection,
    };
    let perpendicular = if rejected.x.hypot(rejected.y) < 1e-6 {
        Coord {
            x: -along.y,
            y: along.x,
        }
    } else {
        normalize(rejected)
    };

    (1..=settings.steps)
        .map(|step| {
            let fraction = step as f32 / settings.steps as f32;
            let angle = (settings.maximum_angle_degrees * fraction).to_radians();
            let radius = settings.reach_pixels * fraction;
            let mut point = Coord {
                x: seam.x + radius * (along.x * angle.cos() + perpendicular.x * angle.sin()),
                y: seam.y + radius * (along.y * angle.cos() + perpendicular.y * angle.sin()),
            };
            if settings.snap_to_pixels {
                point.x = point.x.floor();
                point.y = point.y.floor();
            }
            point
        })
        .collect()
}

impl Default for CutTuning {
    fn default() -> Self {
        Self {
            buffer: 300.0 / 25.4,         // 1mm
            minimum_length: 0.25 * 300.0, // 1/4in
            smoothing: 2,
            simplify: 1.5,
            internal: false,
            white_transparent: true,
        }
    }
}

pub struct CutGenerator {
    tx: UnboundedSender<CutAction>,
    images: Vec<CutImage>,
    tuning: CutTuning,
    canvas_size: &'static CanvasSize,
}

/// Minimal immutable snapshot needed by the cut worker. Avoid cloning the
/// source/original raster and GPU texture handle along with every current
/// raster when a generation starts.
pub(crate) struct CutImage {
    image: image::RgbaImage,
    size: Vec2,
    offset: Pos2,
    rotation_degrees: f32,
}

impl From<&LoadedImage> for CutImage {
    fn from(image: &LoadedImage) -> Self {
        Self {
            image: image.image.clone(),
            size: image.size(),
            offset: image.offset,
            rotation_degrees: image.rotation_degrees,
        }
    }
}

impl CutGenerator {
    pub fn start(
        images: Vec<CutImage>,
        tuning: CutTuning,
        canvas_size: &'static CanvasSize,
    ) -> UnboundedReceiver<CutAction> {
        let (tx, rx) = unbounded();

        let cut_generator = Self {
            tx,
            images,
            tuning,
            canvas_size,
        };

        spawn_blocking(move || {
            if let Err(err) = cut_generator.process() {
                error!("could not process cuts: {err}");
            }
        });

        rx
    }

    fn process(self) -> anyhow::Result<()> {
        let total = self.images.len();

        self.tx.unbounded_send(CutAction::Progress {
            completed: 0,
            total,
        })?;

        let mut line_strings = Vec::new();

        for (index, image) in self.images.iter().enumerate() {
            let paths = self.image(image);
            line_strings.extend_from_slice(&paths);

            self.tx.unbounded_send(CutAction::Progress {
                completed: index + 1,
                total,
            })?;
        }

        let has_intersections = has_any_intersections(&line_strings);

        let offset = (self.canvas_size.size - self.canvas_size.safe_area) / 2.0;

        let canvas_polygon = Rect::new(
            coord! { x: offset.x, y: offset.y },
            coord! { x: self.canvas_size.size.x - offset.x, y: self.canvas_size.size.y - offset.y },
        )
        .to_polygon();

        let off_canvas = line_strings
            .iter()
            .any(|polygons| !canvas_polygon.contains(polygons));

        self.tx.unbounded_send(CutAction::Done(CutResult {
            has_intersections,
            off_canvas,
            line_strings,
        }))?;

        Ok(())
    }

    fn image(&self, image: &CutImage) -> Vec<LineString<f32>> {
        trace!("starting processing image");

        // Preserve the antialiased edge topology used by contour extraction.
        // Nearest-neighbor sampling can materially change thresholded curves
        // and diagonals.
        let size = image.size;
        let resized = imageops::resize(
            &image.image,
            size.x as u32,
            size.y as u32,
            FilterType::Gaussian,
        );

        let threshold = if self.tuning.white_transparent {
            // Invert the colors, unlike a normal image we need blacks to be visible
            // but don't care about white. Normally transparent pixels turn black
            // but we need them to be white for our inversion.
            let mut im = image::ImageBuffer::from_pixel(
                resized.width(),
                resized.height(),
                image::Rgba([255, 255, 255, 255]),
            );
            image::imageops::overlay(&mut im, &resized, 0, 0);
            imageops::colorops::invert(&mut im);

            let grayscale = imageops::grayscale(&im);

            imageproc::contrast::threshold(
                &grayscale,
                5,
                imageproc::contrast::ThresholdType::Binary,
            )
        } else {
            GrayImage::from_fn(resized.width(), resized.height(), |x, y| {
                let image::Rgba([_, _, _, alpha]) = resized.get_pixel(x, y);

                if *alpha > 127 { Luma([255]) } else { Luma([0]) }
            })
        };

        let contours = imageproc::contours::find_contours::<u32>(&threshold);

        let center = image.offset + size / 2.0;
        let radians = image.rotation_degrees.to_radians();
        let (sin, cos) = radians.sin_cos();
        let transform = |point: imageproc::point::Point<u32>| {
            let x = point.x as f32 + image.offset.x;
            let y = point.y as f32 + image.offset.y;
            let dx = x - center.x;
            let dy = y - center.y;
            (
                center.x + cos * dx - sin * dy,
                center.y + sin * dx + cos * dy,
            )
        };

        // Keep track of the outer parts of contours separately from holes, so
        // we can construct a MultiPolygon with an exterior and interiors.
        let mut outers = HashMap::new();
        let mut holes: HashMap<usize, Vec<LineString<f32>>> = HashMap::new();
        let mut frame_index: Option<usize> = None;

        for (index, contour) in contours.into_iter().enumerate() {
            // Create the line from the points in the contour, offset by the
            // position of the image in the canvas. We need to have these
            // offsets here to check if anything overlaps.
            let mut line_string = LineString::from_iter(contour.points.into_iter().map(&transform));

            line_string.close();

            if let Err(err) = line_string.check_validation() {
                warn!("line string was not valid: {err}",);
                continue;
            }

            // Based on the border type, determine where to put this polygon.
            // It's also possible for a hole to not have a parent, and in those
            // cases we need to add the whole image frame as an outer line.
            match contour.border_type {
                BorderType::Outer => {
                    line_string.make_cw_winding();
                    debug!(index, "adding outer");
                    outers.insert(index, line_string);
                }
                BorderType::Hole => {
                    line_string.make_ccw_winding();
                    let hole_key = contour.parent.unwrap_or(*frame_index.get_or_insert(index));
                    holes.entry(hole_key).or_default().push(line_string);
                }
            }
        }

        let get_frame = || {
            let coords = [
                coord! { x: 0.0, y: 0.0 },
                coord! { x: 0.0, y: size.y },
                coord! { x: size.x, y: size.y },
                coord! { x: size.x, y: 0.0},
                coord! { x: 0.0, y: 0.0},
            ];

            let offset = coord! { x: image.offset.x, y: image.offset.y };
            let unrotated = coords.into_iter().map(|coord| coord + offset);
            LineString::new(unrotated.map(|point| {
                let dx = point.x - center.x;
                let dy = point.y - center.y;
                coord! { x: center.x + cos * dx - sin * dy, y: center.y + sin * dx + cos * dy }
            }).collect())
        };

        // If the outers is empty try to get the frame index, otherwise insert
        // the image frame at 0.
        if outers.is_empty() {
            outers.insert(frame_index.unwrap_or(0), get_frame());
        }

        // Create polygons from the line segments, only keeping interiors if we
        // want to cut them.
        let polygons: Vec<Polygon<f32>> = outers
            .into_iter()
            .map(|(index, line)| {
                Polygon::new(
                    line,
                    if self.tuning.internal {
                        holes.remove(&index).unwrap_or_default()
                    } else {
                        vec![]
                    },
                )
            })
            .collect();

        // Wrap up our polygons by expanding them as needed, applying
        // simplification and smoothing, and filtering out areas that ended up
        // smaller than we want to cut.
        MultiPolygon::new(polygons)
            .buffer(self.tuning.buffer)
            .simplify_vw_preserve(self.tuning.simplify)
            .chaikin_smoothing(self.tuning.smoothing)
            .into_iter()
            .filter_map(|polygon| {
                let (exterior, interiors) = polygon.into_inner();
                if exterior.length(&Euclidean) < self.tuning.minimum_length {
                    return None;
                }
                let interiors = self.filter_small_holes(interiors);
                Some(Polygon::new(exterior, interiors.collect()))
            })
            .flat_map(|polygon| {
                let (exterior, mut interiors) = polygon.into_inner();
                interiors.push(exterior);
                interiors
            })
            .collect()
    }

    fn filter_small_holes(
        &self,
        line_strings: impl IntoIterator<Item = LineString<f32>>,
    ) -> impl Iterator<Item = LineString<f32>> {
        line_strings.into_iter().filter(|line_string| {
            let length = line_string.length(&Euclidean);
            if length < self.tuning.minimum_length {
                debug!(
                    length,
                    minimum_length = self.tuning.minimum_length,
                    "interior length was too short"
                );
                false
            } else {
                debug!(interior_length = length);
                true
            }
        })
    }

    /// Mirror generated cut lines for sending to the device.
    #[allow(dead_code)]
    pub fn mirror_cuts<'a>(
        line_strings: impl IntoIterator<Item = &'a LineString<f32>>,
        canvas_size: Vec2,
    ) -> impl Iterator<Item = LineString<f32>> {
        let point = Coord::from((canvas_size.x, canvas_size.y / 2.0));

        line_strings
            .into_iter()
            .map(move |line_string| line_string.scale_around_point(1.0, -1.0, point))
    }
}

fn has_any_intersections(line_strings: &[LineString<f32>]) -> bool {
    let bounds = line_strings
        .iter()
        .map(LineString::bounding_rect)
        .collect::<Vec<_>>();
    for left in 0..line_strings.len() {
        let Some(left_bounds) = bounds[left] else {
            continue;
        };
        for right in left + 1..line_strings.len() {
            let Some(right_bounds) = bounds[right] else {
                continue;
            };
            if left_bounds.intersects(&right_bounds)
                && line_strings[left].intersects(&line_strings[right])
            {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn square() -> LineString<f32> {
        LineString::from(vec![
            (300.0, 1500.0),
            (300.0, 600.0),
            (900.0, 600.0),
            (900.0, 1500.0),
        ])
    }

    #[test]
    fn overcut_closes_contour_and_adds_ramps() {
        let result = apply_overcut(&square(), OvercutSettings::default());
        assert_eq!(result.0.len(), 3 + 4 + 1 + 3);
        assert_eq!(result.0[3], square().0[0]);
        assert_eq!(result.0[7], square().0[0]);
    }

    #[test]
    fn disabled_overcut_only_closes_the_contour() {
        let result = apply_overcut(
            &square(),
            OvercutSettings {
                enabled: false,
                ..Default::default()
            },
        );
        assert_eq!(result.0.len(), 5);
        assert_eq!(result.0.first(), result.0.last());
    }

    #[test]
    fn overcut_uses_requested_reach_and_mirrors_ramps() {
        let result = apply_overcut(
            &square(),
            OvercutSettings {
                steps: 2,
                reach_pixels: 100.0,
                snap_to_pixels: false,
                ..Default::default()
            },
        );
        let seam = square().0[0];
        let approach = [result.0[1], result.0[0]];
        let departure = &result.0[result.0.len() - 2..];
        for (incoming, outgoing) in approach.iter().zip(departure) {
            assert!(((incoming.x - seam.x) + (outgoing.x - seam.x)).abs() < 1e-3);
            assert!((incoming.y - outgoing.y).abs() < 1e-3);
        }
    }

    #[test]
    fn intersection_prefilter_preserves_exact_geometry_results() {
        let disjoint = LineString::from(vec![(0.0, 0.0), (2.0, 2.0)]);
        let crossing = LineString::from(vec![(0.0, 2.0), (2.0, 0.0)]);
        let far = LineString::from(vec![(100.0, 100.0), (102.0, 102.0)]);
        assert!(!has_any_intersections(&[disjoint.clone(), far.clone()]));
        assert!(has_any_intersections(&[
            disjoint.clone(),
            far,
            crossing.clone()
        ]));

        let mut paths = Vec::new();
        for index in 0..64 {
            let x = (index * 37 % 101) as f32;
            let y = (index * 53 % 97) as f32;
            paths.push(LineString::from(vec![(x, y), (x + 7.0, y + 11.0)]));
        }
        let expected = paths
            .iter()
            .enumerate()
            .any(|(left, path)| paths[left + 1..].iter().any(|other| path.intersects(other)));
        assert_eq!(has_any_intersections(&paths), expected);
    }

    #[test]
    fn raster_cut_snapshot_scales_and_closes_transparent_artwork() {
        let pixels = image::RgbaImage::from_fn(32, 24, |x, y| {
            if (5..27).contains(&x) && (4..20).contains(&y) {
                image::Rgba([220, 40, 90, 255])
            } else {
                image::Rgba([0, 0, 0, 0])
            }
        });
        let image = CutImage {
            image: pixels,
            size: Vec2::new(64.0, 48.0),
            offset: Pos2::new(10.0, 20.0),
            rotation_degrees: 0.0,
        };
        let (tx, _rx) = unbounded();
        let canvas = Box::leak(Box::new(CanvasSize {
            name: "test".into(),
            media_size: 0,
            media_type: 0,
            size: Vec2::new(100.0, 100.0),
            safe_area: Vec2::new(100.0, 100.0),
        }));
        let generator = CutGenerator {
            tx,
            images: Vec::new(),
            tuning: CutTuning {
                buffer: 0.0,
                minimum_length: 0.0,
                smoothing: 0,
                simplify: 0.0,
                internal: false,
                white_transparent: false,
            },
            canvas_size: canvas,
        };
        let paths = generator.image(&image);
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0].0.first(), paths[0].0.last());
        let bounds = paths[0].bounding_rect().unwrap();
        assert!(bounds.min().x >= 19.0 && bounds.max().x <= 65.0);
        assert!(bounds.min().y >= 27.0 && bounds.max().y <= 61.0);
    }
}
