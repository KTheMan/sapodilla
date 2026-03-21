use std::collections::HashMap;

use egui::Vec2;
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use geo::{
    Buffer, ChaikinSmoothing, Contains, Coord, Euclidean, Intersects, LineString, MultiPolygon,
    Polygon, Rect, Scale, SimplifyVwPreserve, Validation, Winding, coord,
    line_measures::LengthMeasurable,
};
use image::{
    GrayImage, Luma,
    imageops::{self, FilterType},
};
use imageproc::contours::BorderType;
use itertools::Itertools;
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
    images: Vec<LoadedImage>,
    tuning: CutTuning,
    canvas_size: &'static CanvasSize,
}

impl CutGenerator {
    pub fn start(
        images: Vec<LoadedImage>,
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

        let has_intersections = line_strings
            .iter()
            .combinations(2)
            .any(|polygons| polygons[0].intersects(polygons[1]));

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

    fn image(&self, image: &LoadedImage) -> Vec<LineString<f32>> {
        trace!("starting processing image");

        // Resize image to the expected dimensions. Doesn't need to be a high
        // quality resize, so nearest filter is fine.
        let size = image.size();
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

        // Keep track of the outer parts of contours separately from holes, so
        // we can construct a MultiPolygon with an exterior and interiors.
        let mut outers = HashMap::new();
        let mut holes: HashMap<usize, Vec<LineString<f32>>> = HashMap::new();
        let mut frame_index: Option<usize> = None;

        for (index, contour) in contours.into_iter().enumerate() {
            // Create the line from the points in the contour, offset by the
            // position of the image in the canvas. We need to have these
            // offsets here to check if anything overlaps.
            let mut line_string = LineString::from_iter(contour.points.into_iter().map(|point| {
                (
                    point.x as f32 + image.offset.x,
                    point.y as f32 + image.offset.y,
                )
            }));

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
            LineString::new(coords.into_iter().map(|coord| coord + offset).collect())
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
