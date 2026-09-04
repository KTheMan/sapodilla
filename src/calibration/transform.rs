use serde::{Deserialize, Serialize};
use thiserror::Error;

/// The exact f32 product used by Sapodilla's original PixCut S1 mapping.
pub const LEGACY_PIXCUT_S1_SCALE: f64 = (3.38667_f32 * 1.01333_f32) as f64;
pub const LEGACY_PIXCUT_S1_MAPPING_ID: &str = "pixcut-s1-stock-v1";

/// Direct affine mapping from unmirrored canvas coordinates to plotter units.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct CanvasToPlotter {
    pub matrix: [[f64; 2]; 2],
    pub translation: [f64; 2],
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TransformBounds {
    pub minimum_axis_scale: f64,
    pub maximum_axis_scale: f64,
    pub maximum_condition: f64,
    pub maximum_translation: f64,
}

impl Default for TransformBounds {
    fn default() -> Self {
        Self {
            minimum_axis_scale: 0.5,
            maximum_axis_scale: 10.0,
            maximum_condition: 100.0,
            maximum_translation: 100_000.0,
        }
    }
}

#[derive(Clone, Copy, Debug, Error, PartialEq)]
pub enum TransformError {
    #[error("transform contains a non-finite coefficient")]
    NonFinite,
    #[error("transform is singular or ill-conditioned")]
    Singular,
    #[error("transform is outside plausible calibration bounds")]
    Implausible,
}

impl CanvasToPlotter {
    pub const IDENTITY: Self = Self {
        matrix: [[1.0, 0.0], [0.0, 1.0]],
        translation: [0.0, 0.0],
    };

    /// Reifies the historical mirror/swap/scale/offset sequence as one affine transform.
    ///
    /// The legacy encoder first mirrored Y around `canvas_height_px`, then
    /// emitted `(y + offset_y) * scale` as the first plotter axis and
    /// `(x + offset_x) * scale` as the second. Keeping this conversion at the
    /// boundary lets all downstream code use one direct mapping.
    pub fn from_legacy_components(canvas_height_px: f64, scale: f64, offset_px: [f64; 2]) -> Self {
        Self {
            matrix: [[0.0, -scale], [scale, 0.0]],
            translation: [
                (canvas_height_px + offset_px[1]) * scale,
                offset_px[0] * scale,
            ],
        }
    }

    pub fn legacy_pixcut_s1(canvas_height_px: f64) -> Self {
        Self::from_legacy_components(canvas_height_px, LEGACY_PIXCUT_S1_SCALE, [-9.0, -13.0])
    }

    pub fn apply(self, point: [f64; 2]) -> [f64; 2] {
        [
            self.matrix[0][0] * point[0] + self.matrix[0][1] * point[1] + self.translation[0],
            self.matrix[1][0] * point[0] + self.matrix[1][1] * point[1] + self.translation[1],
        ]
    }

    /// Returns `self(before(point))`.
    pub fn compose(self, before: Self) -> Self {
        let a = self.matrix;
        let b = before.matrix;
        Self {
            matrix: [
                [
                    a[0][0] * b[0][0] + a[0][1] * b[1][0],
                    a[0][0] * b[0][1] + a[0][1] * b[1][1],
                ],
                [
                    a[1][0] * b[0][0] + a[1][1] * b[1][0],
                    a[1][0] * b[0][1] + a[1][1] * b[1][1],
                ],
            ],
            translation: [
                a[0][0] * before.translation[0]
                    + a[0][1] * before.translation[1]
                    + self.translation[0],
                a[1][0] * before.translation[0]
                    + a[1][1] * before.translation[1]
                    + self.translation[1],
            ],
        }
    }

    pub fn determinant(self) -> f64 {
        self.matrix[0][0] * self.matrix[1][1] - self.matrix[0][1] * self.matrix[1][0]
    }

