//! Print-to-cut calibration primitives shared by all measurement workflows.
//!
//! Public coordinates are always unmirrored canvas coordinates. A stored
//! [`CanvasToPlotter`] maps those coordinates directly to plotter units.

mod profile;
mod report;
mod scan;
mod solver;
mod targets;
mod transform;
mod wizard;

pub use profile::*;
pub use report::*;
pub use scan::*;
pub use solver::*;
pub use targets::*;
pub use transform::*;
pub use wizard::*;

use serde::{Deserialize, Serialize};

pub const CALIBRATION_SCHEMA_VERSION: u8 = 1;
pub(crate) const MAX_CALIBRATION_TEXT_BYTES: usize = 256;

pub(crate) fn bounded_calibration_text(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.len() <= MAX_CALIBRATION_TEXT_BYTES {
        return trimmed.to_owned();
    }
    let mut end = MAX_CALIBRATION_TEXT_BYTES;
    while !trimmed.is_char_boundary(end) {
        end -= 1;
    }
    trimmed[..end].to_owned()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CalibrationMethod {
    FlatbedScanner,
    ManualEastBay,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CalibrationObservation {
    pub target_id: String,
    /// Identifies the independently loaded physical sheet that supplied this observation.
    pub sheet_id: String,
    pub nominal_print_mm: [f64; 2],
    pub observed_cut_mm: [f64; 2],
    pub uncertainty_mm: [f64; 2],
    pub confidence: f64,
    pub included: bool,
}

impl CalibrationObservation {
    pub fn sanitize(&mut self) {
        self.target_id = bounded_calibration_text(&self.target_id);
        self.sheet_id = bounded_calibration_text(&self.sheet_id);
    }

    pub fn is_valid(&self) -> bool {
        !self.target_id.trim().is_empty()
            && !self.sheet_id.trim().is_empty()
            && self.target_id.len() <= MAX_CALIBRATION_TEXT_BYTES
            && self.sheet_id.len() <= MAX_CALIBRATION_TEXT_BYTES
            && self.nominal_print_mm.into_iter().all(f64::is_finite)
            && self.observed_cut_mm.into_iter().all(f64::is_finite)
            && self
                .uncertainty_mm
                .into_iter()
                .all(|value| value.is_finite() && value > 0.0 && value <= 10.0)
            && self.confidence.is_finite()
            && (0.0..=1.0).contains(&self.confidence)
    }
}

/// The four measurements used by the Manual East Bay workflow.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ManualEdgeMeasurement {
    pub left_mm: f64,
    pub right_mm: f64,
    pub top_mm: f64,
    pub bottom_mm: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ManualDerivedMeasurement {
    pub displacement_mm: [f64; 2],
    pub cut_size_mm: [f64; 2],
}

impl ManualEdgeMeasurement {
    /// Derive a cut center in printed logical coordinates.
    ///
    /// `print_scale` is measured/expected for the H80 and V150 bars. Passing
    /// `[1, 1]` preserves raw physical measurements when scale was skipped.
    pub fn derive(self, print_scale: [f64; 2]) -> Option<ManualDerivedMeasurement> {
        let values = [self.left_mm, self.right_mm, self.top_mm, self.bottom_mm];
        if !values.into_iter().all(|v| v.is_finite() && v >= 0.0)
            || !print_scale.into_iter().all(|v| v.is_finite() && v > 0.0)
        {
            return None;
        }
        Some(ManualDerivedMeasurement {
            displacement_mm: [
                (self.right_mm - self.left_mm) / (2.0 * print_scale[0]),
                (self.bottom_mm - self.top_mm) / (2.0 * print_scale[1]),
            ],
            cut_size_mm: [self.left_mm + self.right_mm, self.top_mm + self.bottom_mm],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_measurement_signs_and_scale_are_explicit() {
        let result = ManualEdgeMeasurement {
            left_mm: 6.6,
            right_mm: 7.4,
            top_mm: 7.3,
            bottom_mm: 6.7,
        }
        .derive([0.8, 1.2])
        .unwrap();

        assert!((result.displacement_mm[0] - 0.5).abs() < 1e-12);
        assert!((result.displacement_mm[1] + 0.25).abs() < 1e-12);
        assert_eq!(result.cut_size_mm, [14.0, 14.0]);
    }

    #[test]
    fn manual_measurement_rejects_invalid_physical_values() {
        let invalid = ManualEdgeMeasurement {
            left_mm: f64::NAN,
            right_mm: 7.0,
            top_mm: 7.0,
            bottom_mm: 7.0,
        };
        assert!(invalid.derive([1.0, 1.0]).is_none());
        let valid = ManualEdgeMeasurement {
            left_mm: 7.0,
            right_mm: 7.0,
            top_mm: 7.0,
            bottom_mm: 7.0,
        };
        assert!(valid.derive([0.0, 1.0]).is_none());
    }
}
