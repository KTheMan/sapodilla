use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::{
    CALIBRATION_SCHEMA_VERSION, CalibrationMethod, CalibrationModel, CalibrationObservation,
    CandidateFit, CanvasToPlotter, TransformBounds, bounded_calibration_text,
};

pub const MAX_CALIBRATION_PROFILES: usize = 64;
pub const MAX_CALIBRATION_RUNS: usize = 32;
pub const MAX_RUN_OBSERVATIONS: usize = 256;
pub const MAX_RUN_JOB_IDS: usize = 32;
const MAX_TARGETS: usize = 256;
const MAX_RUN_PLOTTER_COMMANDS: usize = 10_000;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum StablePrinterIdentity {
    SerialNumber { serial_number: String },
    NamedFallback { profile_name: String },
}

impl StablePrinterIdentity {
    fn sanitize(&mut self) -> bool {
        let value = match self {
            Self::SerialNumber { serial_number } => serial_number,
            Self::NamedFallback { profile_name } => profile_name,
        };
        *value = bounded_trim(value);
        !value.is_empty()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PrinterCalibrationKey {
    pub identity: StablePrinterIdentity,
    pub model: String,
    pub firmware_revision: String,
    pub media_size: u16,
    pub media_type: u16,
}

impl PrinterCalibrationKey {
    fn sanitize(&mut self) -> bool {
        self.model = bounded_trim(&self.model);
        self.firmware_revision = bounded_trim(&self.firmware_revision);
        self.identity.sanitize() && !self.model.is_empty()
    }

    pub fn same_printer_and_media(&self, other: &Self) -> bool {
        self.identity == other.identity
            && self.model == other.model
            && self.media_size == other.media_size
            && self.media_type == other.media_type
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CalibrationCutMode {
    Kiss,
    ThroughCut,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CutPathDirection {
    Clockwise,
    CounterClockwise,
    Mixed,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CutSettingsProvenance {
    pub mode: CalibrationCutMode,
    pub pressure: u8,
    pub passes: u8,
    /// Informational until job serialization is verified to apply speed.
    pub configured_speed: Option<u8>,
    pub path_direction: CutPathDirection,
    pub path_order_id: String,
}

impl CutSettingsProvenance {
    fn sanitize(&mut self) -> bool {
        self.path_order_id = bounded_calibration_text(&self.path_order_id);
        self.pressure <= 100
            && (1..=4).contains(&self.passes)
            && self
                .configured_speed
                .is_none_or(|speed| (1..=10).contains(&speed))
            && !self.path_order_id.is_empty()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ErrorMetrics {
    pub sample_count: usize,
    pub rms_mm: f64,
    pub p95_mm: f64,
    pub maximum_mm: f64,
    pub mean_xy_mm: [f64; 2],
}

impl ErrorMetrics {
    pub fn is_valid(&self) -> bool {
        self.sample_count > 0
            && [self.rms_mm, self.p95_mm, self.maximum_mm]
                .into_iter()
                .all(|value| value.is_finite() && (0.0..=100.0).contains(&value))
            && self
                .mean_xy_mm
                .into_iter()
                .all(|value| value.is_finite() && value.abs() <= 100.0)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ValidationMetrics {
    pub before: ErrorMetrics,
    pub after: ErrorMetrics,
    pub required_coverage_passed: bool,
    pub maximum_error_passed: bool,
    pub normal_kiss_cut_passed: Option<bool>,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct CalibrationActivationPolicy {
    pub minimum_rms_improvement: f64,
    pub maximum_p95_mm: f64,
    pub maximum_error_relative_factor: f64,
    pub maximum_error_absolute_slack_mm: f64,
}

impl CalibrationActivationPolicy {
    pub const fn pixcut_s1_4x7() -> Self {
        Self {
            minimum_rms_improvement: 0.25,
            maximum_p95_mm: 0.50,
            maximum_error_relative_factor: 1.05,
            maximum_error_absolute_slack_mm: 0.10,
        }
    }
}

impl ValidationMetrics {
    pub fn is_valid(&self) -> bool {
        self.before.is_valid() && self.after.is_valid()
    }

    pub fn activation_passed(&self, method: CalibrationMethod) -> bool {
        self.activation_passed_with(method, CalibrationActivationPolicy::pixcut_s1_4x7())
    }

    pub fn activation_passed_with(
        &self,
        method: CalibrationMethod,
        policy: CalibrationActivationPolicy,
    ) -> bool {
        let improvement = if self.before.rms_mm > f64::EPSILON {
            (self.before.rms_mm - self.after.rms_mm) / self.before.rms_mm
        } else {
            0.0
        };
        let minimum_validation_samples = match method {
            CalibrationMethod::FlatbedScanner => 6,
            CalibrationMethod::ManualEastBay => 4,
        };
        let numeric_maximum_passed = self.after.maximum_mm
            <= self.before.maximum_mm * policy.maximum_error_relative_factor
                + policy.maximum_error_absolute_slack_mm;
        self.is_valid()
            && self.required_coverage_passed
            && self.maximum_error_passed
            && numeric_maximum_passed
            && self.after.sample_count >= minimum_validation_samples
            && improvement >= policy.minimum_rms_improvement
            && self.after.p95_mm <= policy.maximum_p95_mm
            && (method != CalibrationMethod::FlatbedScanner
                || self.normal_kiss_cut_passed == Some(true))
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CalibrationProfile {
    pub version: u8,
    pub profile_id: String,
    pub key: PrinterCalibrationKey,
    pub method: CalibrationMethod,
    pub canvas_to_plotter: CanvasToPlotter,
    pub baseline_mapping_id: String,
    pub created_at: u64,
    pub validation: ValidationMetrics,
    pub measurement_settings: CutSettingsProvenance,
    pub validation_settings: CutSettingsProvenance,
    pub selected_model: CalibrationModel,
    pub previous_profile_id: Option<String>,
}

impl CalibrationProfile {
    pub fn validate_and_sanitize(&mut self) -> Result<(), CalibrationDataError> {
        if self.version != CALIBRATION_SCHEMA_VERSION {
            return Err(CalibrationDataError::UnknownVersion(self.version));
        }
        self.profile_id = bounded_trim(&self.profile_id);
        self.baseline_mapping_id = bounded_trim(&self.baseline_mapping_id);
        self.previous_profile_id = self
            .previous_profile_id
            .take()
            .map(|value| bounded_trim(&value))
            .filter(|value| !value.is_empty());
        if self.profile_id.is_empty()
            || self.baseline_mapping_id.is_empty()
            || !self.key.sanitize()
            || !self.validation.is_valid()
            || !self.measurement_settings.sanitize()
            || !self.validation_settings.sanitize()
        {
            return Err(CalibrationDataError::InvalidProfile);
        }
        self.canvas_to_plotter
            .validate(TransformBounds::default())
            .map_err(|_| CalibrationDataError::InvalidTransform)?;
        Ok(())
    }

    pub fn match_for(&self, key: &PrinterCalibrationKey) -> ProfileMatch {
        if !self.key.same_printer_and_media(key) {
            ProfileMatch::DifferentPrinterOrMedia
        } else if self.key.firmware_revision == key.firmware_revision {
            ProfileMatch::Exact
        } else {
            ProfileMatch::StaleFirmware
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProfileMatch {
    Exact,
    StaleFirmware,
    DifferentPrinterOrMedia,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CalibrationTargetManifest {
    pub revision: u16,
    pub canvas_mm: [f64; 2],
    pub target_ids: Vec<String>,
    pub nominal_print_mm: Vec<[f64; 2]>,
    pub jpeg_sha1: Option<String>,
    pub plt_sha1: Option<String>,
}

impl CalibrationTargetManifest {
    fn sanitize(&mut self) -> bool {
        self.target_ids.truncate(MAX_TARGETS);
        self.nominal_print_mm.truncate(MAX_TARGETS);
        for id in &mut self.target_ids {
            *id = bounded_trim(id);
        }
        self.jpeg_sha1 = sanitize_hash(self.jpeg_sha1.take());
        self.plt_sha1 = sanitize_hash(self.plt_sha1.take());
        self.revision > 0
            && self
                .canvas_mm
                .into_iter()
                .all(|value| value.is_finite() && value > 0.0 && value <= 1_000.0)
            && self.target_ids.len() == self.nominal_print_mm.len()
            && self.target_ids.iter().all(|id| !id.is_empty())
            && self
                .nominal_print_mm
                .iter()
                .flatten()
                .all(|value| value.is_finite())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CalibrationPlotterCommandKind {
    Move,
    Draw,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalibrationPlotterCommand {
    pub kind: CalibrationPlotterCommandKind,
    /// The integer coordinates written into the PLT command stream.
    pub plotter_units: [i64; 2],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CalibrationRunState {
    Preparing,
    Measuring,
    CandidateReady,
    Validating,
    Passed,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalibrationPayloadHashes {
    pub slot: String,
    pub jpeg_sha1: String,
    pub plt_sha1: String,
    /// Ordered move/draw coordinates after the slot's active mapping and the
    /// encoder's final decimal quantization. Terminal carriage parking is not
    /// part of target geometry and is intentionally omitted.
    #[serde(default)]
    pub plotter_commands: Vec<CalibrationPlotterCommand>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CalibrationRun {
    pub version: u8,
    pub run_id: String,
    pub key: PrinterCalibrationKey,
    pub method: CalibrationMethod,
    pub baseline_profile_id: Option<String>,
    #[serde(default = "default_profile_version")]
    pub baseline_profile_version: u16,
    #[serde(default)]
    pub validation_generation: u32,
    pub baseline_mapping: CanvasToPlotter,
    pub manifest: CalibrationTargetManifest,
    pub queue_job_ids: Vec<u64>,
    pub device_job_ids: Vec<u32>,
    #[serde(default)]
    pub payload_hashes: Vec<CalibrationPayloadHashes>,
    pub observations: Vec<CalibrationObservation>,
    pub excluded_target_ids: Vec<String>,
    /// Optional [top, right, bottom, left] printable-area insets recorded by
    /// the Manual workflow.
    #[serde(default)]
    pub printability_insets_mm: [Option<f64>; 4],
    pub fit_candidates: Vec<CandidateFit>,
    pub selected_model: Option<CalibrationModel>,
    pub validation: Option<ValidationMetrics>,
    pub state: CalibrationRunState,
    pub created_at: u64,
    pub updated_at: u64,
}

impl CalibrationRun {
    pub fn validate_and_sanitize(&mut self) -> Result<(), CalibrationDataError> {
        if self.version != CALIBRATION_SCHEMA_VERSION {
            return Err(CalibrationDataError::UnknownVersion(self.version));
        }
        self.run_id = bounded_trim(&self.run_id);
        self.baseline_profile_id = self
            .baseline_profile_id
            .take()
            .map(|value| bounded_trim(&value))
            .filter(|value| !value.is_empty());
        self.queue_job_ids.truncate(MAX_RUN_JOB_IDS);
        self.device_job_ids.truncate(MAX_RUN_JOB_IDS);
        self.payload_hashes.truncate(3);
        let mut payload_slots = BTreeSet::new();
        for payload in &mut self.payload_hashes {
            payload.slot = bounded_trim(&payload.slot);
            payload.jpeg_sha1 = payload.jpeg_sha1.trim().to_ascii_lowercase();
            payload.plt_sha1 = payload.plt_sha1.trim().to_ascii_lowercase();
            payload.plotter_commands.truncate(MAX_RUN_PLOTTER_COMMANDS);
        }
        self.observations.truncate(MAX_RUN_OBSERVATIONS);
        for observation in &mut self.observations {
            observation.sanitize();
        }
        self.observations.retain(CalibrationObservation::is_valid);
        self.excluded_target_ids.truncate(MAX_RUN_OBSERVATIONS);
        for id in &mut self.excluded_target_ids {
            *id = bounded_trim(id);
        }
        self.excluded_target_ids.retain(|id| !id.is_empty());
        self.fit_candidates.truncate(3);
        for candidate in &mut self.fit_candidates {
            candidate.sanitize();
        }
        if self.run_id.is_empty()
            || !self.key.sanitize()
            || !self.manifest.sanitize()
            || self.baseline_profile_version == 0
            || self.updated_at < self.created_at
            || self
                .baseline_mapping
                .validate(TransformBounds::default())
                .is_err()
            || self
                .validation
                .as_ref()
                .is_some_and(|value| !value.is_valid())
            || self
                .printability_insets_mm
                .iter()
                .flatten()
                .any(|value| !value.is_finite() || !(0.0..=25.0).contains(value))
            || self
                .fit_candidates
                .iter()
                .any(|candidate| !candidate.is_valid())
            || self.payload_hashes.iter().any(|payload| {
                !matches!(payload.slot.as_str(), "primary" | "second" | "validation")
                    || !payload_slots.insert(payload.slot.as_str())
                    || !valid_sha1(&payload.jpeg_sha1)
                    || !valid_sha1(&payload.plt_sha1)
                    || payload.plotter_commands.is_empty()
                    || payload.plotter_commands.iter().any(|command| {
                        command
                            .plotter_units
                            .into_iter()
                            .any(|value| !(-1_000_000..=1_000_000).contains(&value))
                    })
            })
        {
            return Err(CalibrationDataError::InvalidRun);
        }
        Ok(())
    }
}

const fn default_profile_version() -> u16 {
    CALIBRATION_SCHEMA_VERSION as u16
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveCalibrationProfile {
    pub key: PrinterCalibrationKey,
    pub profile_id: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CalibrationStore {
    pub version: u8,
    pub profiles: Vec<CalibrationProfile>,
    pub runs: Vec<CalibrationRun>,
    pub active_profiles: Vec<ActiveCalibrationProfile>,
}

impl Default for CalibrationStore {
    fn default() -> Self {
        Self {
            version: CALIBRATION_SCHEMA_VERSION,
            profiles: Vec::new(),
            runs: Vec::new(),
            active_profiles: Vec::new(),
        }
    }
}

impl CalibrationStore {
    /// Rejects an unknown store version and corrupt entries, then applies hard caps.
    pub fn sanitize(mut self) -> Result<Self, CalibrationDataError> {
        if self.version != CALIBRATION_SCHEMA_VERSION {
            return Err(CalibrationDataError::UnknownVersion(self.version));
        }
        for profile in &mut self.profiles {
            profile.validate_and_sanitize()?;
        }
        let mut profile_ids = BTreeSet::new();
        if self
            .profiles
            .iter()
            .any(|profile| !profile_ids.insert(profile.profile_id.as_str()))
        {
            return Err(CalibrationDataError::DuplicateProfile);
        }
        for run in &mut self.runs {
            run.validate_and_sanitize()?;
        }
        self.profiles
            .sort_by_key(|profile| std::cmp::Reverse(profile.created_at));
        self.profiles.truncate(MAX_CALIBRATION_PROFILES);
        self.runs
            .sort_by_key(|run| std::cmp::Reverse(run.updated_at));
        self.runs.truncate(MAX_CALIBRATION_RUNS);

        let profiles: BTreeMap<&str, &CalibrationProfile> = self
            .profiles
            .iter()
            .map(|profile| (profile.profile_id.as_str(), profile))
            .collect();
        let mut seen = BTreeSet::new();
        self.active_profiles.retain_mut(|active| {
            active.profile_id = bounded_trim(&active.profile_id);
            active.key.sanitize()
                && seen.insert(active.key.clone())
                && profiles
                    .get(active.profile_id.as_str())
                    .is_some_and(|profile| {
                        profile.key == active.key
                            && profile.validation.activation_passed(profile.method)
                    })
        });
        Ok(self)
    }

    pub fn active_profile(&self, key: &PrinterCalibrationKey) -> Option<&CalibrationProfile> {
        let active = self
            .active_profiles
            .iter()
            .find(|active| &active.key == key)?;
        self.profiles
            .iter()
            .find(|profile| profile.profile_id == active.profile_id && profile.key == active.key)
    }

    /// Validate, retain, and activate a newly completed profile while linking
    /// it to the currently active profile for one-click rollback.
    pub fn add_and_activate(
        &mut self,
        mut profile: CalibrationProfile,
    ) -> Result<Option<String>, CalibrationDataError> {
        profile.validate_and_sanitize()?;
        if !profile.validation.activation_passed(profile.method) {
            return Err(CalibrationDataError::ValidationNotPassed);
        }
        if self
            .profiles
            .iter()
            .any(|existing| existing.profile_id == profile.profile_id)
        {
            return Err(CalibrationDataError::DuplicateProfile);
        }
        let previous = self
            .active_profile(&profile.key)
            .map(|active| active.profile_id.clone());
        profile.previous_profile_id = previous.clone();
        let profile_id = profile.profile_id.clone();
        self.profiles.push(profile);
        self.profiles
            .sort_by_key(|profile| std::cmp::Reverse(profile.created_at));
        self.profiles.truncate(MAX_CALIBRATION_PROFILES);
        self.activate(&profile_id)?;
        Ok(previous)
    }

    /// Activates an exact-key profile and returns the previously active profile ID.
    pub fn activate(&mut self, profile_id: &str) -> Result<Option<String>, CalibrationDataError> {
        let mut matches = self
            .profiles
            .iter()
            .filter(|profile| profile.profile_id == profile_id);
        let profile = matches.next().ok_or(CalibrationDataError::UnknownProfile)?;
        if matches.next().is_some() {
            return Err(CalibrationDataError::DuplicateProfile);
        }
        if !profile.validation.activation_passed(profile.method) {
            return Err(CalibrationDataError::ValidationNotPassed);
        }
        let key = profile.key.clone();
        let previous = self
            .active_profiles
            .iter()
            .find(|active| active.key == key)
            .map(|active| active.profile_id.clone());
        self.active_profiles.retain(|active| active.key != key);
        self.active_profiles.push(ActiveCalibrationProfile {
            key,
            profile_id: profile_id.to_owned(),
        });
        Ok(previous)
    }

    pub fn reset(&mut self, key: &PrinterCalibrationKey) -> Option<String> {
        let index = self
            .active_profiles
            .iter()
            .position(|active| &active.key == key)?;
        Some(self.active_profiles.remove(index).profile_id)
    }

    /// Restore the active profile's predecessor, or the stock mapping when the
    /// active profile was the first calibrated profile for this key.
    pub fn rollback(
        &mut self,
        key: &PrinterCalibrationKey,
    ) -> Result<Option<String>, CalibrationDataError> {
        let Some(current) = self.active_profile(key) else {
            return Ok(None);
        };
        let current_id = current.profile_id.clone();
        let previous_id = current.previous_profile_id.clone();
        if let Some(previous_id) = previous_id {
            let previous = self
                .profiles
                .iter()
                .find(|profile| profile.profile_id == previous_id)
                .ok_or(CalibrationDataError::BrokenRollbackLineage)?;
            if previous.key != *key {
                return Err(CalibrationDataError::BrokenRollbackLineage);
            }
            self.activate(&previous_id)?;
        } else {
            self.reset(key);
        }
        Ok(Some(current_id))
    }
}

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum CalibrationDataError {
    #[error("unsupported calibration schema version {0}")]
    UnknownVersion(u8),
    #[error("calibration profile is invalid")]
    InvalidProfile,
    #[error("calibration run is invalid")]
    InvalidRun,
    #[error("calibration transform is invalid")]
    InvalidTransform,
    #[error("calibration profile was not found")]
    UnknownProfile,
    #[error("calibration profile has not passed its validation gates")]
    ValidationNotPassed,
    #[error("calibration profile ID already exists")]
    DuplicateProfile,
    #[error("calibration rollback lineage is missing or incompatible")]
    BrokenRollbackLineage,
}

fn bounded_trim(value: &str) -> String {
    bounded_calibration_text(value)
}

fn sanitize_hash(hash: Option<String>) -> Option<String> {
    hash.map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

fn valid_sha1(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calibration::{CutPathDirection, LEGACY_PIXCUT_S1_MAPPING_ID};

    fn metrics(rms: f64) -> ErrorMetrics {
        ErrorMetrics {
            sample_count: 6,
            rms_mm: rms,
            p95_mm: rms * 1.2,
            maximum_mm: rms * 1.3,
            mean_xy_mm: [0.01, -0.02],
        }
    }

    fn key(firmware: &str) -> PrinterCalibrationKey {
        PrinterCalibrationKey {
            identity: StablePrinterIdentity::SerialNumber {
                serial_number: "SN-123".into(),
            },
            model: "DHP700".into(),
            firmware_revision: firmware.into(),
            media_size: 5013,
            media_type: 2030,
        }
    }

    fn settings() -> CutSettingsProvenance {
        CutSettingsProvenance {
            mode: CalibrationCutMode::Kiss,
            pressure: 42,
            passes: 1,
            configured_speed: Some(3),
            path_direction: CutPathDirection::Clockwise,
            path_order_id: "sorted-v1".into(),
        }
    }

    fn profile(id: &str, created_at: u64) -> CalibrationProfile {
        CalibrationProfile {
            version: CALIBRATION_SCHEMA_VERSION,
            profile_id: id.into(),
            key: key("1.0.15"),
            method: CalibrationMethod::ManualEastBay,
            canvas_to_plotter: CanvasToPlotter::legacy_pixcut_s1(2100.0),
            baseline_mapping_id: LEGACY_PIXCUT_S1_MAPPING_ID.into(),
            created_at,
            validation: ValidationMetrics {
                before: metrics(0.8),
                after: metrics(0.25),
                required_coverage_passed: true,
                maximum_error_passed: true,
                normal_kiss_cut_passed: Some(true),
            },
            measurement_settings: settings(),
            validation_settings: settings(),
            selected_model: CalibrationModel::Translation,
            previous_profile_id: None,
        }
    }

    #[test]
    fn profile_json_round_trip_and_firmware_staleness_are_preserved() {
        let profile = profile("p1", 100);
        let encoded = serde_json::to_string(&profile).unwrap();
        let mut decoded: CalibrationProfile = serde_json::from_str(&encoded).unwrap();
        decoded.validate_and_sanitize().unwrap();
        assert_eq!(decoded, profile);
        assert_eq!(decoded.match_for(&key("1.0.15")), ProfileMatch::Exact);
        assert_eq!(
            decoded.match_for(&key("1.0.16")),
            ProfileMatch::StaleFirmware
        );
    }

    #[test]
    fn store_activation_reset_and_caps_are_deterministic() {
        let mut store = CalibrationStore::default();
        for index in 0..(MAX_CALIBRATION_PROFILES + 5) {
            store
                .profiles
                .push(profile(&format!("p{index}"), index as u64));
        }
        let mut store = store.sanitize().unwrap();
        assert_eq!(store.profiles.len(), MAX_CALIBRATION_PROFILES);
        assert_eq!(
            store.profiles[0].created_at,
            (MAX_CALIBRATION_PROFILES + 4) as u64
        );
        let id = store.profiles[0].profile_id.clone();
        assert_eq!(store.activate(&id), Ok(None));
        assert_eq!(store.active_profile(&key("1.0.15")).unwrap().profile_id, id);
        assert_eq!(store.reset(&key("1.0.15")), Some(id));
        assert!(store.active_profile(&key("1.0.15")).is_none());
    }

    #[test]
    fn sanitization_rejects_unknown_versions_and_bad_transforms() {
        let mut bad_version = profile("bad", 1);
        bad_version.version = 99;
        assert_eq!(
            bad_version.validate_and_sanitize(),
            Err(CalibrationDataError::UnknownVersion(99))
        );
        let mut profile = profile("bad-transform", 2);
        profile.canvas_to_plotter.matrix = [[1.0, 2.0], [2.0, 4.0]];
        assert_eq!(
            profile.validate_and_sanitize(),
            Err(CalibrationDataError::InvalidTransform)
        );
        let store = CalibrationStore {
            version: 9,
            ..CalibrationStore::default()
        };
        assert_eq!(
            store.sanitize(),
            Err(CalibrationDataError::UnknownVersion(9))
        );
    }

    #[test]
    fn activation_enforces_shared_and_flatbed_specific_validation_gates() {
        let mut failed_coverage = profile("failed-coverage", 1);
        failed_coverage.validation.required_coverage_passed = false;
        let mut store = CalibrationStore {
            profiles: vec![failed_coverage],
            ..CalibrationStore::default()
        };
        assert_eq!(
            store.activate("failed-coverage"),
            Err(CalibrationDataError::ValidationNotPassed)
        );

        let mut flatbed = profile("flatbed", 2);
        flatbed.method = CalibrationMethod::FlatbedScanner;
        flatbed.validation.normal_kiss_cut_passed = None;
        store.profiles.push(flatbed);
        assert_eq!(
            store.activate("flatbed"),
            Err(CalibrationDataError::ValidationNotPassed)
        );
        store
            .profiles
            .last_mut()
            .unwrap()
            .validation
            .normal_kiss_cut_passed = Some(true);
        assert_eq!(store.activate("flatbed"), Ok(None));

        let mut no_improvement = profile("no-improvement", 3);
        no_improvement.validation.after = no_improvement.validation.before.clone();
        store.profiles.push(no_improvement);
        assert_eq!(
            store.activate("no-improvement"),
            Err(CalibrationDataError::ValidationNotPassed)
        );

        let mut outside_goal = profile("outside-goal", 4);
        outside_goal.validation.before = metrics(2.0);
        outside_goal.validation.after = metrics(0.6);
        store.profiles.push(outside_goal);
        assert_eq!(
            store.activate("outside-goal"),
            Err(CalibrationDataError::ValidationNotPassed)
        );

        let mut numeric_max_regression = profile("max-regression", 5);
        numeric_max_regression.validation.after.maximum_mm = 2.0;
        numeric_max_regression.validation.maximum_error_passed = true;
        store.profiles.push(numeric_max_regression);
        assert_eq!(
            store.activate("max-regression"),
            Err(CalibrationDataError::ValidationNotPassed)
        );
    }

    #[test]
    fn imported_active_profiles_that_fail_validation_are_dropped() {
        let mut failed = profile("failed-import", 1);
        failed.validation.required_coverage_passed = false;
        let key = failed.key.clone();
        let store = CalibrationStore {
            profiles: vec![failed],
            active_profiles: vec![ActiveCalibrationProfile {
                key: key.clone(),
                profile_id: "failed-import".into(),
            }],
            ..CalibrationStore::default()
        }
        .sanitize()
        .unwrap();

        assert!(store.active_profile(&key).is_none());
        assert!(store.active_profiles.is_empty());
        assert_eq!(store.profiles.len(), 1);
    }

    #[test]
    fn duplicate_ids_are_rejected_and_lookup_requires_the_exact_key() {
        let first = profile("duplicate", 1);
        let mut second = profile("duplicate", 2);
        second.key.identity = StablePrinterIdentity::SerialNumber {
            serial_number: "OTHER".into(),
        };
        let store = CalibrationStore {
            profiles: vec![first, second],
            ..CalibrationStore::default()
        };
        assert_eq!(
            store.sanitize(),
            Err(CalibrationDataError::DuplicateProfile)
        );
    }

    #[test]
    fn add_and_activate_records_and_enforces_rollback_lineage() {
        let mut store = CalibrationStore::default();
        assert_eq!(store.add_and_activate(profile("first", 1)), Ok(None));
        assert_eq!(
            store.add_and_activate(profile("second", 2)),
            Ok(Some("first".into()))
        );
        assert_eq!(
            store.active_profile(&key("1.0.15")).unwrap().profile_id,
            "second"
        );
        assert_eq!(store.rollback(&key("1.0.15")), Ok(Some("second".into())));
        assert_eq!(
            store.active_profile(&key("1.0.15")).unwrap().profile_id,
            "first"
        );
        assert_eq!(store.rollback(&key("1.0.15")), Ok(Some("first".into())));
        assert!(store.active_profile(&key("1.0.15")).is_none());
    }

    #[test]
    fn profile_rejects_out_of_device_range_cut_settings() {
        let mut invalid_pressure = profile("pressure", 1);
        invalid_pressure.measurement_settings.pressure = 101;
        assert_eq!(
            invalid_pressure.validate_and_sanitize(),
            Err(CalibrationDataError::InvalidProfile)
        );

        let mut invalid_passes = profile("passes", 1);
        invalid_passes.validation_settings.passes = 5;
        assert_eq!(
            invalid_passes.validate_and_sanitize(),
            Err(CalibrationDataError::InvalidProfile)
        );

        let mut invalid_speed = profile("speed", 1);
        invalid_speed.measurement_settings.configured_speed = Some(0);
        assert_eq!(
            invalid_speed.validate_and_sanitize(),
            Err(CalibrationDataError::InvalidProfile)
        );
    }

    #[test]
    fn persisted_run_bounds_observation_identifiers() {
        let long = format!("  {}  ", "target".repeat(100));
        let mut run = CalibrationRun {
            version: CALIBRATION_SCHEMA_VERSION,
            run_id: "run-1".into(),
            key: key("1.0.15"),
            method: CalibrationMethod::ManualEastBay,
            baseline_profile_id: None,
            baseline_profile_version: default_profile_version(),
            validation_generation: 0,
            baseline_mapping: CanvasToPlotter::legacy_pixcut_s1(2100.0),
            manifest: CalibrationTargetManifest {
                revision: 1,
                canvas_mm: [101.6, 177.8],
                target_ids: vec!["C1".into()],
                nominal_print_mm: vec![[15.0, 21.0]],
                jpeg_sha1: None,
                plt_sha1: None,
            },
            queue_job_ids: vec![],
            device_job_ids: vec![],
            payload_hashes: vec![],
            observations: vec![CalibrationObservation {
                target_id: long.clone(),
                sheet_id: long,
                nominal_print_mm: [15.0, 21.0],
                observed_cut_mm: [15.2, 20.9],
                uncertainty_mm: [0.1, 0.1],
                confidence: 1.0,
                included: true,
            }],
            excluded_target_ids: vec![],
            printability_insets_mm: [Some(1.0), None, Some(2.0), None],
            fit_candidates: vec![],
            selected_model: None,
            validation: None,
            state: CalibrationRunState::Measuring,
            created_at: 1,
            updated_at: 2,
        };
        run.validate_and_sanitize().unwrap();
        assert_eq!(run.observations.len(), 1);
        assert_eq!(
            run.observations[0].target_id.len(),
            super::super::MAX_CALIBRATION_TEXT_BYTES
        );
        assert_eq!(
            run.observations[0].sheet_id.len(),
            super::super::MAX_CALIBRATION_TEXT_BYTES
        );
    }
}
