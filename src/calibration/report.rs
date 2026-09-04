//! Repeatable calibration-run diagnostics and metric calculation.

use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};

use super::{
    CalibrationMethod, CalibrationModel, CalibrationObservation, CalibrationProfile,
    CalibrationSolution, CanvasToPlotter, ErrorMetrics, PrinterCalibrationKey, TargetManifest,
    ValidationMetrics,
};

pub const CALIBRATION_REPORT_VERSION: u8 = 1;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CalibrationRunReport {
    pub version: u8,
    pub run_id: String,
    pub printer: PrinterCalibrationKey,
    pub method: CalibrationMethod,
    pub target_manifest: TargetManifest,
    pub baseline_profile_id: Option<String>,
    pub baseline_mapping: CanvasToPlotter,
    pub queue_job_ids: Vec<u64>,
    pub image_sha1: Vec<String>,
    pub plotter_sha1: Vec<String>,
    pub observations: Vec<CalibrationObservation>,
    pub excluded_target_ids: Vec<String>,
    pub selected_model: CalibrationModel,
    pub candidate_mapping: CanvasToPlotter,
    pub validation: Option<ValidationMetrics>,
    pub activated_profile: Option<CalibrationProfile>,
    pub created_at: u64,
    pub updated_at: u64,
}

impl CalibrationRunReport {
    pub fn to_pretty_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    pub fn summary(&self) -> String {
        let accepted = self.observations.iter().filter(|o| o.included).count();
        let validation = self.validation.as_ref().map_or_else(
            || "not yet validated".to_owned(),
            |metrics| {
                format!(
                    "validation RMS {:.3} -> {:.3} mm; p95 {:.3} -> {:.3} mm",
                    metrics.before.rms_mm,
                    metrics.after.rms_mm,
                    metrics.before.p95_mm,
                    metrics.after.p95_mm
                )
            },
        );
        format!(
            "{} calibration for {}: {accepted} accepted observations, model {:?}, {validation}",
            match self.method {
                CalibrationMethod::FlatbedScanner => "Flatbed Scanner",
                CalibrationMethod::ManualEastBay => "Manual",
            },
            self.printer.model,
            self.selected_model,
        )
    }
}

/// Calculates deterministic Euclidean error statistics for included observations.
pub fn observation_error_metrics(observations: &[CalibrationObservation]) -> Option<ErrorMetrics> {
    let mut errors = observations
        .iter()
        .filter(|observation| observation.included && observation.is_valid())
        .map(|observation| {
            let dx = observation.observed_cut_mm[0] - observation.nominal_print_mm[0];
            let dy = observation.observed_cut_mm[1] - observation.nominal_print_mm[1];
            (dx.hypot(dy), dx, dy)
        })
        .collect::<Vec<_>>();
    if errors.is_empty() {
        return None;
    }
    errors.sort_by(|a, b| a.0.total_cmp(&b.0));
    let sample_count = errors.len();
    let square_sum = errors.iter().map(|value| value.0 * value.0).sum::<f64>();
    let sum_x = errors.iter().map(|value| value.1).sum::<f64>();
    let sum_y = errors.iter().map(|value| value.2).sum::<f64>();
    let p95_index = ((sample_count as f64 * 0.95).ceil() as usize)
        .saturating_sub(1)
        .min(sample_count - 1);
    Some(ErrorMetrics {
        sample_count,
        rms_mm: (square_sum / sample_count as f64).sqrt(),
        p95_mm: errors[p95_index].0,
        maximum_mm: errors.last().expect("non-empty error list").0,
        mean_xy_mm: [sum_x / sample_count as f64, sum_y / sample_count as f64],
    })
}

pub fn sha1_hex(bytes: &[u8]) -> String {
    hex::encode(Sha1::digest(bytes))
}

/// Captures candidate information without letting validation observations enter the fit.
pub fn selected_solution_parts(solution: &CalibrationSolution) -> (CalibrationModel, Vec<String>) {
    (
        solution.selected.model,
        solution
            .selected
            .residuals
            .iter()
            .filter(|residual| residual.final_weight <= 0.0)
            .map(|residual| residual.target_id.clone())
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observation(id: &str, dx: f64, dy: f64, included: bool) -> CalibrationObservation {
        CalibrationObservation {
            target_id: id.into(),
            sheet_id: "sheet-1".into(),
            nominal_print_mm: [10.0, 20.0],
            observed_cut_mm: [10.0 + dx, 20.0 + dy],
            uncertainty_mm: [0.1, 0.1],
            confidence: 1.0,
            included,
        }
    }

    #[test]
    fn metrics_are_deterministic_and_ignore_excluded_points() {
        let values = [
            observation("a", 3.0, 4.0, true),
            observation("b", 0.0, 0.0, true),
            observation("excluded", 100.0, 100.0, false),
        ];
        let metrics = observation_error_metrics(&values).unwrap();
        assert_eq!(metrics.sample_count, 2);
        assert!((metrics.rms_mm - (12.5_f64).sqrt()).abs() < 1e-12);
        assert_eq!(metrics.p95_mm, 5.0);
        assert_eq!(metrics.maximum_mm, 5.0);
        assert_eq!(metrics.mean_xy_mm, [1.5, 2.0]);
    }

    #[test]
    fn payload_hash_is_stable() {
        assert_eq!(sha1_hex(b"calibration"), sha1_hex(b"calibration"));
        assert_ne!(sha1_hex(b"calibration"), sha1_hex(b"validation"));
    }
}