    pub fn inverse(self) -> Result<Self, TransformError> {
        if !self.is_finite() {
            return Err(TransformError::NonFinite);
        }
        let determinant = self.determinant();
        let norm = self
            .matrix
            .into_iter()
            .flatten()
            .map(f64::abs)
            .fold(0.0, f64::max)
            .max(1.0);
        if determinant.abs() <= f64::EPSILON * norm * norm * 16.0 {
            return Err(TransformError::Singular);
        }
        let inverse_matrix = [
            [
                self.matrix[1][1] / determinant,
                -self.matrix[0][1] / determinant,
            ],
            [
                -self.matrix[1][0] / determinant,
                self.matrix[0][0] / determinant,
            ],
        ];
        let inverse = Self {
            matrix: inverse_matrix,
            translation: [
                -(inverse_matrix[0][0] * self.translation[0]
                    + inverse_matrix[0][1] * self.translation[1]),
                -(inverse_matrix[1][0] * self.translation[0]
                    + inverse_matrix[1][1] * self.translation[1]),
            ],
        };
        Ok(inverse)
    }

    pub fn is_finite(self) -> bool {
        self.matrix.into_iter().flatten().all(f64::is_finite)
            && self.translation.into_iter().all(f64::is_finite)
    }

    /// Ratio of the largest to smallest singular value of the linear part.
    pub fn condition_number(self) -> f64 {
        let a = self.matrix[0][0];
        let b = self.matrix[0][1];
        let c = self.matrix[1][0];
        let d = self.matrix[1][1];
        let trace = a * a + b * b + c * c + d * d;
        let det_squared = self.determinant().powi(2);
        let discriminant = (trace * trace - 4.0 * det_squared).max(0.0).sqrt();
        let largest = ((trace + discriminant) / 2.0).sqrt();
        let smallest = ((trace - discriminant) / 2.0).max(0.0).sqrt();
        if smallest <= f64::EPSILON {
            f64::INFINITY
        } else {
            largest / smallest
        }
    }

    pub fn validate(self, bounds: TransformBounds) -> Result<(), TransformError> {
        if !self.is_finite() {
            return Err(TransformError::NonFinite);
        }
        self.inverse()?;
        let column_scales = [
            self.matrix[0][0].hypot(self.matrix[1][0]),
            self.matrix[0][1].hypot(self.matrix[1][1]),
        ];
        if column_scales
            .into_iter()
            .any(|scale| scale < bounds.minimum_axis_scale || scale > bounds.maximum_axis_scale)
            || self.condition_number() > bounds.maximum_condition
            || self
                .translation
                .into_iter()
                .any(|value| value.abs() > bounds.maximum_translation)
        {
            return Err(TransformError::Implausible);
        }
        Ok(())
    }

    /// Compose a fitted physical response inverse before this baseline.
    /// Both mappings must use the same input unit.
    pub fn compensated_for(self, forward_response: Self) -> Result<Self, TransformError> {
        Ok(self.compose(forward_response.inverse()?))
    }

