use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::{
    CalibrationMethod, CalibrationObservation, CanvasToPlotter, ErrorMetrics,
    MAX_CALIBRATION_TEXT_BYTES, TransformBounds, bounded_calibration_text,
};

const MAX_CANDIDATE_RESIDUALS: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CalibrationModel {
    Translation,
    IndependentAxisScale,
    Affine,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CalibrationResidual {
    pub target_id: String,
    pub sheet_id: String,
    pub xy_mm: [f64; 2],
    pub distance_mm: f64,
    pub final_weight: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CandidateFit {
    pub model: CalibrationModel,
    /// Fitted physical response `observed_cut = forward_response(nominal_print)`.
    pub forward_response: CanvasToPlotter,
    pub training_metrics: ErrorMetrics,
    pub leave_one_out_metrics: ErrorMetrics,
    pub residuals: Vec<CalibrationResidual>,
    pub condition: f64,
}

impl CandidateFit {
    pub fn sanitize(&mut self) {
        self.residuals.truncate(MAX_CANDIDATE_RESIDUALS);
        for residual in &mut self.residuals {
            residual.target_id = bounded_calibration_text(&residual.target_id);
            residual.sheet_id = bounded_calibration_text(&residual.sheet_id);
        }
    }

    pub fn is_valid(&self) -> bool {
        self.forward_response
            .validate(TransformBounds {
                minimum_axis_scale: 0.5,
                maximum_axis_scale: 1.5,
                maximum_condition: 100.0,
                maximum_translation: 50.0,
            })
            .is_ok()
            && self.training_metrics.is_valid()
            && self.leave_one_out_metrics.is_valid()
            && self.condition.is_finite()
            && self.condition >= 1.0
            && self.residuals.len() <= MAX_CANDIDATE_RESIDUALS
            && self.residuals.iter().all(|residual| {
                !residual.target_id.is_empty()
                    && !residual.sheet_id.is_empty()
                    && residual.target_id.len() <= MAX_CALIBRATION_TEXT_BYTES
                    && residual.sheet_id.len() <= MAX_CALIBRATION_TEXT_BYTES
                    && residual.xy_mm.into_iter().all(f64::is_finite)
                    && residual.distance_mm.is_finite()
                    && residual.distance_mm >= 0.0
                    && residual.final_weight.is_finite()
                    && residual.final_weight >= 0.0
            })
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CalibrationSolution {
    pub selected: CandidateFit,
    pub candidates: Vec<CandidateFit>,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct CalibrationPolicy {
    pub canvas_size_mm: [f64; 2],
    pub huber_delta_mm: f64,
    pub minimum_relative_p95_improvement: f64,
    pub minimum_absolute_p95_improvement_mm: f64,
    pub maximum_condition: f64,
}

impl CalibrationPolicy {
    pub fn pixcut_s1_4x7() -> Self {
        Self {
            canvas_size_mm: [101.6, 177.8],
            huber_delta_mm: 0.25,
            minimum_relative_p95_improvement: 0.10,
            minimum_absolute_p95_improvement_mm: 0.05,
            maximum_condition: 1_000_000.0,
        }
    }
}

#[derive(Clone, Debug, Error, PartialEq)]
pub enum CalibrationSolveError {
    #[error("calibration policy is invalid")]
    InvalidPolicy,
    #[error("not enough valid observations or spatial coverage")]
    InsufficientCoverage,
    #[error("calibration design matrix is singular or ill-conditioned")]
    IllConditioned,
    #[error("calibration fit produced an implausible transform")]
    ImplausibleTransform,
}

pub fn solve_calibration(
    method: CalibrationMethod,
    observations: &[CalibrationObservation],
    policy: CalibrationPolicy,
) -> Result<CalibrationSolution, CalibrationSolveError> {
    if !policy_is_valid(policy) {
        return Err(CalibrationSolveError::InvalidPolicy);
    }
    let accepted: Vec<&CalibrationObservation> = observations
        .iter()
        .filter(|observation| observation.included && observation.is_valid())
        .collect();
    if !method_gate(method, &accepted, policy.canvas_size_mm) {
        return Err(CalibrationSolveError::InsufficientCoverage);
    }

    let mut candidates = Vec::new();
    for model in [
        CalibrationModel::Translation,
        CalibrationModel::IndependentAxisScale,
        CalibrationModel::Affine,
    ] {
        if !model_is_eligible(method, model, &accepted, policy.canvas_size_mm) {
            continue;
        }
        let candidate = fit_candidate(model, &accepted, policy)?;
        if candidate.condition <= policy.maximum_condition {
            candidates.push(candidate);
        }
    }
    let Some(mut selected) = candidates.first().cloned() else {
        return Err(CalibrationSolveError::InsufficientCoverage);
    };
    for candidate in candidates.iter().skip(1) {
        let simple = selected.leave_one_out_metrics.p95_mm;
        let complex = candidate.leave_one_out_metrics.p95_mm;
        let absolute = simple - complex;
        let relative = if simple > f64::EPSILON {
            absolute / simple
        } else {
            0.0
        };
        if absolute >= policy.minimum_absolute_p95_improvement_mm
            && relative >= policy.minimum_relative_p95_improvement
        {
            selected = candidate.clone();
        }
    }
    Ok(CalibrationSolution {
        selected,
        candidates,
    })
}

pub fn model_is_eligible(
    method: CalibrationMethod,
    model: CalibrationModel,
    observations: &[&CalibrationObservation],
    canvas: [f64; 2],
) -> bool {
    let count = observations.len();
    let xs: Vec<f64> = observations.iter().map(|o| o.nominal_print_mm[0]).collect();
    let ys: Vec<f64> = observations.iter().map(|o| o.nominal_print_mm[1]).collect();
    let left = xs.iter().any(|x| *x <= canvas[0] * 0.4);
    let right = xs.iter().any(|x| *x >= canvas[0] * 0.6);
    let y_span = range(&ys) >= canvas[1] * 0.2;
    match model {
        CalibrationModel::Translation => count >= 4 && left && right && y_span,
        CalibrationModel::IndependentAxisScale => {
            count >= 6
                && xs.iter().any(|x| *x <= canvas[0] * 0.25)
                && xs.iter().any(|x| *x >= canvas[0] * 0.75)
                && ys.iter().any(|y| *y <= canvas[1] * 0.25)
                && ys.iter().any(|y| *y >= canvas[1] * 0.75)
        }
        CalibrationModel::Affine => {
            let quadrants = represented_quadrants(observations, canvas);
            let distinct = observations
                .iter()
                .map(|observation| {
                    (
                        (observation.nominal_print_mm[0] * 1_000.0).round() as i64,
                        (observation.nominal_print_mm[1] * 1_000.0).round() as i64,
                    )
                })
                .collect::<BTreeSet<_>>()
                .len();
            let generic = count >= 8 && distinct >= 6 && quadrants == 0b1111;
            if method == CalibrationMethod::ManualEastBay {
                let mut sheets = BTreeMap::<&str, usize>::new();
                for observation in observations {
                    *sheets.entry(observation.sheet_id.as_str()).or_default() += 1;
                }
                generic
                    && count >= 12
                    && sheets.len() >= 2
                    && sheets.values().filter(|count| **count >= 6).count() >= 2
            } else {
                generic
            }
        }
    }
}

fn method_gate(
    method: CalibrationMethod,
    observations: &[&CalibrationObservation],
    canvas: [f64; 2],
) -> bool {
    match method {
        CalibrationMethod::FlatbedScanner => {
            observations.len() >= 8 && represented_quadrants(observations, canvas) == 0b1111
        }
        CalibrationMethod::ManualEastBay => true,
    }
}

fn represented_quadrants(observations: &[&CalibrationObservation], canvas: [f64; 2]) -> u8 {
    observations.iter().fold(0, |bits, observation| {
        let right = usize::from(observation.nominal_print_mm[0] >= canvas[0] / 2.0);
        let bottom = usize::from(observation.nominal_print_mm[1] >= canvas[1] / 2.0);
        bits | (1 << (bottom * 2 + right))
    })
}

fn range(values: &[f64]) -> f64 {
    let minimum = values.iter().copied().fold(f64::INFINITY, f64::min);
    let maximum = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    maximum - minimum
}

fn policy_is_valid(policy: CalibrationPolicy) -> bool {
    policy
        .canvas_size_mm
        .into_iter()
        .all(|v| v.is_finite() && v > 0.0)
        && policy.huber_delta_mm.is_finite()
        && policy.huber_delta_mm > 0.0
        && policy.minimum_relative_p95_improvement.is_finite()
        && (0.0..1.0).contains(&policy.minimum_relative_p95_improvement)
        && policy.minimum_absolute_p95_improvement_mm.is_finite()
        && policy.minimum_absolute_p95_improvement_mm >= 0.0
        && policy.maximum_condition.is_finite()
        && policy.maximum_condition >= 1.0
}

fn fit_candidate(
    model: CalibrationModel,
    observations: &[&CalibrationObservation],
    policy: CalibrationPolicy,
) -> Result<CandidateFit, CalibrationSolveError> {
    let (forward_response, condition, weights) =
        fit_robust(model, observations, policy.huber_delta_mm)?;
    validate_response(forward_response)?;
    let residuals = residuals(forward_response, observations, &weights);
    let training_metrics = metrics(residuals.iter().map(|residual| residual.xy_mm));
    let leave_one_out_metrics = leave_one_out(model, observations, policy.huber_delta_mm)?;
    Ok(CandidateFit {
        model,
        forward_response,
        training_metrics,
        leave_one_out_metrics,
        residuals,
        condition,
    })
}

fn validate_response(transform: CanvasToPlotter) -> Result<(), CalibrationSolveError> {
    transform
        .validate(TransformBounds {
            minimum_axis_scale: 0.5,
            maximum_axis_scale: 1.5,
            maximum_condition: 100.0,
            maximum_translation: 50.0,
        })
        .map_err(|_| CalibrationSolveError::ImplausibleTransform)
}

fn fit_robust(
    model: CalibrationModel,
    observations: &[&CalibrationObservation],
    huber_delta: f64,
) -> Result<(CanvasToPlotter, f64, Vec<f64>), CalibrationSolveError> {
    let base_weights: Vec<f64> = observations
        .iter()
        .map(|observation| {
            let variance = (observation.uncertainty_mm[0].powi(2)
                + observation.uncertainty_mm[1].powi(2))
                / 2.0;
            observation.confidence.max(0.001) / variance.max(0.0025)
        })
        .collect();
    let mut weights = base_weights.clone();
    let mut result = fit_weighted(model, observations, &weights)?;
    for _ in 0..12 {
        let previous = result.0;
        for ((weight, base), observation) in weights.iter_mut().zip(&base_weights).zip(observations)
        {
            let predicted = previous.apply(observation.nominal_print_mm);
            let distance = (predicted[0] - observation.observed_cut_mm[0])
                .hypot(predicted[1] - observation.observed_cut_mm[1]);
            let robust = if distance <= huber_delta {
                1.0
            } else {
                huber_delta / distance
            };
            *weight = *base * robust;
        }
        result = fit_weighted(model, observations, &weights)?;
        let coefficient_delta = transform_delta(previous, result.0);
        if coefficient_delta < 1e-12 {
            break;
        }
    }
    Ok((result.0, result.1.max(1.0), weights))
}

fn fit_weighted(
    model: CalibrationModel,
    observations: &[&CalibrationObservation],
    weights: &[f64],
) -> Result<(CanvasToPlotter, f64), CalibrationSolveError> {
    match model {
        CalibrationModel::Translation => {
            let weight_sum: f64 = weights.iter().sum();
            if weight_sum <= f64::EPSILON {
                return Err(CalibrationSolveError::IllConditioned);
            }
            let mut offset = [0.0, 0.0];
            for (observation, weight) in observations.iter().zip(weights) {
                offset[0] +=
                    weight * (observation.observed_cut_mm[0] - observation.nominal_print_mm[0]);
                offset[1] +=
                    weight * (observation.observed_cut_mm[1] - observation.nominal_print_mm[1]);
            }
            offset[0] /= weight_sum;
            offset[1] /= weight_sum;
            Ok((
                CanvasToPlotter {
                    matrix: [[1.0, 0.0], [0.0, 1.0]],
                    translation: offset,
                },
                1.0,
            ))
        }
        CalibrationModel::IndependentAxisScale => {
            let (center, scale) = normalization(observations);
            let x_features: Vec<Vec<f64>> = observations
                .iter()
                .map(|o| vec![(o.nominal_print_mm[0] - center[0]) / scale[0], 1.0])
                .collect();
            let y_features: Vec<Vec<f64>> = observations
                .iter()
                .map(|o| vec![(o.nominal_print_mm[1] - center[1]) / scale[1], 1.0])
                .collect();
            let out_x: Vec<f64> = observations.iter().map(|o| o.observed_cut_mm[0]).collect();
            let out_y: Vec<f64> = observations.iter().map(|o| o.observed_cut_mm[1]).collect();
            let (cx, condition_x) = weighted_regression(&x_features, &out_x, weights)?;
            let (cy, condition_y) = weighted_regression(&y_features, &out_y, weights)?;
            let sx = cx[0] / scale[0];
            let sy = cy[0] / scale[1];
            Ok((
                CanvasToPlotter {
                    matrix: [[sx, 0.0], [0.0, sy]],
                    translation: [cx[1] - sx * center[0], cy[1] - sy * center[1]],
                },
                condition_x.max(condition_y),
            ))
        }
        CalibrationModel::Affine => {
            let (center, scale) = normalization(observations);
            let features: Vec<Vec<f64>> = observations
                .iter()
                .map(|o| {
                    vec![
                        (o.nominal_print_mm[0] - center[0]) / scale[0],
                        (o.nominal_print_mm[1] - center[1]) / scale[1],
                        1.0,
                    ]
                })
                .collect();
            let out_x: Vec<f64> = observations.iter().map(|o| o.observed_cut_mm[0]).collect();
            let out_y: Vec<f64> = observations.iter().map(|o| o.observed_cut_mm[1]).collect();
            let (cx, condition_x) = weighted_regression(&features, &out_x, weights)?;
            let (cy, condition_y) = weighted_regression(&features, &out_y, weights)?;
            let matrix = [
                [cx[0] / scale[0], cx[1] / scale[1]],
                [cy[0] / scale[0], cy[1] / scale[1]],
            ];
            let translation = [
                cx[2] - matrix[0][0] * center[0] - matrix[0][1] * center[1],
                cy[2] - matrix[1][0] * center[0] - matrix[1][1] * center[1],
            ];
            Ok((
                CanvasToPlotter {
                    matrix,
                    translation,
                },
                condition_x.max(condition_y),
            ))
        }
    }
}

fn normalization(observations: &[&CalibrationObservation]) -> ([f64; 2], [f64; 2]) {
    let count = observations.len() as f64;
    let center = observations
        .iter()
        .fold([0.0, 0.0], |mut sum, observation| {
            sum[0] += observation.nominal_print_mm[0];
            sum[1] += observation.nominal_print_mm[1];
            sum
        })
        .map(|sum| sum / count);
    let mut scale = [0.0_f64, 0.0_f64];
    for observation in observations {
        scale[0] = scale[0].max((observation.nominal_print_mm[0] - center[0]).abs());
        scale[1] = scale[1].max((observation.nominal_print_mm[1] - center[1]).abs());
    }
    [scale[0], scale[1]] = [scale[0].max(1e-9), scale[1].max(1e-9)];
    (center, scale)
}

fn weighted_regression(
    features: &[Vec<f64>],
    output: &[f64],
    weights: &[f64],
) -> Result<(Vec<f64>, f64), CalibrationSolveError> {
    let columns = features.first().map(Vec::len).unwrap_or(0);
    if columns == 0
        || columns > 3
        || features.len() != output.len()
        || output.len() != weights.len()
    {
        return Err(CalibrationSolveError::IllConditioned);
    }
    let mut augmented = vec![vec![0.0; columns + 1]; columns];
    for ((row, target), weight) in features.iter().zip(output).zip(weights) {
        for i in 0..columns {
            for j in 0..columns {
                augmented[i][j] += weight * row[i] * row[j];
            }
            augmented[i][columns] += weight * row[i] * target;
        }
    }
    gaussian_solve(augmented)
}

fn gaussian_solve(mut matrix: Vec<Vec<f64>>) -> Result<(Vec<f64>, f64), CalibrationSolveError> {
    let size = matrix.len();
    let mut maximum_pivot: f64 = 0.0;
    let mut minimum_pivot = f64::INFINITY;
    for column in 0..size {
        let pivot_row = (column..size)
            .max_by(|left, right| {
                matrix[*left][column]
                    .abs()
                    .total_cmp(&matrix[*right][column].abs())
            })
            .unwrap();
        matrix.swap(column, pivot_row);
        let pivot = matrix[column][column].abs();
        if !pivot.is_finite() || pivot <= f64::EPSILON {
            return Err(CalibrationSolveError::IllConditioned);
        }
        maximum_pivot = maximum_pivot.max(pivot);
        minimum_pivot = minimum_pivot.min(pivot);
        let pivot_values = matrix[column].clone();
        for row in (column + 1)..size {
            let factor = matrix[row][column] / matrix[column][column];
            for (index, value) in matrix[row]
                .iter_mut()
                .enumerate()
                .take(size + 1)
                .skip(column)
            {
                *value -= factor * pivot_values[index];
            }
        }
    }
    let mut solution = vec![0.0; size];
    for row in (0..size).rev() {
        let known: f64 = ((row + 1)..size)
            .map(|column| matrix[row][column] * solution[column])
            .sum();
        solution[row] = (matrix[row][size] - known) / matrix[row][row];
    }
    if solution.iter().any(|value| !value.is_finite()) {
        return Err(CalibrationSolveError::IllConditioned);
    }
    Ok((solution, maximum_pivot / minimum_pivot))
}

fn leave_one_out(
    model: CalibrationModel,
    observations: &[&CalibrationObservation],
    huber_delta: f64,
) -> Result<ErrorMetrics, CalibrationSolveError> {
    let mut errors = Vec::with_capacity(observations.len());
    for held_out in 0..observations.len() {
        let training: Vec<_> = observations
            .iter()
            .enumerate()
            .filter_map(|(index, observation)| (index != held_out).then_some(*observation))
            .collect();
        let (transform, _, _) = fit_robust(model, &training, huber_delta)?;
        let predicted = transform.apply(observations[held_out].nominal_print_mm);
        errors.push([
            observations[held_out].observed_cut_mm[0] - predicted[0],
            observations[held_out].observed_cut_mm[1] - predicted[1],
        ]);
    }
    Ok(metrics(errors))
}

fn residuals(
    transform: CanvasToPlotter,
    observations: &[&CalibrationObservation],
    weights: &[f64],
) -> Vec<CalibrationResidual> {
    observations
        .iter()
        .zip(weights)
        .map(|(observation, weight)| {
            let predicted = transform.apply(observation.nominal_print_mm);
            let xy = [
                observation.observed_cut_mm[0] - predicted[0],
                observation.observed_cut_mm[1] - predicted[1],
            ];
            CalibrationResidual {
                target_id: observation.target_id.clone(),
                sheet_id: observation.sheet_id.clone(),
                distance_mm: xy[0].hypot(xy[1]),
                xy_mm: xy,
                final_weight: *weight,
            }
        })
        .collect()
}

fn metrics(errors: impl IntoIterator<Item = [f64; 2]>) -> ErrorMetrics {
    let errors: Vec<[f64; 2]> = errors.into_iter().collect();
    let mut distances: Vec<f64> = errors
        .iter()
        .map(|error| error[0].hypot(error[1]))
        .collect();
    distances.sort_by(f64::total_cmp);
    let count = errors.len();
    let mean_xy_mm = errors
        .iter()
        .fold([0.0, 0.0], |mut sum, error| {
            sum[0] += error[0];
            sum[1] += error[1];
            sum
        })
        .map(|sum| sum / count as f64);
    let rms_mm = (distances
        .iter()
        .map(|distance| distance * distance)
        .sum::<f64>()
        / count as f64)
        .sqrt();
    let p95_index = ((count as f64 * 0.95).ceil() as usize)
        .saturating_sub(1)
        .min(count - 1);
    ErrorMetrics {
        sample_count: count,
        rms_mm,
        p95_mm: distances[p95_index],
        maximum_mm: *distances.last().unwrap(),
        mean_xy_mm,
    }
}

fn transform_delta(left: CanvasToPlotter, right: CanvasToPlotter) -> f64 {
    left.matrix
        .into_iter()
        .flatten()
        .zip(right.matrix.into_iter().flatten())
        .map(|(a, b)| (a - b).abs())
        .chain(
            left.translation
                .into_iter()
                .zip(right.translation)
                .map(|(a, b)| (a - b).abs()),
        )
        .fold(0.0, f64::max)
}

#[cfg(test)]
mod tests {
    use super::*;

    const POINTS: [[f64; 2]; 7] = [
        [15.0, 21.0],
        [85.0, 21.0],
        [15.0, 85.0],
        [50.0, 85.0],
        [85.0, 85.0],
        [15.0, 153.0],
        [85.0, 153.0],
    ];

    fn observations(transform: CanvasToPlotter, sheets: usize) -> Vec<CalibrationObservation> {
        (0..sheets)
            .flat_map(|sheet| {
                POINTS
                    .into_iter()
                    .enumerate()
                    .map(move |(index, point)| CalibrationObservation {
                        target_id: format!("C{}", index + 1),
                        sheet_id: format!("sheet-{sheet}"),
                        nominal_print_mm: point,
                        observed_cut_mm: transform.apply(point),
                        uncertainty_mm: [0.05, 0.05],
                        confidence: 1.0,
                        included: true,
                    })
            })
            .collect()
    }

    #[test]
    fn robust_translation_recovers_offset_despite_one_gross_outlier() {
        let expected = CanvasToPlotter {
            matrix: [[1.0, 0.0], [0.0, 1.0]],
            translation: [0.72, -0.43],
        };
        let mut data = observations(expected, 1);
        data[3].observed_cut_mm[0] += 5.0;
        let solution = solve_calibration(
            CalibrationMethod::ManualEastBay,
            &data,
            CalibrationPolicy::pixcut_s1_4x7(),
        )
        .unwrap();
        assert_eq!(solution.selected.model, CalibrationModel::Translation);
        assert!((solution.selected.forward_response.translation[0] - 0.72).abs() < 0.08);
        assert!((solution.selected.forward_response.translation[1] + 0.43).abs() < 1e-10);
        assert!(
            solution.selected.residuals[3].final_weight
                < solution.selected.residuals[0].final_weight * 0.1
        );
    }

    #[test]
    fn axis_scale_is_promoted_only_when_quantitatively_better() {
        let expected = CanvasToPlotter {
            matrix: [[1.012, 0.0], [0.0, 0.986]],
            translation: [0.35, -0.27],
        };
        let data = observations(expected, 1);
        let solution = solve_calibration(
            CalibrationMethod::ManualEastBay,
            &data,
            CalibrationPolicy::pixcut_s1_4x7(),
        )
        .unwrap();
        assert_eq!(
            solution.selected.model,
            CalibrationModel::IndependentAxisScale
        );
        assert!((solution.selected.forward_response.matrix[0][0] - 1.012).abs() < 1e-10);
        assert!((solution.selected.forward_response.matrix[1][1] - 0.986).abs() < 1e-10);
        assert!(solution.selected.training_metrics.maximum_mm < 1e-10);
    }

    #[test]
    fn pure_skew_can_promote_affine_even_when_axis_scale_is_not_selected() {
        let skew = CanvasToPlotter {
            matrix: [[1.0, 0.018], [-0.014, 1.0]],
            translation: [0.3, -0.2],
        };
        let solution = solve_calibration(
            CalibrationMethod::FlatbedScanner,
            &observations(skew, 2),
            CalibrationPolicy::pixcut_s1_4x7(),
        )
        .unwrap();
        assert_eq!(solution.selected.model, CalibrationModel::Affine);
        assert!(solution.selected.leave_one_out_metrics.p95_mm < 1e-8);
    }

    #[test]
    fn manual_affine_requires_two_independent_sheets_and_twelve_points() {
        let expected = CanvasToPlotter {
            matrix: [[1.004, 0.006], [-0.005, 0.997]],
            translation: [0.2, -0.4],
        };
        let one_sheet = observations(expected, 1);
        let one_refs: Vec<_> = one_sheet.iter().collect();
        assert!(!model_is_eligible(
            CalibrationMethod::ManualEastBay,
            CalibrationModel::Affine,
            &one_refs,
            [101.6, 177.8]
        ));

        let two_sheets = observations(expected, 2);
        let solution = solve_calibration(
            CalibrationMethod::ManualEastBay,
            &two_sheets,
            CalibrationPolicy::pixcut_s1_4x7(),
        )
        .unwrap();
        assert_eq!(solution.selected.model, CalibrationModel::Affine);
        for row in 0..2 {
            for column in 0..2 {
                assert!(
                    (solution.selected.forward_response.matrix[row][column]
                        - expected.matrix[row][column])
                        .abs()
                        < 1e-10
                );
            }
        }
        assert!(solution.selected.training_metrics.rms_mm < 1e-10);
    }

    #[test]
    fn flatbed_enforces_eight_detections_and_all_quadrants() {
        let expected = CanvasToPlotter {
            matrix: [[1.0, 0.0], [0.0, 1.0]],
            translation: [0.3, 0.2],
        };
        let seven = observations(expected, 1);
        assert_eq!(
            solve_calibration(
                CalibrationMethod::FlatbedScanner,
                &seven,
                CalibrationPolicy::pixcut_s1_4x7()
            ),
            Err(CalibrationSolveError::InsufficientCoverage)
        );
        let mut eight = seven;
        eight.push(CalibrationObservation {
            target_id: "C8".into(),
            sheet_id: "sheet-0".into(),
            nominal_print_mm: [50.0, 150.0],
            observed_cut_mm: expected.apply([50.0, 150.0]),
            uncertainty_mm: [0.03, 0.03],
            confidence: 1.0,
            included: true,
        });
        assert!(
            solve_calibration(
                CalibrationMethod::FlatbedScanner,
                &eight,
                CalibrationPolicy::pixcut_s1_4x7()
            )
            .is_ok()
        );
    }

    #[test]
    fn invalid_and_excluded_observations_do_not_satisfy_coverage() {
        let expected = CanvasToPlotter::IDENTITY;
        let mut data = observations(expected, 1);
        for observation in &mut data[3..] {
            observation.included = false;
        }
        data[0].confidence = f64::NAN;
        assert_eq!(
            solve_calibration(
                CalibrationMethod::ManualEastBay,
                &data,
                CalibrationPolicy::pixcut_s1_4x7()
            ),
            Err(CalibrationSolveError::InsufficientCoverage)
        );
    }

    #[test]
    fn candidate_sanitization_caps_residual_count_and_text() {
        let expected = CanvasToPlotter {
            matrix: [[1.0, 0.0], [0.0, 1.0]],
            translation: [0.3, -0.2],
        };
        let data = observations(expected, 1);
        let mut candidate = solve_calibration(
            CalibrationMethod::ManualEastBay,
            &data,
            CalibrationPolicy::pixcut_s1_4x7(),
        )
        .unwrap()
        .selected;
        candidate.residuals[0].target_id = "x".repeat(1_000);
        candidate.residuals[0].sheet_id = "y".repeat(1_000);
        let residual = candidate.residuals[0].clone();
        candidate
            .residuals
            .extend(std::iter::repeat_n(residual, MAX_CANDIDATE_RESIDUALS));
        candidate.sanitize();
        assert_eq!(candidate.residuals.len(), MAX_CANDIDATE_RESIDUALS);
        assert_eq!(
            candidate.residuals[0].target_id.len(),
            MAX_CALIBRATION_TEXT_BYTES
        );
        assert_eq!(
            candidate.residuals[0].sheet_id.len(),
            MAX_CALIBRATION_TEXT_BYTES
        );
        assert!(candidate.is_valid());
    }
}
