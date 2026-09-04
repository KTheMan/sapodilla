//! Serializable, UI-independent calibration wizard state machine.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::{
    CalibrationMethod, CalibrationObservation, ManualEdgeMeasurement, bounded_calibration_text,
};

pub const CALIBRATION_WIZARD_SCHEMA_VERSION: u8 = 1;
pub const EAST_BAY_METHOD_COPY: &str = "Guided measurements based on the seven-target method publicly documented by East Bay Makers Club.";
pub const EAST_BAY_RESULTS_CREDIT: &str = "Method credit: East Bay Makers Club · pixcut-s1";
pub const EAST_BAY_SOURCE_URL: &str =
    "https://github.com/eastbaymakersclub/pixcut-s1#calibration-workflow";

const MANUAL_PRIMARY_IDS: [&str; 7] = ["C1", "C2", "C3", "C4", "C5", "C6", "C7"];
const MANUAL_PRIMARY_CENTERS: [[f64; 2]; 7] = [
    [15.0, 21.0],
    [85.0, 21.0],
    [15.0, 85.0],
    [50.0, 85.0],
    [85.0, 85.0],
    [15.0, 153.0],
    [85.0, 153.0],
];
const MANUAL_VALIDATION_IDS: [&str; 5] = ["V1", "V2", "V3", "V4", "V5"];
const MANUAL_VALIDATION_CENTERS: [[f64; 2]; 5] = [
    [15.0, 21.0],
    [85.0, 21.0],
    [50.0, 85.0],
    [15.0, 153.0],
    [85.0, 153.0],
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WizardStep {
    ChooseMethod,
    Prepare,
    PrintCalibration,
    RemoveCenters,
    ImportScan,
    ReviewScan,
    PrintArea,
    PrintScale,
    ManualTargets,
    SecondSheetChoice,
    PrintSecondCalibration,
    SecondPrintScale,
    SecondManualTargets,
    Candidate,
    PrintValidation,
    RemoveValidationCenters,
    ImportValidationScan,
    ReviewValidation,
    ValidationManualTargets,
    KissCutInspection,
    Finish,
}

impl WizardStep {
    pub const fn label(self, method: Option<CalibrationMethod>) -> &'static str {
        match self {
            Self::ChooseMethod => "Choose method",
            Self::Prepare => "Prepare",
            Self::PrintCalibration => match method {
                Some(CalibrationMethod::ManualEastBay) => "Print measurement sheet",
                _ => "Print calibration sheet",
            },
            Self::RemoveCenters => "Remove centers",
            Self::ImportScan => "Import scan",
            Self::ReviewScan => "Review scan",
            Self::PrintArea => "Print area",
            Self::PrintScale => "Print scale",
            Self::ManualTargets => "Registration measurements",
            Self::SecondSheetChoice => "Measurement review",
            Self::PrintSecondCalibration => "Print another sheet",
            Self::SecondPrintScale => "Second-sheet print scale",
            Self::SecondManualTargets => "Second-sheet measurements",
            Self::Candidate => "Candidate result",
            Self::PrintValidation => "Print validation sheet",
            Self::RemoveValidationCenters => "Remove validation centers",
            Self::ImportValidationScan => "Import validation scan",
            Self::ReviewValidation => "Review validation",
            Self::ValidationManualTargets => "Validation measurements",
            Self::KissCutInspection => "Inspect kiss cuts",
            Self::Finish => "Finish",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum JobStatus {
    #[default]
    NotStarted,
    Queued,
    InProgress,
    Completed,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobSlot {
    Primary,
    Second,
    Validation,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "kebab-case")]
pub enum ScanImportStatus {
    #[default]
    NotImported,
    Importing {
        file_name: String,
    },
    Imported {
        file_name: String,
        accepted_targets: usize,
        all_quadrants: bool,
    },
    Failed {
        message: String,
    },
}

impl ScanImportStatus {
    fn is_usable(&self, validation: bool) -> bool {
        matches!(
            self,
            Self::Imported {
                accepted_targets,
                all_quadrants: true,
                ..
            } if *accepted_targets >= if validation { 6 } else { 8 }
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScanSlot {
    Training,
    Validation,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OptionalMeasurementState {
    #[default]
    Pending,
    Measured,
    Skipped,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PrintScaleDraft {
    pub state: OptionalMeasurementState,
    pub horizontal_mm: Option<f64>,
    pub vertical_mm: Option<f64>,
}

impl Default for PrintScaleDraft {
    fn default() -> Self {
        Self {
            state: OptionalMeasurementState::Pending,
            horizontal_mm: None,
            vertical_mm: None,
        }
    }
}

impl PrintScaleDraft {
    pub fn measured(&mut self, horizontal_mm: f64, vertical_mm: f64) -> bool {
        if !horizontal_mm.is_finite()
            || !vertical_mm.is_finite()
            || horizontal_mm <= 0.0
            || vertical_mm <= 0.0
        {
            return false;
        }
        self.state = OptionalMeasurementState::Measured;
        self.horizontal_mm = Some(horizontal_mm);
        self.vertical_mm = Some(vertical_mm);
        true
    }

    pub fn skip(&mut self) {
        self.state = OptionalMeasurementState::Skipped;
        self.horizontal_mm = None;
        self.vertical_mm = None;
    }

    pub fn ratios(&self) -> Option<[f64; 2]> {
        match self.state {
            OptionalMeasurementState::Measured => {
                let horizontal = self.horizontal_mm?;
                let vertical = self.vertical_mm?;
                if !horizontal.is_finite()
                    || !vertical.is_finite()
                    || horizontal <= 0.0
                    || vertical <= 0.0
                {
                    return None;
                }
                Some([horizontal / 80.0, vertical / 150.0])
            }
            OptionalMeasurementState::Skipped => Some([1.0, 1.0]),
            OptionalMeasurementState::Pending => None,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ManualEdgeDraft {
    pub left_mm: Option<f64>,
    pub right_mm: Option<f64>,
    pub top_mm: Option<f64>,
    pub bottom_mm: Option<f64>,
}

impl ManualEdgeDraft {
    pub fn complete_measurement(&self) -> Option<ManualEdgeMeasurement> {
        let value = ManualEdgeMeasurement {
            left_mm: self.left_mm?,
            right_mm: self.right_mm?,
            top_mm: self.top_mm?,
            bottom_mm: self.bottom_mm?,
        };
        value.derive([1.0, 1.0])?;
        Some(value)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ManualTargetDraft {
    pub target_id: String,
    pub nominal_print_mm: [f64; 2],
    pub edges: ManualEdgeDraft,
    pub skipped_damaged: bool,
}

impl ManualTargetDraft {
    pub fn is_accepted(&self) -> bool {
        !self.skipped_damaged && self.edges.complete_measurement().is_some()
    }

    fn observation(
        &self,
        sheet_id: &str,
        print_scale: [f64; 2],
        uncertainty_mm: f64,
    ) -> Option<CalibrationObservation> {
        if self.skipped_damaged {
            return None;
        }
        let derived = self.edges.complete_measurement()?.derive(print_scale)?;
        Some(CalibrationObservation {
            target_id: self.target_id.clone(),
            sheet_id: sheet_id.into(),
            nominal_print_mm: self.nominal_print_mm,
            observed_cut_mm: [
                self.nominal_print_mm[0] + derived.displacement_mm[0],
                self.nominal_print_mm[1] + derived.displacement_mm[1],
            ],
            uncertainty_mm: [uncertainty_mm; 2],
            confidence: 1.0,
            included: true,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ManualSheetDraft {
    pub sheet_id: String,
    pub print_scale: PrintScaleDraft,
    pub targets: Vec<ManualTargetDraft>,
}

impl ManualSheetDraft {
    fn primary(sheet_id: String) -> Self {
        Self {
            sheet_id,
            print_scale: PrintScaleDraft::default(),
            targets: manual_targets(&MANUAL_PRIMARY_IDS, &MANUAL_PRIMARY_CENTERS),
        }
    }

    fn validation(sheet_id: String) -> Self {
        Self {
            sheet_id,
            // Validation uses the fresh sheet's printed coordinates but does not
            // refit print scale; the candidate is tested as actually produced.
            print_scale: PrintScaleDraft {
                state: OptionalMeasurementState::Skipped,
                horizontal_mm: None,
                vertical_mm: None,
            },
            targets: manual_targets(&MANUAL_VALIDATION_IDS, &MANUAL_VALIDATION_CENTERS),
        }
    }

    pub fn accepted_count(&self) -> usize {
        self.targets
            .iter()
            .filter(|target| target.is_accepted())
            .count()
    }

    fn observations(&self, uncertainty_mm: f64) -> Vec<CalibrationObservation> {
        let Some(print_scale) = self.print_scale.ratios() else {
            return Vec::new();
        };
        self.observations_with_print_scale(uncertainty_mm, print_scale)
    }

    fn observations_with_print_scale(
        &self,
        uncertainty_mm: f64,
        print_scale: [f64; 2],
    ) -> Vec<CalibrationObservation> {
        self.targets
            .iter()
            .filter_map(|target| target.observation(&self.sheet_id, print_scale, uncertainty_mm))
            .collect()
    }
}

fn manual_targets<const N: usize>(
    ids: &[&str; N],
    centers: &[[f64; 2]; N],
) -> Vec<ManualTargetDraft> {
    ids.iter()
        .zip(centers)
        .map(|(id, center)| ManualTargetDraft {
            target_id: (*id).into(),
            nominal_print_mm: *center,
            edges: ManualEdgeDraft::default(),
            skipped_damaged: false,
        })
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SecondSheetChoice {
    ContinueWithOneSheet,
    MeasureAnotherSheet,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "kebab-case")]
pub enum CandidateStatus {
    #[default]
    NotStarted,
    Computing,
    Ready {
        selected_model: String,
    },
    Failed {
        message: String,
    },
}

impl CandidateStatus {
    fn is_ready(&self) -> bool {
        matches!(self, Self::Ready { .. })
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "kebab-case")]
pub enum ValidationStatus {
    #[default]
    NotStarted,
    Collecting,
    Evaluating,
    Passed,
    Failed {
        message: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "kebab-case")]
pub enum DraftPersistence {
    Dirty,
    Saved { saved_at: u64 },
    Discarded,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CalibrationWizard {
    pub version: u8,
    pub run_id: String,
    pub method: Option<CalibrationMethod>,
    pub step: WizardStep,
    pub history: Vec<WizardStep>,
    pub prepared: bool,
    pub primary_job: JobStatus,
    pub second_job: JobStatus,
    pub validation_job: JobStatus,
    pub primary_centers_removed: bool,
    pub validation_centers_removed: bool,
    pub training_scan: ScanImportStatus,
    pub validation_scan: ScanImportStatus,
    pub training_scan_reviewed: bool,
    pub validation_scan_reviewed: bool,
    pub scan_training_observations: Vec<CalibrationObservation>,
    pub validation_observations: Vec<CalibrationObservation>,
    pub print_area_reviewed: bool,
    pub printability_insets_mm: [Option<f64>; 4],
    pub manual_primary: ManualSheetDraft,
    pub manual_second: ManualSheetDraft,
    pub manual_validation: ManualSheetDraft,
    pub second_sheet_choice: Option<SecondSheetChoice>,
    pub manual_uncertainty_mm: f64,
    pub candidate: CandidateStatus,
    pub validation: ValidationStatus,
    /// Advances whenever training inputs change. Validation target identity
    /// includes this value so output from an older candidate cannot be reused.
    #[serde(default)]
    pub validation_generation: u32,
    pub normal_kiss_cut_passed: Option<bool>,
    pub persistence: DraftPersistence,
    pub created_at: u64,
    pub updated_at: u64,
}

impl CalibrationWizard {
    pub fn new(run_id: impl Into<String>, created_at: u64) -> Result<Self, WizardError> {
        let run_id = bounded_calibration_text(&run_id.into());
        if !valid_wizard_id(&run_id) {
            return Err(WizardError::InvalidDraft(
                "run id is empty or unsupported".into(),
            ));
        }
        Ok(Self {
            version: CALIBRATION_WIZARD_SCHEMA_VERSION,
            manual_primary: ManualSheetDraft::primary(format!("{run_id}-sheet-1")),
            manual_second: ManualSheetDraft::primary(format!("{run_id}-sheet-2")),
            manual_validation: ManualSheetDraft::validation(format!("{run_id}-validation")),
            run_id,
            method: None,
            step: WizardStep::ChooseMethod,
            history: Vec::new(),
            prepared: false,
            primary_job: JobStatus::NotStarted,
            second_job: JobStatus::NotStarted,
            validation_job: JobStatus::NotStarted,
            primary_centers_removed: false,
            validation_centers_removed: false,
            training_scan: ScanImportStatus::NotImported,
            validation_scan: ScanImportStatus::NotImported,
            training_scan_reviewed: false,
            validation_scan_reviewed: false,
            scan_training_observations: Vec::new(),
            validation_observations: Vec::new(),
            print_area_reviewed: false,
            printability_insets_mm: [None; 4],
            second_sheet_choice: None,
            manual_uncertainty_mm: 0.1,
            candidate: CandidateStatus::NotStarted,
            validation: ValidationStatus::NotStarted,
            validation_generation: 0,
            normal_kiss_cut_passed: None,
            persistence: DraftPersistence::Dirty,
            created_at,
            updated_at: created_at,
        })
    }

    pub fn select_method(
        &mut self,
        method: CalibrationMethod,
        now: u64,
    ) -> Result<(), WizardError> {
        self.ensure_active()?;
        if self.step != WizardStep::ChooseMethod {
            return Err(WizardError::TransitionBlocked(
                "the calibration method can only change on the method step".into(),
            ));
        }
        if self.method != Some(method) {
            self.invalidate_candidate();
        }
        self.method = Some(method);
        self.touch(now);
        Ok(())
    }

    pub fn confirm_prepared(&mut self, confirmed: bool, now: u64) {
        self.prepared = confirmed;
        self.touch(now);
    }

    pub fn set_job_status(&mut self, slot: JobSlot, status: JobStatus, now: u64) {
        *match slot {
            JobSlot::Primary => &mut self.primary_job,
            JobSlot::Second => &mut self.second_job,
            JobSlot::Validation => &mut self.validation_job,
        } = status;
        if slot == JobSlot::Validation && status != JobStatus::NotStarted {
            self.validation = ValidationStatus::Collecting;
        }
        self.touch(now);
    }

    /// Starts a new physical print for `slot` and invalidates evidence that
    /// belonged to the previously printed sheet. The host should call this at
    /// the job-start boundary before preparing or reprinting a calibration job.
    ///
    /// This clears stale observations, but the host must also ensure its
    /// scanner-readable manifest identity changes when a physical sheet is
    /// reprinted so an intentionally re-imported old scan cannot bind again.
    pub fn begin_print_job(&mut self, slot: JobSlot, now: u64) -> Result<(), WizardError> {
        self.ensure_active()?;
        match slot {
            JobSlot::Primary => {
                self.primary_job = JobStatus::Queued;
                self.primary_centers_removed = false;
                self.training_scan = ScanImportStatus::NotImported;
                self.training_scan_reviewed = false;
                self.scan_training_observations.clear();
                self.print_area_reviewed = false;
                self.printability_insets_mm = [None; 4];
                self.manual_primary = ManualSheetDraft::primary(format!("{}-sheet-1", self.run_id));
                self.manual_second = ManualSheetDraft::primary(format!("{}-sheet-2", self.run_id));
                self.second_sheet_choice = None;
                self.second_job = JobStatus::NotStarted;
                self.invalidate_candidate();
            }
            JobSlot::Second => {
                self.second_job = JobStatus::Queued;
                self.manual_second = ManualSheetDraft::primary(format!("{}-sheet-2", self.run_id));
                self.invalidate_candidate();
            }
            JobSlot::Validation => {
                self.validation_job = JobStatus::Queued;
                self.validation_centers_removed = false;
                self.validation_scan = ScanImportStatus::NotImported;
                self.validation_scan_reviewed = false;
                self.validation_observations.clear();
                self.manual_validation =
                    ManualSheetDraft::validation(format!("{}-validation", self.run_id));
                self.normal_kiss_cut_passed = None;
                self.validation = ValidationStatus::Collecting;
            }
        }
        self.touch(now);
        Ok(())
    }

    pub fn confirm_centers_removed(&mut self, validation: bool, now: u64) {
        if validation {
            self.validation_centers_removed = true;
        } else {
            self.primary_centers_removed = true;
        }
        self.touch(now);
    }

    pub fn begin_scan_import(&mut self, slot: ScanSlot, file_name: &str, now: u64) {
        match slot {
            ScanSlot::Training => {
                self.scan_training_observations.clear();
                self.training_scan_reviewed = false;
                self.invalidate_candidate();
            }
            ScanSlot::Validation => {
                self.validation_observations.clear();
                self.validation_scan_reviewed = false;
            }
        }
        *self.scan_status_mut(slot) = ScanImportStatus::Importing {
            file_name: bounded_calibration_text(file_name),
        };
        self.touch(now);
    }

    pub fn complete_scan_import(
        &mut self,
        slot: ScanSlot,
        file_name: &str,
        all_quadrants: bool,
        observations: Vec<CalibrationObservation>,
        now: u64,
    ) {
        let observations = observations
            .into_iter()
            .filter(CalibrationObservation::is_valid)
            .collect::<Vec<_>>();
        let accepted_targets = observations.iter().filter(|value| value.included).count();
        *self.scan_status_mut(slot) = ScanImportStatus::Imported {
            file_name: bounded_calibration_text(file_name),
            accepted_targets,
            all_quadrants,
        };
        match slot {
            ScanSlot::Training => {
                self.scan_training_observations = observations;
                self.training_scan_reviewed = false;
                self.invalidate_candidate();
            }
            ScanSlot::Validation => {
                self.validation_observations = observations;
                self.validation_scan_reviewed = false;
            }
        }
        self.touch(now);
    }

    pub fn fail_scan_import(&mut self, slot: ScanSlot, message: &str, now: u64) {
        match slot {
            ScanSlot::Training => self.scan_training_observations.clear(),
            ScanSlot::Validation => self.validation_observations.clear(),
        }
        *self.scan_status_mut(slot) = ScanImportStatus::Failed {
            message: bounded_calibration_text(message),
        };
        self.touch(now);
    }

    pub fn accept_scan_review(&mut self, slot: ScanSlot, now: u64) {
        if self.scan_review_coverage_issue(slot).is_some() {
            return;
        }
        match slot {
            ScanSlot::Training => self.training_scan_reviewed = true,
            ScanSlot::Validation => self.validation_scan_reviewed = true,
        }
        self.touch(now);
    }

    pub fn set_scan_target_included(
        &mut self,
        slot: ScanSlot,
        target_id: &str,
        included: bool,
        now: u64,
    ) -> Result<(), WizardError> {
        self.ensure_active()?;
        let observations = match slot {
            ScanSlot::Training => &mut self.scan_training_observations,
            ScanSlot::Validation => &mut self.validation_observations,
        };
        let observation = observations
            .iter_mut()
            .find(|observation| observation.target_id == target_id)
            .ok_or_else(|| WizardError::UnknownTarget(target_id.into()))?;
        if observation.included != included {
            observation.included = included;
            match slot {
                ScanSlot::Training => {
                    self.training_scan_reviewed = false;
                    self.invalidate_candidate();
                }
                ScanSlot::Validation => {
                    self.validation_scan_reviewed = false;
                    self.validation = ValidationStatus::Collecting;
                }
            }
            self.touch(now);
        }
        Ok(())
    }

    pub fn scan_review_coverage_issue(&self, slot: ScanSlot) -> Option<&'static str> {
        let (observations, minimum) = match slot {
            ScanSlot::Training => (&self.scan_training_observations, 8),
            ScanSlot::Validation => (&self.validation_observations, 6),
        };
        let included = observations
            .iter()
            .filter(|observation| observation.included)
            .collect::<Vec<_>>();
        if included.len() < minimum {
            return Some(match slot {
                ScanSlot::Training => "include at least eight reliable aperture detections",
                ScanSlot::Validation => "include all six reliable validation detections",
            });
        }
        let mut quadrants = 0u8;
        for observation in included {
            let right = observation.nominal_print_mm[0] >= 101.6 / 2.0;
            let bottom = observation.nominal_print_mm[1] >= 177.8 / 2.0;
            quadrants |= 1 << (usize::from(bottom) * 2 + usize::from(right));
        }
        (quadrants != 0b1111).then_some("include reliable detections in all four sheet quadrants")
    }

    pub fn mark_print_area_reviewed(
        &mut self,
        insets_mm: [Option<f64>; 4],
        now: u64,
    ) -> Result<(), WizardError> {
        if insets_mm
            .iter()
            .flatten()
            .any(|value| !value.is_finite() || *value < 0.0 || *value > 25.0)
        {
            return Err(WizardError::InvalidMeasurement);
        }
        self.printability_insets_mm = insets_mm;
        self.print_area_reviewed = true;
        self.touch(now);
        Ok(())
    }

    pub fn set_print_scale(
        &mut self,
        slot: ManualSheetSlot,
        measured_mm: Option<[f64; 2]>,
        now: u64,
    ) -> Result<(), WizardError> {
        let draft = self.manual_sheet_mut(slot);
        match measured_mm {
            Some([horizontal, vertical]) if draft.print_scale.measured(horizontal, vertical) => {}
            Some(_) => return Err(WizardError::InvalidMeasurement),
            None => draft.print_scale.skip(),
        }
        if slot != ManualSheetSlot::Validation {
            self.invalidate_candidate();
        }
        self.touch(now);
        Ok(())
    }

    /// Records the expected one-axis precision of the user's measuring tool.
    pub fn set_manual_uncertainty(
        &mut self,
        uncertainty_mm: f64,
        now: u64,
    ) -> Result<(), WizardError> {
        self.ensure_active()?;
        if !uncertainty_mm.is_finite() || !(0.01..=10.0).contains(&uncertainty_mm) {
            return Err(WizardError::InvalidMeasurement);
        }
        if (self.manual_uncertainty_mm - uncertainty_mm).abs() > f64::EPSILON {
            self.manual_uncertainty_mm = uncertainty_mm;
            self.invalidate_candidate();
            self.touch(now);
        }
        Ok(())
    }

    pub fn set_manual_target(
        &mut self,
        slot: ManualSheetSlot,
        target_id: &str,
        edges: ManualEdgeDraft,
        skipped_damaged: bool,
        now: u64,
    ) -> Result<(), WizardError> {
        // Partial drafts are valid while entering data, but every present
        // value must itself be a plausible non-negative measurement.
        if [edges.left_mm, edges.right_mm, edges.top_mm, edges.bottom_mm]
            .into_iter()
            .flatten()
            .any(|value| !value.is_finite() || !(0.0..=30.0).contains(&value))
        {
            return Err(WizardError::InvalidMeasurement);
        }
        let target = self
            .manual_sheet_mut(slot)
            .targets
            .iter_mut()
            .find(|target| target.target_id == target_id)
            .ok_or_else(|| WizardError::UnknownTarget(target_id.into()))?;
        target.edges = edges;
        target.skipped_damaged = skipped_damaged;
        if slot != ManualSheetSlot::Validation {
            self.invalidate_candidate();
        }
        self.touch(now);
        Ok(())
    }

    pub fn choose_second_sheet(&mut self, choice: SecondSheetChoice, now: u64) {
        self.second_sheet_choice = Some(choice);
        self.invalidate_candidate();
        self.touch(now);
    }

    /// Abandons an incomplete second sheet and evaluates the first sheet only.
    pub fn continue_with_one_sheet(&mut self, now: u64) {
        self.second_sheet_choice = Some(SecondSheetChoice::ContinueWithOneSheet);
        self.invalidate_candidate();
        self.touch(now);
    }

    pub fn second_sheet_merge_eligible(&self) -> bool {
        self.manual_primary.accepted_count() >= 6
            && self.manual_second.accepted_count() >= 6
            && self.manual_primary.print_scale.ratios().is_some()
            && self.manual_second.print_scale.ratios().is_some()
    }

    /// Explains why a manual sheet cannot yet support even the Translation
    /// model. This mirrors the solver's count and spatial-coverage gate.
    pub fn manual_translation_coverage_issue(&self, slot: ManualSheetSlot) -> Option<&'static str> {
        manual_translation_coverage_issue(self.manual_sheet(slot))
    }

    pub fn mark_candidate_computing(&mut self, now: u64) {
        self.candidate = CandidateStatus::Computing;
        self.touch(now);
    }

    pub fn mark_candidate_ready(&mut self, selected_model: &str, now: u64) {
        self.candidate = CandidateStatus::Ready {
            selected_model: bounded_calibration_text(selected_model),
        };
        self.touch(now);
    }

    pub fn mark_candidate_failed(&mut self, message: &str, now: u64) {
        self.candidate = CandidateStatus::Failed {
            message: bounded_calibration_text(message),
        };
        self.touch(now);
    }

    pub fn set_validation_result(&mut self, passed: bool, message: &str, now: u64) {
        self.validation = if passed {
            ValidationStatus::Passed
        } else {
            ValidationStatus::Failed {
                message: bounded_calibration_text(message),
            }
        };
        self.touch(now);
    }

    pub fn set_kiss_cut_inspection(&mut self, passed: bool, now: u64) {
        self.normal_kiss_cut_passed = Some(passed);
        self.touch(now);
    }

    pub fn training_observations(&self) -> Vec<CalibrationObservation> {
        match self.method {
            Some(CalibrationMethod::FlatbedScanner) => self.scan_training_observations.clone(),
            Some(CalibrationMethod::ManualEastBay) => {
                let mut observations = self.manual_primary.observations(self.manual_uncertainty_mm);
                if self.second_sheet_choice == Some(SecondSheetChoice::MeasureAnotherSheet)
                    && self.second_sheet_merge_eligible()
                {
                    observations
                        .extend(self.manual_second.observations(self.manual_uncertainty_mm));
                }
                observations
            }
            None => Vec::new(),
        }
    }

    /// Returns only observations from the newly generated validation sheet.
    /// These values are intentionally never returned by
    /// [`Self::training_observations`].
    pub fn validation_observations_for_evaluation(&self) -> Vec<CalibrationObservation> {
        match self.method {
            Some(CalibrationMethod::FlatbedScanner) => self.validation_observations.clone(),
            // The compact validation sheet intentionally omits H80/V150. Use
            // the primary training sheet's measured printer scale so held-out
            // L/R/T/B displacements are expressed in the same printed logical
            // coordinate system as the fit. A skipped primary scale resolves
            // to [1, 1].
            Some(CalibrationMethod::ManualEastBay) => {
                let Some(print_scale) = self.manual_primary.print_scale.ratios() else {
                    return Vec::new();
                };
                self.manual_validation
                    .observations_with_print_scale(self.manual_uncertainty_mm, print_scale)
            }
            None => Vec::new(),
        }
    }

    pub fn next(&mut self, now: u64) -> Result<WizardStep, WizardError> {
        self.ensure_active()?;
        let next = self.guarded_next()?;
        self.history.push(self.step);
        self.step = next;
        if next == WizardStep::Candidate && matches!(self.candidate, CandidateStatus::NotStarted) {
            self.candidate = CandidateStatus::Computing;
        }
        self.touch(now);
        Ok(next)
    }

    pub fn back(&mut self, now: u64) -> Result<WizardStep, WizardError> {
        self.ensure_active()?;
        let previous = self.history.pop().ok_or(WizardError::NoPreviousStep)?;
        self.step = previous;
        self.touch(now);
        Ok(previous)
    }

    pub fn save_json(&mut self, saved_at: u64) -> Result<String, WizardError> {
        self.ensure_active()?;
        self.updated_at = saved_at.max(self.updated_at);
        self.persistence = DraftPersistence::Saved {
            saved_at: self.updated_at,
        };
        self.validate_resume()?;
        serde_json::to_string(self).map_err(|error| WizardError::Serialization(error.to_string()))
    }

    pub fn resume_json(json: &str) -> Result<Self, WizardError> {
        let draft: Self = serde_json::from_str(json)
            .map_err(|error| WizardError::Serialization(error.to_string()))?;
        draft.validate_resume()?;
        Ok(draft)
    }

    pub fn discard(&mut self, now: u64) {
        self.scan_training_observations.clear();
        self.validation_observations.clear();
        self.persistence = DraftPersistence::Discarded;
        self.updated_at = now.max(self.updated_at);
    }

    fn guarded_next(&self) -> Result<WizardStep, WizardError> {
        let method = self.method;
        let blocked = |reason: &str| Err(WizardError::TransitionBlocked(reason.into()));
        match self.step {
            WizardStep::ChooseMethod => method.map(|_| WizardStep::Prepare).ok_or_else(|| {
                WizardError::TransitionBlocked("choose a calibration method".into())
            }),
            WizardStep::Prepare => {
                if self.prepared {
                    Ok(WizardStep::PrintCalibration)
                } else {
                    blocked("complete the preparation checklist")
                }
            }
            WizardStep::PrintCalibration => {
                if self.primary_job != JobStatus::Completed {
                    return blocked("the calibration job has not completed");
                }
                match method {
                    Some(CalibrationMethod::FlatbedScanner) => Ok(WizardStep::RemoveCenters),
                    Some(CalibrationMethod::ManualEastBay) => Ok(WizardStep::PrintArea),
                    None => blocked("choose a calibration method"),
                }
            }
            WizardStep::RemoveCenters => {
                if self.primary_centers_removed {
                    Ok(WizardStep::ImportScan)
                } else {
                    blocked("confirm that the removable centers are out")
                }
            }
            WizardStep::ImportScan => {
                if self.training_scan.is_usable(false) {
                    Ok(WizardStep::ReviewScan)
                } else {
                    blocked("import a usable scan with at least eight distributed targets")
                }
            }
            WizardStep::ReviewScan => {
                if self.training_scan_reviewed {
                    Ok(WizardStep::Candidate)
                } else {
                    blocked("accept the scan review")
                }
            }
            WizardStep::PrintArea => {
                if self.print_area_reviewed {
                    Ok(WizardStep::PrintScale)
                } else {
                    blocked("review or skip the optional print-area measurements")
                }
            }
            WizardStep::PrintScale => {
                if self.manual_primary.print_scale.ratios().is_some() {
                    Ok(WizardStep::ManualTargets)
                } else {
                    blocked("measure or skip the print scale bars")
                }
            }
            WizardStep::ManualTargets => {
                if let Some(reason) =
                    self.manual_translation_coverage_issue(ManualSheetSlot::Primary)
                {
                    blocked(reason)
                } else {
                    Ok(WizardStep::SecondSheetChoice)
                }
            }
            WizardStep::SecondSheetChoice => match self.second_sheet_choice {
                Some(SecondSheetChoice::ContinueWithOneSheet) => Ok(WizardStep::Candidate),
                Some(SecondSheetChoice::MeasureAnotherSheet) => {
                    Ok(WizardStep::PrintSecondCalibration)
                }
                None => blocked("choose whether to measure another sheet"),
            },
            WizardStep::PrintSecondCalibration => {
                if self.second_job == JobStatus::Completed {
                    Ok(WizardStep::SecondPrintScale)
                } else {
                    blocked("the second independently loaded sheet has not completed")
                }
            }
            WizardStep::SecondPrintScale => {
                if self.manual_second.print_scale.ratios().is_some() {
                    Ok(WizardStep::SecondManualTargets)
                } else {
                    blocked("measure or skip the second sheet's print scale")
                }
            }
            WizardStep::SecondManualTargets => match self.second_sheet_choice {
                Some(SecondSheetChoice::ContinueWithOneSheet) => Ok(WizardStep::Candidate),
                Some(SecondSheetChoice::MeasureAnotherSheet)
                    if self.second_sheet_merge_eligible() =>
                {
                    Ok(WizardStep::Candidate)
                }
                _ => blocked("two-sheet merging requires six accepted targets on each sheet"),
            },
            WizardStep::Candidate => {
                if self.candidate.is_ready() {
                    Ok(WizardStep::PrintValidation)
                } else {
                    blocked("wait for a valid calibration candidate")
                }
            }
            WizardStep::PrintValidation => {
                if self.validation_job != JobStatus::Completed {
                    return blocked("the new validation sheet has not completed");
                }
                match method {
                    Some(CalibrationMethod::FlatbedScanner) => {
                        Ok(WizardStep::RemoveValidationCenters)
                    }
                    Some(CalibrationMethod::ManualEastBay) => {
                        Ok(WizardStep::ValidationManualTargets)
                    }
                    None => blocked("choose a calibration method"),
                }
            }
            WizardStep::RemoveValidationCenters => {
                if self.validation_centers_removed {
                    Ok(WizardStep::ImportValidationScan)
                } else {
                    blocked("confirm that the validation centers are out")
                }
            }
            WizardStep::ImportValidationScan => {
                if self.validation_scan.is_usable(true) {
                    Ok(WizardStep::ReviewValidation)
                } else {
                    blocked("import a validation scan with six distributed apertures")
                }
            }
            WizardStep::ValidationManualTargets => {
                if let Some(reason) =
                    self.manual_translation_coverage_issue(ManualSheetSlot::Validation)
                {
                    blocked(reason)
                } else {
                    Ok(WizardStep::ReviewValidation)
                }
            }
            WizardStep::ReviewValidation => match self.validation {
                ValidationStatus::Passed => match method {
                    Some(CalibrationMethod::FlatbedScanner) => {
                        if self.validation_scan_reviewed {
                            Ok(WizardStep::KissCutInspection)
                        } else {
                            blocked("accept the validation scan review")
                        }
                    }
                    Some(CalibrationMethod::ManualEastBay) => Ok(WizardStep::Finish),
                    None => blocked("choose a calibration method"),
                },
                ValidationStatus::Failed { .. } => {
                    blocked("validation failed; keep the current profile or retry")
                }
                _ => blocked("wait for validation evaluation"),
            },
            WizardStep::KissCutInspection => {
                if self.normal_kiss_cut_passed == Some(true) {
                    Ok(WizardStep::Finish)
                } else {
                    blocked("confirm that the production kiss cuts follow the print")
                }
            }
            WizardStep::Finish => blocked("the wizard is already complete"),
        }
    }

    fn scan_status_mut(&mut self, slot: ScanSlot) -> &mut ScanImportStatus {
        match slot {
            ScanSlot::Training => &mut self.training_scan,
            ScanSlot::Validation => &mut self.validation_scan,
        }
    }

    fn manual_sheet_mut(&mut self, slot: ManualSheetSlot) -> &mut ManualSheetDraft {
        match slot {
            ManualSheetSlot::Primary => &mut self.manual_primary,
            ManualSheetSlot::Second => &mut self.manual_second,
            ManualSheetSlot::Validation => &mut self.manual_validation,
        }
    }

    fn manual_sheet(&self, slot: ManualSheetSlot) -> &ManualSheetDraft {
        match slot {
            ManualSheetSlot::Primary => &self.manual_primary,
            ManualSheetSlot::Second => &self.manual_second,
            ManualSheetSlot::Validation => &self.manual_validation,
        }
    }

    fn touch(&mut self, now: u64) {
        self.updated_at = now.max(self.updated_at);
        self.persistence = DraftPersistence::Dirty;
    }

    fn invalidate_candidate(&mut self) {
        self.candidate = CandidateStatus::NotStarted;
        self.validation = ValidationStatus::NotStarted;
        self.validation_generation = self.validation_generation.saturating_add(1);
        self.validation_job = JobStatus::NotStarted;
        self.validation_centers_removed = false;
        self.validation_scan = ScanImportStatus::NotImported;
        self.validation_scan_reviewed = false;
        self.validation_observations.clear();
        self.manual_validation =
            ManualSheetDraft::validation(format!("{}-validation", self.run_id));
        self.normal_kiss_cut_passed = None;
    }

    fn ensure_active(&self) -> Result<(), WizardError> {
        if self.persistence == DraftPersistence::Discarded {
            Err(WizardError::Discarded)
        } else {
            Ok(())
        }
    }

    fn validate_resume(&self) -> Result<(), WizardError> {
        if self.version != CALIBRATION_WIZARD_SCHEMA_VERSION {
            return Err(WizardError::UnknownVersion(self.version));
        }
        if !valid_wizard_id(&self.run_id) || self.updated_at < self.created_at {
            return Err(WizardError::InvalidDraft(
                "invalid identity or timestamps".into(),
            ));
        }
        if !self.manual_uncertainty_mm.is_finite()
            || self.manual_uncertainty_mm <= 0.0
            || self.manual_uncertainty_mm > 10.0
        {
            return Err(WizardError::InvalidDraft(
                "manual uncertainty is outside supported bounds".into(),
            ));
        }
        if self.persistence == DraftPersistence::Discarded {
            return Err(WizardError::Discarded);
        }
        if self.history.len() > 64
            || !scan_state_consistent(&self.training_scan, &self.scan_training_observations)
            || !scan_state_consistent(&self.validation_scan, &self.validation_observations)
            || matches!(
                &self.candidate,
                CandidateStatus::Ready { selected_model } if selected_model.trim().is_empty()
            )
        {
            return Err(WizardError::InvalidDraft(
                "wizard status data is internally inconsistent".into(),
            ));
        }
        validate_sheet(
            &self.manual_primary,
            &MANUAL_PRIMARY_IDS,
            &MANUAL_PRIMARY_CENTERS,
        )?;
        validate_sheet(
            &self.manual_second,
            &MANUAL_PRIMARY_IDS,
            &MANUAL_PRIMARY_CENTERS,
        )?;
        validate_sheet(
            &self.manual_validation,
            &MANUAL_VALIDATION_IDS,
            &MANUAL_VALIDATION_CENTERS,
        )?;
        if self
            .scan_training_observations
            .iter()
            .chain(&self.validation_observations)
            .any(|observation| !observation.is_valid())
        {
            return Err(WizardError::InvalidDraft("invalid observation".into()));
        }
        if self.step != WizardStep::ChooseMethod && self.method.is_none() {
            return Err(WizardError::InvalidDraft(
                "method missing after selection".into(),
            ));
        }
        if let Some(method) = self.method
            && (!step_allowed_for_method(self.step, method)
                || self
                    .history
                    .iter()
                    .any(|step| !step_allowed_for_method(*step, method)))
        {
            return Err(WizardError::InvalidDraft(
                "wizard step does not match its method".into(),
            ));
        }
        Ok(())
    }
}

fn manual_translation_coverage_issue(sheet: &ManualSheetDraft) -> Option<&'static str> {
    let accepted = sheet
        .targets
        .iter()
        .filter(|target| target.is_accepted())
        .collect::<Vec<_>>();
    if accepted.len() < 4 {
        return Some("complete at least four targets");
    }
    if !accepted
        .iter()
        .any(|target| target.nominal_print_mm[0] <= 101.6 * 0.4)
    {
        return Some("measure at least one target on the left side of the sheet");
    }
    if !accepted
        .iter()
        .any(|target| target.nominal_print_mm[0] >= 101.6 * 0.6)
    {
        return Some("measure at least one target on the right side of the sheet");
    }
    let minimum_y = accepted
        .iter()
        .map(|target| target.nominal_print_mm[1])
        .fold(f64::INFINITY, f64::min);
    let maximum_y = accepted
        .iter()
        .map(|target| target.nominal_print_mm[1])
        .fold(f64::NEG_INFINITY, f64::max);
    if maximum_y - minimum_y < 177.8 * 0.2 {
        return Some("measure targets in two well-separated rows");
    }
    None
}

fn step_allowed_for_method(step: WizardStep, method: CalibrationMethod) -> bool {
    let flatbed_only = matches!(
        step,
        WizardStep::RemoveCenters
            | WizardStep::ImportScan
            | WizardStep::ReviewScan
            | WizardStep::RemoveValidationCenters
            | WizardStep::ImportValidationScan
            | WizardStep::KissCutInspection
    );
    let manual_only = matches!(
        step,
        WizardStep::PrintArea
            | WizardStep::PrintScale
            | WizardStep::ManualTargets
            | WizardStep::SecondSheetChoice
            | WizardStep::PrintSecondCalibration
            | WizardStep::SecondPrintScale
            | WizardStep::SecondManualTargets
            | WizardStep::ValidationManualTargets
    );
    match method {
        CalibrationMethod::FlatbedScanner => !manual_only,
        CalibrationMethod::ManualEastBay => !flatbed_only,
    }
}

fn valid_wizard_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.:".contains(&byte))
}

fn scan_state_consistent(
    status: &ScanImportStatus,
    observations: &[CalibrationObservation],
) -> bool {
    match status {
        ScanImportStatus::Imported {
            file_name,
            accepted_targets,
            ..
        } => {
            !file_name.trim().is_empty()
                && *accepted_targets
                    == observations
                        .iter()
                        .filter(|observation| observation.included)
                        .count()
        }
        ScanImportStatus::NotImported
        | ScanImportStatus::Importing { .. }
        | ScanImportStatus::Failed { .. } => observations.is_empty(),
    }
}

fn validate_sheet<const N: usize>(
    sheet: &ManualSheetDraft,
    expected_ids: &[&str; N],
    expected_centers: &[[f64; 2]; N],
) -> Result<(), WizardError> {
    if sheet.sheet_id.trim().is_empty()
        || sheet.targets.len() != N
        || sheet
            .targets
            .iter()
            .zip(expected_ids.iter().zip(expected_centers))
            .any(|(target, (expected_id, expected_center))| {
                target.target_id != *expected_id || target.nominal_print_mm != *expected_center
            })
    {
        return Err(WizardError::InvalidDraft(
            "manual target manifest mismatch".into(),
        ));
    }
    let print_scale_valid = match sheet.print_scale.state {
        OptionalMeasurementState::Pending | OptionalMeasurementState::Skipped => {
            sheet.print_scale.horizontal_mm.is_none() && sheet.print_scale.vertical_mm.is_none()
        }
        OptionalMeasurementState::Measured => sheet.print_scale.ratios().is_some(),
    };
    let measurements_valid = sheet.targets.iter().all(|target| {
        [
            target.edges.left_mm,
            target.edges.right_mm,
            target.edges.top_mm,
            target.edges.bottom_mm,
        ]
        .into_iter()
        .flatten()
        .all(|value| value.is_finite() && (0.0..=30.0).contains(&value))
    });
    if !print_scale_valid || !measurements_valid {
        return Err(WizardError::InvalidDraft(
            "manual measurements are internally inconsistent".into(),
        ));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ManualSheetSlot {
    Primary,
    Second,
    Validation,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum WizardError {
    #[error("wizard transition blocked: {0}")]
    TransitionBlocked(String),
    #[error("wizard has no previous step")]
    NoPreviousStep,
    #[error("unknown manual target {0}")]
    UnknownTarget(String),
    #[error("invalid physical measurement")]
    InvalidMeasurement,
    #[error("unknown wizard version {0}")]
    UnknownVersion(u8),
    #[error("invalid wizard draft: {0}")]
    InvalidDraft(String),
    #[error("discarded wizard drafts cannot be resumed or changed")]
    Discarded,
    #[error("wizard serialization failed: {0}")]
    Serialization(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn complete_edges() -> ManualEdgeDraft {
        ManualEdgeDraft {
            left_mm: Some(6.8),
            right_mm: Some(7.2),
            top_mm: Some(7.1),
            bottom_mm: Some(6.9),
        }
    }

    fn observation(id: usize, sheet: &str) -> CalibrationObservation {
        let [x, y] = match id % 4 {
            0 => [15.0, 20.0],
            1 => [85.0, 20.0],
            2 => [15.0, 155.0],
            _ => [85.0, 155.0],
        };
        CalibrationObservation {
            target_id: format!("A{id:02}"),
            sheet_id: sheet.into(),
            nominal_print_mm: [x, y],
            observed_cut_mm: [x + 0.1, y - 0.1],
            uncertainty_mm: [0.05, 0.05],
            confidence: 0.95,
            included: true,
        }
    }

    fn advance_to_print(wizard: &mut CalibrationWizard, method: CalibrationMethod) {
        wizard.select_method(method, 1).unwrap();
        assert_eq!(wizard.next(2).unwrap(), WizardStep::Prepare);
        wizard.confirm_prepared(true, 3);
        assert_eq!(wizard.next(4).unwrap(), WizardStep::PrintCalibration);
        wizard.set_job_status(JobSlot::Primary, JobStatus::Completed, 5);
    }

    #[test]
    fn flatbed_path_guards_import_validation_and_kiss_cut() {
        let mut wizard = CalibrationWizard::new("flatbed-run", 0).unwrap();
        advance_to_print(&mut wizard, CalibrationMethod::FlatbedScanner);
        assert_eq!(wizard.next(6).unwrap(), WizardStep::RemoveCenters);
        assert!(wizard.next(7).is_err());
        wizard.confirm_centers_removed(false, 8);
        assert_eq!(wizard.next(9).unwrap(), WizardStep::ImportScan);
        wizard.complete_scan_import(
            ScanSlot::Training,
            "scan.png",
            true,
            (1..=8).map(|id| observation(id, "sheet-1")).collect(),
            10,
        );
        assert_eq!(wizard.next(11).unwrap(), WizardStep::ReviewScan);
        wizard.accept_scan_review(ScanSlot::Training, 12);
        assert_eq!(wizard.next(13).unwrap(), WizardStep::Candidate);
        assert!(wizard.next(14).is_err());
        wizard.mark_candidate_ready("translation", 15);
        assert_eq!(wizard.next(16).unwrap(), WizardStep::PrintValidation);
        wizard.set_job_status(JobSlot::Validation, JobStatus::Completed, 17);
        assert_eq!(
            wizard.next(18).unwrap(),
            WizardStep::RemoveValidationCenters
        );
        wizard.confirm_centers_removed(true, 19);
        assert_eq!(wizard.next(20).unwrap(), WizardStep::ImportValidationScan);
        wizard.complete_scan_import(
            ScanSlot::Validation,
            "validation.png",
            true,
            (1..=6).map(|id| observation(id, "validation")).collect(),
            21,
        );
        assert_eq!(wizard.next(22).unwrap(), WizardStep::ReviewValidation);
        wizard.accept_scan_review(ScanSlot::Validation, 23);
        wizard.set_validation_result(true, "", 24);
        assert_eq!(wizard.next(25).unwrap(), WizardStep::KissCutInspection);
        assert!(wizard.next(26).is_err());
        wizard.set_kiss_cut_inspection(true, 27);
        assert_eq!(wizard.next(28).unwrap(), WizardStep::Finish);
    }

    #[test]
    fn manual_one_sheet_fallback_reaches_candidate_and_validation() {
        let mut wizard = CalibrationWizard::new("manual-one", 0).unwrap();
        advance_to_print(&mut wizard, CalibrationMethod::ManualEastBay);
        assert_eq!(wizard.next(6).unwrap(), WizardStep::PrintArea);
        wizard.mark_print_area_reviewed([None; 4], 7).unwrap();
        assert_eq!(wizard.next(8).unwrap(), WizardStep::PrintScale);
        wizard
            .set_print_scale(ManualSheetSlot::Primary, None, 9)
            .unwrap();
        assert_eq!(wizard.next(10).unwrap(), WizardStep::ManualTargets);
        for id in MANUAL_PRIMARY_IDS.iter().take(4) {
            wizard
                .set_manual_target(ManualSheetSlot::Primary, id, complete_edges(), false, 11)
                .unwrap();
        }
        assert_eq!(wizard.next(12).unwrap(), WizardStep::SecondSheetChoice);
        wizard.continue_with_one_sheet(13);
        assert_eq!(wizard.next(14).unwrap(), WizardStep::Candidate);
        assert_eq!(wizard.training_observations().len(), 4);
        wizard.mark_candidate_ready("translation", 15);
        assert_eq!(wizard.next(16).unwrap(), WizardStep::PrintValidation);
        wizard.set_job_status(JobSlot::Validation, JobStatus::Completed, 17);
        assert_eq!(
            wizard.next(18).unwrap(),
            WizardStep::ValidationManualTargets
        );
        for id in MANUAL_VALIDATION_IDS.iter().take(4) {
            wizard
                .set_manual_target(ManualSheetSlot::Validation, id, complete_edges(), false, 19)
                .unwrap();
        }
        assert_eq!(wizard.next(20).unwrap(), WizardStep::ReviewValidation);
        wizard.set_validation_result(true, "", 21);
        assert_eq!(wizard.next(22).unwrap(), WizardStep::Finish);
    }

    #[test]
    fn manual_second_sheet_branch_requires_six_per_sheet() {
        let mut wizard = CalibrationWizard::new("manual-two", 0).unwrap();
        advance_to_print(&mut wizard, CalibrationMethod::ManualEastBay);
        wizard.next(6).unwrap();
        wizard.mark_print_area_reviewed([None; 4], 7).unwrap();
        wizard.next(8).unwrap();
        wizard
            .set_print_scale(ManualSheetSlot::Primary, Some([80.1, 149.8]), 9)
            .unwrap();
        wizard.next(10).unwrap();
        for id in MANUAL_PRIMARY_IDS.iter().take(6) {
            wizard
                .set_manual_target(ManualSheetSlot::Primary, id, complete_edges(), false, 11)
                .unwrap();
        }
        wizard.next(12).unwrap();
        wizard.choose_second_sheet(SecondSheetChoice::MeasureAnotherSheet, 13);
        assert_eq!(wizard.next(14).unwrap(), WizardStep::PrintSecondCalibration);
        wizard.set_job_status(JobSlot::Second, JobStatus::Completed, 15);
        assert_eq!(wizard.next(16).unwrap(), WizardStep::SecondPrintScale);
        wizard
            .set_print_scale(ManualSheetSlot::Second, None, 17)
            .unwrap();
        assert_eq!(wizard.next(18).unwrap(), WizardStep::SecondManualTargets);
        for id in MANUAL_PRIMARY_IDS.iter().take(5) {
            wizard
                .set_manual_target(ManualSheetSlot::Second, id, complete_edges(), false, 19)
                .unwrap();
        }
        assert!(!wizard.second_sheet_merge_eligible());
        assert!(wizard.next(20).is_err());
        let mut fallback = wizard.clone();
        fallback.continue_with_one_sheet(21);
        assert_eq!(fallback.next(22).unwrap(), WizardStep::Candidate);
        assert_eq!(fallback.training_observations().len(), 6);
        wizard
            .set_manual_target(ManualSheetSlot::Second, "C6", complete_edges(), false, 23)
            .unwrap();
        assert!(wizard.second_sheet_merge_eligible());
        assert_eq!(wizard.next(24).unwrap(), WizardStep::Candidate);
        assert_eq!(wizard.training_observations().len(), 12);
    }

    #[test]
    fn validation_observations_never_enter_training_data() {
        let mut wizard = CalibrationWizard::new("separation", 0).unwrap();
        wizard
            .select_method(CalibrationMethod::FlatbedScanner, 1)
            .unwrap();
        wizard.complete_scan_import(
            ScanSlot::Training,
            "train.png",
            true,
            (1..=8).map(|id| observation(id, "training")).collect(),
            2,
        );
        wizard.complete_scan_import(
            ScanSlot::Validation,
            "validation.png",
            true,
            (1..=6).map(|id| observation(id, "validation")).collect(),
            3,
        );
        let training = wizard.training_observations();
        assert_eq!(training.len(), 8);
        assert!(training.iter().all(|value| value.sheet_id == "training"));
        assert_eq!(wizard.validation_observations.len(), 6);
        let validation = wizard.validation_observations_for_evaluation();
        assert_eq!(validation.len(), 6);
        assert!(
            validation
                .iter()
                .all(|value| value.sheet_id == "validation")
        );
    }

    #[test]
    fn training_edit_invalidates_every_validation_artifact_and_generation() {
        let mut wizard = CalibrationWizard::new("candidate-generation", 0).unwrap();
        wizard
            .select_method(CalibrationMethod::ManualEastBay, 1)
            .unwrap();
        wizard.mark_candidate_ready("translation", 2);
        wizard.set_job_status(JobSlot::Validation, JobStatus::Completed, 3);
        wizard.confirm_centers_removed(true, 4);
        wizard.complete_scan_import(
            ScanSlot::Validation,
            "old-candidate.png",
            true,
            (1..=6).map(|id| observation(id, "old-candidate")).collect(),
            5,
        );
        wizard.accept_scan_review(ScanSlot::Validation, 6);
        wizard
            .set_manual_target(
                ManualSheetSlot::Validation,
                "V1",
                complete_edges(),
                false,
                7,
            )
            .unwrap();
        wizard.set_validation_result(true, "passed", 8);
        wizard.set_kiss_cut_inspection(true, 9);
        let previous_generation = wizard.validation_generation;

        wizard
            .set_manual_target(ManualSheetSlot::Primary, "C1", complete_edges(), false, 10)
            .unwrap();

        assert_eq!(wizard.candidate, CandidateStatus::NotStarted);
        assert_eq!(wizard.validation, ValidationStatus::NotStarted);
        assert_eq!(wizard.validation_job, JobStatus::NotStarted);
        assert!(!wizard.validation_centers_removed);
        assert_eq!(wizard.validation_scan, ScanImportStatus::NotImported);
        assert!(!wizard.validation_scan_reviewed);
        assert!(wizard.validation_observations.is_empty());
        assert_eq!(wizard.manual_validation.accepted_count(), 0);
        assert_eq!(wizard.normal_kiss_cut_passed, None);
        assert_eq!(
            wizard.validation_generation,
            previous_generation.saturating_add(1)
        );
    }

    #[test]
    fn save_resume_back_and_discard_are_explicit() {
        let mut wizard = CalibrationWizard::new("resume-run", 10).unwrap();
        wizard
            .select_method(CalibrationMethod::ManualEastBay, 11)
            .unwrap();
        wizard.next(12).unwrap();
        wizard.confirm_prepared(true, 13);
        wizard.next(14).unwrap();
        let json = wizard.save_json(15).unwrap();
        let mut resumed = CalibrationWizard::resume_json(&json).unwrap();
        assert_eq!(resumed.step, WizardStep::PrintCalibration);
        assert_eq!(resumed.back(16).unwrap(), WizardStep::Prepare);
        resumed.discard(17);
        assert_eq!(resumed.next(18), Err(WizardError::Discarded));
        let discarded = serde_json::to_string(&resumed).unwrap();
        assert_eq!(
            CalibrationWizard::resume_json(&discarded),
            Err(WizardError::Discarded)
        );
    }

    #[test]
    fn manual_next_requires_translation_coverage_not_only_count() {
        let mut wizard = CalibrationWizard::new("manual-coverage", 0).unwrap();
        wizard
            .select_method(CalibrationMethod::ManualEastBay, 1)
            .unwrap();
        wizard.step = WizardStep::ManualTargets;
        wizard
            .set_print_scale(ManualSheetSlot::Primary, None, 2)
            .unwrap();
        for id in ["C1", "C3", "C4", "C6"] {
            wizard
                .set_manual_target(ManualSheetSlot::Primary, id, complete_edges(), false, 3)
                .unwrap();
        }
        assert_eq!(
            wizard.manual_translation_coverage_issue(ManualSheetSlot::Primary),
            Some("measure at least one target on the right side of the sheet")
        );
        assert!(matches!(
            wizard.next(4),
            Err(WizardError::TransitionBlocked(reason)) if reason.contains("right side")
        ));

        wizard
            .set_manual_target(ManualSheetSlot::Primary, "C2", complete_edges(), false, 5)
            .unwrap();
        assert_eq!(
            wizard.manual_translation_coverage_issue(ManualSheetSlot::Primary),
            None
        );
        assert_eq!(wizard.next(6).unwrap(), WizardStep::SecondSheetChoice);
    }

    #[test]
    fn print_restart_clears_evidence_owned_by_the_old_physical_sheet() {
        let mut wizard = CalibrationWizard::new("reprint-reset", 0).unwrap();
        wizard
            .select_method(CalibrationMethod::FlatbedScanner, 1)
            .unwrap();
        wizard.primary_job = JobStatus::Completed;
        wizard.primary_centers_removed = true;
        wizard.complete_scan_import(
            ScanSlot::Training,
            "old-training.png",
            true,
            (1..=8).map(|id| observation(id, "old-primary")).collect(),
            2,
        );
        wizard.accept_scan_review(ScanSlot::Training, 3);
        wizard.mark_candidate_ready("translation", 4);

        wizard.begin_print_job(JobSlot::Primary, 5).unwrap();
        assert_eq!(wizard.primary_job, JobStatus::Queued);
        assert!(!wizard.primary_centers_removed);
        assert_eq!(wizard.training_scan, ScanImportStatus::NotImported);
        assert!(wizard.scan_training_observations.is_empty());
        assert!(!wizard.training_scan_reviewed);
        assert_eq!(wizard.candidate, CandidateStatus::NotStarted);

        wizard.mark_candidate_ready("translation", 6);
        wizard.validation_job = JobStatus::Completed;
        wizard.validation_centers_removed = true;
        wizard.complete_scan_import(
            ScanSlot::Validation,
            "old-validation.png",
            true,
            (1..=6)
                .map(|id| observation(id, "old-validation"))
                .collect(),
            7,
        );
        wizard.accept_scan_review(ScanSlot::Validation, 8);
        wizard.set_validation_result(true, "passed", 9);
        wizard.set_kiss_cut_inspection(true, 10);

        wizard.begin_print_job(JobSlot::Validation, 11).unwrap();
        assert_eq!(wizard.validation_job, JobStatus::Queued);
        assert!(!wizard.validation_centers_removed);
        assert_eq!(wizard.validation_scan, ScanImportStatus::NotImported);
        assert!(wizard.validation_observations.is_empty());
        assert!(!wizard.validation_scan_reviewed);
        assert_eq!(wizard.validation, ValidationStatus::Collecting);
        assert_eq!(wizard.normal_kiss_cut_passed, None);
        assert!(matches!(wizard.candidate, CandidateStatus::Ready { .. }));
    }

    #[test]
    fn manual_uncertainty_preset_is_validated_persisted_and_invalidates_fit() {
        let mut wizard = CalibrationWizard::new("manual-precision", 0).unwrap();
        wizard
            .select_method(CalibrationMethod::ManualEastBay, 1)
            .unwrap();
        wizard.mark_candidate_ready("translation", 2);
        let generation = wizard.validation_generation;

        wizard.set_manual_uncertainty(0.25, 3).unwrap();
        assert_eq!(wizard.manual_uncertainty_mm, 0.25);
        assert_eq!(wizard.candidate, CandidateStatus::NotStarted);
        assert!(wizard.validation_generation > generation);
        assert_eq!(
            wizard.set_manual_uncertainty(0.0, 4),
            Err(WizardError::InvalidMeasurement)
        );

        let json = wizard.save_json(5).unwrap();
        let resumed = CalibrationWizard::resume_json(&json).unwrap();
        assert_eq!(resumed.manual_uncertainty_mm, 0.25);
    }

    #[test]
    fn manual_validation_inherits_primary_print_scale() {
        let mut wizard = CalibrationWizard::new("validation-scale", 0).unwrap();
        wizard
            .select_method(CalibrationMethod::ManualEastBay, 1)
            .unwrap();
        wizard
            .set_print_scale(ManualSheetSlot::Primary, Some([40.0, 150.0]), 2)
            .unwrap();
        wizard
            .set_manual_target(
                ManualSheetSlot::Validation,
                "V1",
                complete_edges(),
                false,
                3,
            )
            .unwrap();

        let observations = wizard.validation_observations_for_evaluation();
        assert_eq!(observations.len(), 1);
        // complete_edges has a raw +0.2 mm X displacement. The inherited
        // H80 ratio is 0.5, so logical displacement is +0.4 mm.
        assert!(
            (observations[0].observed_cut_mm[0] - observations[0].nominal_print_mm[0] - 0.4).abs()
                < 1e-12
        );
        assert!(
            (observations[0].observed_cut_mm[1] - observations[0].nominal_print_mm[1] + 0.1).abs()
                < 1e-12
        );
    }

    #[test]
    fn excluding_scan_targets_rechecks_coverage_before_review_acceptance() {
        let mut wizard = CalibrationWizard::new("scan-exclusion", 0).unwrap();
        wizard
            .select_method(CalibrationMethod::FlatbedScanner, 1)
            .unwrap();
        wizard.complete_scan_import(
            ScanSlot::Training,
            "scan.png",
            true,
            (1..=8).map(|id| observation(id, "sheet-1")).collect(),
            2,
        );
        assert_eq!(wizard.scan_review_coverage_issue(ScanSlot::Training), None);
        wizard
            .set_scan_target_included(ScanSlot::Training, "A01", false, 3)
            .unwrap();
        assert_eq!(
            wizard.scan_review_coverage_issue(ScanSlot::Training),
            Some("include at least eight reliable aperture detections")
        );
        wizard.accept_scan_review(ScanSlot::Training, 4);
        assert!(!wizard.training_scan_reviewed);
        wizard
            .set_scan_target_included(ScanSlot::Training, "A01", true, 5)
            .unwrap();
        wizard.accept_scan_review(ScanSlot::Training, 6);
        assert!(wizard.training_scan_reviewed);
    }

    #[test]
    fn attribution_constants_are_stable_and_linked() {
        assert!(EAST_BAY_METHOD_COPY.contains("East Bay Makers Club"));
        assert!(EAST_BAY_RESULTS_CREDIT.contains("pixcut-s1"));
        assert_eq!(
            EAST_BAY_SOURCE_URL,
            "https://github.com/eastbaymakersclub/pixcut-s1#calibration-workflow"
        );
    }
}