    /// Compose a physical millimetre response into a pixel-input baseline.
    ///
    /// Calibration observations and solver output are expressed in millimetres,
    /// while the current canvas and legacy plotter mapping use raster pixels.
    /// The explicit unit changes prevent a 1 mm correction from being applied
    /// as only one 300-DPI pixel.
    pub fn compensated_for_mm_response(
        self,
        forward_response_mm: Self,
        pixels_per_mm: f64,
    ) -> Result<Self, TransformError> {
        if !pixels_per_mm.is_finite() || pixels_per_mm <= 0.0 {
            return Err(TransformError::Implausible);
        }
        let pixels_to_mm = Self {
            matrix: [[1.0 / pixels_per_mm, 0.0], [0.0, 1.0 / pixels_per_mm]],
            translation: [0.0, 0.0],
        };
        let mm_to_pixels = Self {
            matrix: [[pixels_per_mm, 0.0], [0.0, pixels_per_mm]],
            translation: [0.0, 0.0],
        };
        let compensation_pixels =
            mm_to_pixels.compose(forward_response_mm.inverse()?.compose(pixels_to_mm));
        Ok(self.compose(compensation_pixels))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_point_close(actual: [f64; 2], expected: [f64; 2], epsilon: f64) {
        assert!(
            (actual[0] - expected[0]).abs() <= epsilon,
            "{actual:?} != {expected:?}"
        );
        assert!(
            (actual[1] - expected[1]).abs() <= epsilon,
            "{actual:?} != {expected:?}"
        );
    }

    #[test]
    fn legacy_mapping_reifies_the_current_hidden_sequence() {
        let mapping = CanvasToPlotter::legacy_pixcut_s1(2100.0);
        let scale = f64::from(3.38667_f32 * 1.01333_f32);
        for point in [[0.0, 0.0], [9.0, 13.0], [600.25, 1050.75], [1200.0, 2100.0]] {
            let legacy = [(2100.0 - point[1] - 13.0) * scale, (point[0] - 9.0) * scale];
            assert_point_close(mapping.apply(point), legacy, 1e-10);
        }
        assert_eq!(LEGACY_PIXCUT_S1_MAPPING_ID, "pixcut-s1-stock-v1");
    }

    #[test]
    fn arbitrary_legacy_components_preserve_axis_order_and_offsets() {
        let mapping = CanvasToPlotter::from_legacy_components(200.0, 2.0, [3.0, -5.0]);
        assert_point_close(mapping.apply([10.0, 20.0]), [350.0, 26.0], 1e-12);
    }

    #[test]
    fn inverse_and_composition_round_trip_quantitatively() {
        let transform = CanvasToPlotter {
            matrix: [[1.012, 0.007], [-0.004, 0.993]],
            translation: [0.42, -0.31],
        };
        let inverse = transform.inverse().unwrap();
        let identity = transform.compose(inverse);
        for point in [[0.0, 0.0], [50.0, 88.0], [101.6, 177.8]] {
            assert_point_close(identity.apply(point), point, 5e-14);
        }
    }

    #[test]
    fn compensation_applies_the_inverse_error_not_the_forward_error() {
        let baseline = CanvasToPlotter::legacy_pixcut_s1(2100.0);
        let response = CanvasToPlotter {
            matrix: [[1.01, 0.0], [0.0, 0.99]],
            translation: [0.8, -0.4],
        };
        let corrected = baseline.compensated_for(response).unwrap();
        let desired = [40.0, 120.0];
        let command = response.inverse().unwrap().apply(desired);
        assert_point_close(corrected.apply(desired), baseline.apply(command), 1e-10);
    }

    #[test]
    fn millimetre_response_is_converted_before_pixel_baseline_composition() {
        let pixels_per_mm = 300.0 / 25.4;
        let baseline = CanvasToPlotter::legacy_pixcut_s1(2100.0);
        let response_mm = CanvasToPlotter {
            matrix: [[1.0, 0.0], [0.0, 1.0]],
            translation: [1.0, -0.5],
        };
        let corrected = baseline
            .compensated_for_mm_response(response_mm, pixels_per_mm)
            .unwrap();
        let desired_px = [600.0, 1050.0];
        let command_px = [
            desired_px[0] - pixels_per_mm,
            desired_px[1] + 0.5 * pixels_per_mm,
        ];
        assert_point_close(
            corrected.apply(desired_px),
            baseline.apply(command_px),
            1e-9,
        );
    }

    #[test]
    fn validation_rejects_nonfinite_singular_and_implausible_transforms() {
        let bounds = TransformBounds::default();
        let mut transform = CanvasToPlotter::IDENTITY;
        transform.translation[0] = f64::NAN;
        assert_eq!(transform.validate(bounds), Err(TransformError::NonFinite));
        transform = CanvasToPlotter {
            matrix: [[1.0, 2.0], [2.0, 4.0]],
            translation: [0.0, 0.0],
        };
        assert_eq!(transform.validate(bounds), Err(TransformError::Singular));
        transform = CanvasToPlotter {
            matrix: [[100.0, 0.0], [0.0, 100.0]],
            translation: [0.0, 0.0],
        };
        assert_eq!(transform.validate(bounds), Err(TransformError::Implausible));
    }
}
