//! Host-agnostic egui walkthrough for print-to-cut calibration.
//!
//! The renderer updates [`CalibrationWizard`] only for synchronous form and
//! navigation actions. Operations owned by the application (printing, file
//! picking, solving, persistence, and profile activation) are returned as
//! [`CalibrationUiEvent`] values.

use std::{collections::BTreeMap, sync::Arc};

use egui::{Color32, RichText, ScrollArea, Ui};

use crate::calibration::{
    CalibrationMethod, CalibrationSolution, CalibrationWizard, CandidateStatus,
    EAST_BAY_METHOD_COPY, EAST_BAY_RESULTS_CREDIT, EAST_BAY_SOURCE_URL, JobSlot, JobStatus,
    ManifestIdentity, ManualEdgeDraft, ManualSheetDraft, ManualSheetSlot, OptionalMeasurementState,
    ScanAnalysisReport, ScanFailureReason, ScanImportStatus, ScanSlot, ScanTargetStatus,
    SecondSheetChoice, TargetKind, ValidationMetrics, ValidationStatus, WizardStep,
    flatbed_calibration, flatbed_validation, manual_calibration, manual_validation,
};

const WIDE_LAYOUT_MIN_POINTS: f32 = 680.0;
const CALIBRATION_WINDOW_MAX_HEIGHT: f32 = 760.0;
const CALIBRATION_WINDOW_MARGIN: f32 = 24.0;
// Title bar, header, separators, and wrapped footer controls live outside the
// scrolling body. Reserving them from the viewport-derived window cap keeps
// navigation reachable even during egui's first sizing pass.
const CALIBRATION_NON_BODY_HEIGHT: f32 = 340.0;

/// Work that must be performed by the application hosting the walkthrough.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CalibrationUiEvent {
    PrintPrimary,
    UseExistingPrimarySheet,
    PrintSecond,
    PrintValidation,
    ImportTrainingScan,
    ImportValidationScan,
    ComputeCandidate,
    EvaluateValidation,
    ActivateProfile,
    SaveAndExit,
    Discard,
}

/// Transient presentation state. The durable workflow state remains in
/// [`CalibrationWizard`] and can be saved independently of this value.
#[derive(Debug)]
pub struct CalibrationUiState {
    pub open: bool,
    pub last_error: Option<String>,
    pub selected_printer: String,
    pub selected_media: String,
    confirm_discard: bool,
    fullscreen_scan: Option<ScanSlot>,
    text_fields: BTreeMap<String, String>,
}

impl Default for CalibrationUiState {
    fn default() -> Self {
        Self {
            open: true,
            last_error: None,
            selected_printer: String::new(),
            selected_media: String::new(),
            confirm_discard: false,
            fullscreen_scan: None,
            text_fields: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct CalibrationUiDiagnostics<'a> {
    pub training_scan: Option<&'a ScanAnalysisReport>,
    pub validation_scan: Option<&'a ScanAnalysisReport>,
    pub training_scan_preview_png: Option<&'a Arc<[u8]>>,
    pub validation_scan_preview_png: Option<&'a Arc<[u8]>>,
    pub training_scan_preview_sha1: Option<&'a str>,
    pub validation_scan_preview_sha1: Option<&'a str>,
    pub candidate: Option<&'a CalibrationSolution>,
    pub validation: Option<&'a ValidationMetrics>,
}

/// Render the calibration window and return any host-owned actions requested
/// during this frame.
pub fn show_calibration_wizard(
    ctx: &egui::Context,
    wizard: &mut CalibrationWizard,
    manifest_identity: &ManifestIdentity,
    state: &mut CalibrationUiState,
    now: u64,
    diagnostics: CalibrationUiDiagnostics<'_>,
) -> Vec<CalibrationUiEvent> {
    if !state.open {
        state.fullscreen_scan = None;
        return Vec::new();
    }
    if ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
        if state.fullscreen_scan.take().is_some() {
            // Close only the evidence viewer.
        } else {
            state.confirm_discard = !state.confirm_discard;
        }
    }

    let mut events = Vec::new();
    let mut window_open = state.open;
    let (window_min_height, window_max_height) =
        calibration_window_height_limits(ctx.content_rect().height());
    egui::Window::new("Print-to-cut calibration")
        .id(egui::Id::new("print_to_cut_calibration_wizard"))
        .open(&mut window_open)
        .collapsible(false)
        .constrain_to(ctx.content_rect())
        .resizable(true)
        .default_width(780.0)
        .min_width(340.0)
        .min_height(window_min_height)
        .max_height(window_max_height)
        .show(ctx, |ui| {
            if state.confirm_discard || state.fullscreen_scan.is_some() {
                ui.disable();
            }
            render_header(ui, wizard);
            ui.separator();

            // Keep the navigation controls visible; step content and the rail
            // independently scroll within whatever height the viewport leaves.
            let body_height = (window_max_height - CALIBRATION_NON_BODY_HEIGHT).max(0.0);
            if ui.available_width() >= WIDE_LAYOUT_MIN_POINTS {
                ui.horizontal_top(|ui| {
                    ui.allocate_ui_with_layout(
                        egui::vec2(176.0, body_height),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            ScrollArea::vertical()
                                .id_salt("calibration_step_rail")
                                .max_height(body_height)
                                .show(ui, |ui| render_step_rail(ui, wizard));
                        },
                    );
                    ui.separator();
                    ui.allocate_ui_with_layout(
                        egui::vec2(ui.available_width(), body_height),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            ScrollArea::vertical()
                                .id_salt("calibration_step_body_wide")
                                .max_height(body_height)
                                .show(ui, |ui| {
                                    render_step(
                                        ui,
                                        wizard,
                                        manifest_identity,
                                        state,
                                        now,
                                        diagnostics,
                                        &mut events,
                                    )
                                });
                        },
                    );
                });
            } else {
                render_compact_progress(ui, wizard);
                ui.separator();
                ScrollArea::vertical()
                    .id_salt("calibration_step_body_compact")
                    .max_height(body_height)
                    .show(ui, |ui| {
                        render_step(
                            ui,
                            wizard,
                            manifest_identity,
                            state,
                            now,
                            diagnostics,
                            &mut events,
                        )
                    });
            }

            ui.separator();
            render_footer(ui, wizard, state, now, &mut events);
        });

    // Save/Discard buttons close explicitly. The title-bar button enters the
    // same leave-confirmation flow as Escape rather than choosing for users.
    let explicitly_closed = events.iter().any(|event| {
        matches!(
            event,
            CalibrationUiEvent::SaveAndExit | CalibrationUiEvent::Discard
        )
    });
    if explicitly_closed {
        state.open = false;
    } else if state.open && !window_open {
        // The title-bar close button follows the same explicit leave flow as
        // Escape and Discard instead of silently choosing Save & Exit.
        state.confirm_discard = true;
        state.open = true;
    } else {
        state.open = window_open;
    }
    if state.open && state.confirm_discard {
        render_discard_confirmation(ctx, wizard, state, now, &mut events);
    } else if state.open {
        render_fullscreen_scan(ctx, wizard, state, diagnostics);
    } else {
        state.fullscreen_scan = None;
    }
    events
}

fn render_header(ui: &mut Ui, wizard: &CalibrationWizard) {
    ui.horizontal_wrapped(|ui| {
        ui.heading(wizard.step.label(wizard.method));
        ui.weak(format!("Run {}", wizard.run_id));
    });
    ui.label(step_summary(wizard.step, wizard.method));
}

fn render_step_rail(ui: &mut Ui, wizard: &CalibrationWizard) {
    ui.strong("Progress");
    ui.add_space(6.0);
    let journey = journey_steps(wizard.method);
    let active = journey
        .iter()
        .position(|step| *step == wizard.step)
        .unwrap_or_default();
    for (index, step) in journey.iter().enumerate() {
        let prefix = if index < active {
            "✓"
        } else if index == active {
            "●"
        } else {
            "○"
        };
        let text = format!("{prefix} {}", step.label(wizard.method));
        if index == active {
            ui.label(RichText::new(text).strong());
        } else {
            ui.label(RichText::new(text).color(ui.visuals().weak_text_color()));
        }
    }
}

fn render_compact_progress(ui: &mut Ui, wizard: &CalibrationWizard) {
    let journey = journey_steps(wizard.method);
    let active = journey
        .iter()
        .position(|step| *step == wizard.step)
        .unwrap_or_default();
    ui.horizontal_wrapped(|ui| {
        ui.strong(format!("Step {} of {}", active + 1, journey.len()));
        ui.weak(wizard.step.label(wizard.method));
    });
    ui.add(
        egui::ProgressBar::new((active + 1) as f32 / journey.len().max(1) as f32).show_percentage(),
    );
}

fn render_step(
    ui: &mut Ui,
    wizard: &mut CalibrationWizard,
    manifest_identity: &ManifestIdentity,
    state: &mut CalibrationUiState,
    now: u64,
    diagnostics: CalibrationUiDiagnostics<'_>,
    events: &mut Vec<CalibrationUiEvent>,
) {
    ui.add_space(6.0);
    match wizard.step {
        WizardStep::ChooseMethod => render_method_choice(ui, wizard, state, now),
        WizardStep::Prepare => render_prepare(ui, wizard, now),
        WizardStep::PrintCalibration => {
            render_target_preview(ui, wizard, manifest_identity, false);
            render_print_job(
                ui,
                wizard,
                JobSlot::Primary,
                "Print and cut the calibration sheet",
                CalibrationUiEvent::PrintPrimary,
                state,
                events,
            )
        }
        WizardStep::RemoveCenters => render_remove_centers(ui, wizard, false, now),
        WizardStep::ImportScan => render_scan_import(ui, &wizard.training_scan, false, events),
        WizardStep::ReviewScan => render_scan_review(
            ui,
            wizard,
            state,
            false,
            now,
            diagnostics.training_scan,
            diagnostics
                .training_scan_preview_png
                .zip(diagnostics.training_scan_preview_sha1),
        ),
        WizardStep::PrintArea => render_print_area(ui, wizard, state, now),
        WizardStep::PrintScale => {
            render_print_scale(ui, wizard, state, ManualSheetSlot::Primary, now)
        }
        WizardStep::ManualTargets => {
            render_manual_targets(ui, wizard, state, ManualSheetSlot::Primary, now)
        }
        WizardStep::SecondSheetChoice => render_second_sheet_choice(ui, wizard, now),
        WizardStep::PrintSecondCalibration => {
            render_target_preview(ui, wizard, manifest_identity, false);
            render_print_job(
                ui,
                wizard,
                JobSlot::Second,
                "Print and cut a freshly loaded second sheet",
                CalibrationUiEvent::PrintSecond,
                state,
                events,
            )
        }
        WizardStep::SecondPrintScale => {
            render_print_scale(ui, wizard, state, ManualSheetSlot::Second, now)
        }
        WizardStep::SecondManualTargets => {
            render_manual_targets(ui, wizard, state, ManualSheetSlot::Second, now);
            if ui
                .button("Abandon second sheet and use the first")
                .clicked()
            {
                wizard.continue_with_one_sheet(now);
            }
        }
        WizardStep::Candidate => render_candidate(ui, wizard, now, diagnostics.candidate, events),
        WizardStep::PrintValidation => {
            render_target_preview(ui, wizard, manifest_identity, true);
            render_print_job(
                ui,
                wizard,
                JobSlot::Validation,
                "Print and cut a new validation sheet",
                CalibrationUiEvent::PrintValidation,
                state,
                events,
            )
        }
        WizardStep::RemoveValidationCenters => render_remove_centers(ui, wizard, true, now),
        WizardStep::ImportValidationScan => {
            render_scan_import(ui, &wizard.validation_scan, true, events)
        }
        WizardStep::ReviewValidation => render_validation_review(
            ui,
            wizard,
            now,
            diagnostics.validation_scan,
            diagnostics.validation_scan_preview_png,
            diagnostics.validation_scan_preview_sha1,
            diagnostics.validation,
            state,
            events,
        ),
        WizardStep::ValidationManualTargets => {
            render_manual_targets(ui, wizard, state, ManualSheetSlot::Validation, now)
        }
        WizardStep::KissCutInspection => render_kiss_cut_inspection(ui, wizard, now),
        WizardStep::Finish => render_finish(ui, wizard, diagnostics.validation, events),
    }
    ui.add_space(12.0);
}

fn render_method_choice(
    ui: &mut Ui,
    wizard: &mut CalibrationWizard,
    state: &mut CalibrationUiState,
    now: u64,
) {
    if !state.selected_printer.is_empty() {
        ui.strong(format!("Printer: {}", state.selected_printer));
    }
    if !state.selected_media.is_empty() {
        ui.label(format!("Media: {}", state.selected_media));
    }
    ui.label("Choose the workflow that matches the equipment and time you have available.");
    ui.add_space(8.0);
    let wide = ui.available_width() >= 580.0;
    if wide {
        ui.columns(2, |columns| {
            method_card(
                &mut columns[0],
                wizard,
                state,
                now,
                CalibrationMethod::FlatbedScanner,
            );
            method_card(
                &mut columns[1],
                wizard,
                state,
                now,
                CalibrationMethod::ManualEastBay,
            );
        });
    } else {
        method_card(ui, wizard, state, now, CalibrationMethod::FlatbedScanner);
        ui.add_space(8.0);
        method_card(ui, wizard, state, now, CalibrationMethod::ManualEastBay);
    }
}

fn method_card(
    ui: &mut Ui,
    wizard: &mut CalibrationWizard,
    state: &mut CalibrationUiState,
    now: u64,
    method: CalibrationMethod,
) {
    let selected = wizard.method == Some(method);
    let fill = if selected {
        ui.visuals().selection.bg_fill.gamma_multiply(0.32)
    } else {
        ui.visuals().faint_bg_color
    };
    egui::Frame::group(ui.style()).fill(fill).show(ui, |ui| {
        ui.set_min_height(176.0);
        match method {
            CalibrationMethod::FlatbedScanner => {
                ui.heading("Flatbed Scanner");
                ui.label("Best repeatability");
                ui.add_space(4.0);
                ui.label("Print, cut, remove the through-cut centers, then scan against a clean white backing sheet. Detection and fitting are automatic.");
                ui.add_space(8.0);
                if ui.selectable_label(selected, "Use Flatbed Scanner").clicked() {
                    set_method(wizard, state, method, now);
                }
            }
            CalibrationMethod::ManualEastBay => {
                ui.heading("Manual");
                ui.label("No scanner required");
                ui.add_space(4.0);
                ui.label(EAST_BAY_METHOD_COPY);
                ui.hyperlink_to("View the documented method", EAST_BAY_SOURCE_URL);
                ui.add_space(8.0);
                if ui.selectable_label(selected, "Use Manual").clicked() {
                    set_method(wizard, state, method, now);
                }
            }
        }
    });
}

fn set_method(
    wizard: &mut CalibrationWizard,
    state: &mut CalibrationUiState,
    method: CalibrationMethod,
    now: u64,
) {
    if let Err(error) = wizard.select_method(method, now) {
        state.last_error = Some(error.to_string());
    } else {
        state.last_error = None;
    }
}

fn render_prepare(ui: &mut Ui, wizard: &mut CalibrationWizard, now: u64) {
    ui.strong("Before you start");
    ui.label("• Use the same printer, quality, scaling, paper, and cutter settings as production.");
    ui.label("• Print at 100% / Actual size. Disable Fit, Shrink, and borderless expansion.");
    ui.label("• Load the sheet squarely and keep its orientation unchanged.");
    if wizard.method == Some(CalibrationMethod::FlatbedScanner) {
        ui.label("• Have a flatbed scanner and a clean, unprinted white backing sheet ready.");
    } else {
        ui.label("• Have a precise ruler or calipers ready; measure in millimetres.");
        ui.label(EAST_BAY_METHOD_COPY);
        ui.hyperlink_to("View the documented method", EAST_BAY_SOURCE_URL);
        ui.add_space(8.0);
        ui.strong("Measuring-tool precision");
        ui.label("Choose the closest preset so the fit does not over-trust coarse measurements.");
        let presets = [
            (0.05, "Digital calipers · 0.05 mm"),
            (0.25, "Fine metric ruler · 0.25 mm"),
            (0.50, "Basic metric ruler · 0.50 mm"),
        ];
        for (uncertainty, label) in presets {
            let selected = (wizard.manual_uncertainty_mm - uncertainty).abs() < 1e-9;
            if ui.selectable_label(selected, label).clicked() {
                let _ = wizard.set_manual_uncertainty(uncertainty, now);
            }
        }
        ui.weak(format!(
            "Current measurement uncertainty: {:.2} mm",
            wizard.manual_uncertainty_mm
        ));
    }
    ui.add_space(8.0);
    let mut confirmed = wizard.prepared;
    if ui
        .checkbox(&mut confirmed, "I have checked these settings")
        .changed()
    {
        wizard.confirm_prepared(confirmed, now);
    }
}

fn render_target_preview(
    ui: &mut Ui,
    wizard: &CalibrationWizard,
    manifest_identity: &ManifestIdentity,
    validation: bool,
) {
    let Some(method) = wizard.method else {
        return;
    };
    let mut identity = manifest_identity.clone();
    if validation {
        identity.run_id.push_str("-validation");
    }
    let manifest = match (method, validation) {
        (CalibrationMethod::FlatbedScanner, false) => flatbed_calibration(identity),
        (CalibrationMethod::FlatbedScanner, true) => flatbed_validation(identity),
        (CalibrationMethod::ManualEastBay, false) => manual_calibration(identity),
        (CalibrationMethod::ManualEastBay, true) => manual_validation(identity),
    };
    let Ok(manifest) = manifest else {
        return;
    };
    ui.label("Target preview");
    let height = 210.0_f32.min(ui.available_height().max(120.0));
    let size = egui::vec2(height * 4.0 / 7.0, height);
    let (response, painter) = ui.allocate_painter(size, egui::Sense::hover());
    let sheet = response.rect;
    painter.rect_filled(sheet, 3.0, Color32::WHITE);
    painter.rect_stroke(
        sheet,
        3.0,
        egui::Stroke::new(1.0_f32, Color32::GRAY),
        egui::StrokeKind::Inside,
    );
    let project = |point: [f64; 2]| {
        egui::pos2(
            sheet.left() + (point[0] / manifest.canvas.width_mm) as f32 * sheet.width(),
            sheet.top() + (point[1] / manifest.canvas.height_mm) as f32 * sheet.height(),
        )
    };
    for target in &manifest.targets {
        let center = project([target.center_mm.x, target.center_mm.y]);
        let color = match target.kind {
            TargetKind::FlatbedAperture => Color32::from_rgb(30, 100, 210),
            TargetKind::ManualRectangle => Color32::from_rgb(210, 80, 55),
            TargetKind::KissCutCheck => Color32::from_rgb(60, 155, 95),
        };
        let radius = if target.kind == TargetKind::FlatbedAperture {
            4.0
        } else {
            3.0
        };
        painter.circle_stroke(center, radius, egui::Stroke::new(1.5_f32, color));
    }
    response.on_hover_text("Generated 4×7 target layout; output is printed at exactly 300 DPI.");
}

fn render_print_job(
    ui: &mut Ui,
    wizard: &CalibrationWizard,
    slot: JobSlot,
    heading: &str,
    event: CalibrationUiEvent,
    state: &mut CalibrationUiState,
    events: &mut Vec<CalibrationUiEvent>,
) {
    let status = match slot {
        JobSlot::Primary => wizard.primary_job,
        JobSlot::Second => wizard.second_job,
        JobSlot::Validation => wizard.validation_job,
    };
    ui.heading(heading);
    ui.label("Keep scaling at 100% / Actual size and use the production print-and-cut settings.");
    if slot == JobSlot::Primary && wizard.method == Some(CalibrationMethod::FlatbedScanner) {
        ui.weak("You can reuse an earlier flatbed calibration sheet when it has the same 12-aperture target layout. Validation sheets are not interchangeable.");
    }
    ui.add_space(8.0);
    ui.label(format!("Job status: {}", job_status_label(status)));
    if ui
        .add_enabled(
            !matches!(status, JobStatus::Queued | JobStatus::InProgress),
            egui::Button::new(if status.has_physical_output() {
                "Print again"
            } else {
                "Print and cut"
            }),
        )
        .clicked()
    {
        state.last_error = None;
        clear_slot_text_fields(state, slot);
        events.push(event);
    }
    if !status.has_physical_output() {
        ui.weak("Next becomes available after the host reports that the job completed.");
    }
    if status == JobStatus::Failed {
        ui.colored_label(
            Color32::YELLOW,
            "Check the printer and production queue before printing again. A failure after printer motion may already have produced a sheet.",
        );
    }
}

fn render_remove_centers(ui: &mut Ui, wizard: &mut CalibrationWizard, validation: bool, now: u64) {
    ui.heading("Create high-contrast apertures");
    ui.label("Remove every small perforated center completely. Leave the surrounding printed target and the rest of the sticker material in place.");
    ui.label("Scanner stack, bottom to top: clean glass → printed face down → translucent liner → clean plain-white printer paper → scanner lid. Use light lid pressure to keep everything flat.");
    ui.label(
        "The white sheet masks the repeating ‘Back Side’ pattern and scanner-bed discoloration.",
    );
    ui.add_space(8.0);
    let mut checked = if validation {
        wizard.validation_centers_removed
    } else {
        wizard.primary_centers_removed
    };
    if ui
        .checkbox(
            &mut checked,
            "All removable centers are out; white backing is ready",
        )
        .changed()
        && checked
    {
        wizard.confirm_centers_removed(validation, now);
    }
}

fn render_scan_import(
    ui: &mut Ui,
    status: &ScanImportStatus,
    validation: bool,
    events: &mut Vec<CalibrationUiEvent>,
) {
    ui.heading(if validation {
        "Import the validation scan"
    } else {
        "Import the calibration scan"
    });
    ui.label("Put the printed face directly against clean scanner glass, with the plain-white backing sheet immediately behind the translucent liner. Close the lid with light pressure so the stack stays flat.");
    ui.label("Scan the complete sheet at 600 DPI in color. PNG is preferred; JPEG is accepted. Disable automatic cropping, deskewing, cleanup, sharpening, and perspective correction.");
    ui.add_space(8.0);
    render_scan_status(ui, status, validation);
    let event = if validation {
        CalibrationUiEvent::ImportValidationScan
    } else {
        CalibrationUiEvent::ImportTrainingScan
    };
    if ui
        .add_enabled(
            !matches!(status, ScanImportStatus::Importing { .. }),
            egui::Button::new("Choose scan image…"),
        )
        .clicked()
    {
        events.push(event);
    }
}

fn render_scan_status(ui: &mut Ui, status: &ScanImportStatus, validation: bool) {
    let required = if validation { 6 } else { 8 };
    match status {
        ScanImportStatus::NotImported => {
            ui.weak("No scan imported yet.");
            ui.weak(format!(
                "Acceptance requires at least {required} targets distributed across all four sheet quadrants."
            ));
        }
        ScanImportStatus::Importing { file_name } => {
            ui.spinner();
            ui.label(format!("Analyzing {file_name}"));
        }
        ScanImportStatus::Imported {
            file_name,
            accepted_targets,
            all_quadrants,
        } => {
            ui.label(format!("{file_name}: {accepted_targets} targets accepted"));
            ui.label(if *all_quadrants {
                "Coverage: all four sheet quadrants"
            } else {
                "Coverage is incomplete; rescan the full sheet"
            });
            if *accepted_targets < required {
                ui.colored_label(
                    Color32::YELLOW,
                    format!(
                        "Need {} more accepted target(s). Remove retained centers, flatten the stack, and rescan.",
                        required - *accepted_targets
                    ),
                );
            }
        }
        ScanImportStatus::Failed { message } => {
            ui.colored_label(Color32::from_rgb(210, 70, 70), message);
            ui.label("Check that the printed face is against the glass, every center and its liner are removed, the white backing is clean, and the entire sheet is visible.");
        }
    }
}

fn render_scan_review(
    ui: &mut Ui,
    wizard: &mut CalibrationWizard,
    state: &mut CalibrationUiState,
    validation: bool,
    now: u64,
    report: Option<&ScanAnalysisReport>,
    preview: Option<(&Arc<[u8]>, &str)>,
) {
    let (status, mut reviewed) = if validation {
        (&wizard.validation_scan, wizard.validation_scan_reviewed)
    } else {
        (&wizard.training_scan, wizard.training_scan_reviewed)
    };
    ui.heading("Review detected targets");
    render_scan_status(ui, status, validation);
    ui.label("Check that detections follow the cut apertures, not the printed target edges or the liner pattern.");
    if let Some(report) = report {
        ui.small(format!(
            "Orientation: {:?} · fiducial fit RMS {:.2} px · backing RGB {:.0}/{:.0}/{:.0}",
            report.orientation,
            report.fiducial_rms_px,
            report.backing_rgb[0],
            report.backing_rgb[1],
            report.backing_rgb[2]
        ));
        if let Some((preview_png, preview_sha1)) = preview {
            render_scan_overlay(
                ui,
                report,
                preview_png,
                preview_sha1,
                validation,
                wizard,
                state,
            );
        } else {
            ui.colored_label(
                Color32::YELLOW,
                "The visual preview is unavailable after restart. Re-import the scan before accepting detections.",
            );
        }
        egui::Grid::new(if validation {
            "validation_scan_target_table"
        } else {
            "training_scan_target_table"
        })
        .striped(true)
        .show(ui, |ui| {
            ui.strong("Target");
            ui.strong("Result");
            ui.strong("Confidence");
            ui.strong("Center (mm)");
            ui.strong("Use");
            ui.end_row();
            for target in &report.targets {
                ui.label(&target.target_id);
                ui.label(scan_target_status_label(target.status));
                ui.label(format!("{:.0}%", target.confidence * 100.0));
                ui.label(target.observed_center_mm.map_or_else(
                    || "—".into(),
                    |center| format!("{:.2}, {:.2}", center[0], center[1]),
                ));
                let slot = if validation {
                    ScanSlot::Validation
                } else {
                    ScanSlot::Training
                };
                let observations = if validation {
                    &wizard.validation_observations
                } else {
                    &wizard.scan_training_observations
                };
                let mut included = observations
                    .iter()
                    .find(|observation| observation.target_id == target.target_id)
                    .is_some_and(|observation| observation.included);
                if ui
                    .add_enabled(
                        target.status == ScanTargetStatus::Accepted,
                        egui::Checkbox::without_text(&mut included),
                    )
                    .on_hover_text(
                        "Exclude a damaged or suspicious automatic detection from the fit",
                    )
                    .changed()
                {
                    let _ = wizard.set_scan_target_included(slot, &target.target_id, included, now);
                }
                ui.end_row();
            }
        });
        let mut reasons = report
            .targets
            .iter()
            .filter_map(|target| match target.status {
                ScanTargetStatus::Accepted => None,
                ScanTargetStatus::Review(reason) | ScanTargetStatus::Missing(reason) => {
                    Some(reason)
                }
            })
            .collect::<Vec<_>>();
        reasons.sort_by_key(|reason| *reason as u8);
        reasons.dedup();
        for reason in reasons {
            ui.weak(scan_failure_remediation(reason));
        }
    }
    let slot = if validation {
        ScanSlot::Validation
    } else {
        ScanSlot::Training
    };
    let coverage_issue = wizard.scan_review_coverage_issue(slot);
    if let Some(issue) = coverage_issue {
        ui.colored_label(Color32::YELLOW, issue);
    }
    if ui
        .add_enabled(
            coverage_issue.is_none() && preview.is_some(),
            egui::Checkbox::new(&mut reviewed, "The overlay follows the physical cut edges"),
        )
        .changed()
        && reviewed
    {
        wizard.accept_scan_review(slot, now);
    }
}

fn render_scan_overlay(
    ui: &mut Ui,
    report: &ScanAnalysisReport,
    preview_png: &Arc<[u8]>,
    preview_sha1: &str,
    validation: bool,
    wizard: &CalibrationWizard,
    state: &mut CalibrationUiState,
) {
    let [source_width, source_height] = report.scan_dimensions_px;
    if source_width == 0 || source_height == 0 {
        return;
    }
    let maximum = egui::vec2(ui.available_width().clamp(1.0, 720.0), 480.0);
    let size = fitted_evidence_size(source_width, source_height, maximum);
    let slot = if validation {
        ScanSlot::Validation
    } else {
        ScanSlot::Training
    };
    let response = render_scan_overlay_image(
        ui,
        report,
        preview_png,
        preview_sha1,
        validation,
        wizard,
        size,
        true,
    );
    if response.clicked() {
        state.fullscreen_scan = Some(slot);
    }
    response
        .clone()
        .on_hover_text("Click to inspect this evidence full screen");
    ui.small("Overlay: yellow cross = intended center · green circle = included detection · orange = excluded/review");
    if ui
        .button(if validation {
            "View validation evidence full screen"
        } else {
            "View calibration evidence full screen"
        })
        .clicked()
    {
        state.fullscreen_scan = Some(slot);
    }
}

#[allow(clippy::too_many_arguments)]
fn render_scan_overlay_image(
    ui: &mut Ui,
    report: &ScanAnalysisReport,
    preview_png: &Arc<[u8]>,
    preview_sha1: &str,
    validation: bool,
    wizard: &CalibrationWizard,
    size: egui::Vec2,
    clickable: bool,
) -> egui::Response {
    let uri = format!(
        "bytes://calibration-scan-{}-{}.png",
        preview_sha1,
        if validation { "validation" } else { "training" }
    );
    let image = egui::Image::from_bytes(uri, preview_png.clone())
        .fit_to_exact_size(size)
        .alt_text(if validation {
            "Validation scan evidence"
        } else {
            "Calibration scan evidence"
        });
    let response = if clickable {
        let label = if validation {
            "Open full-screen validation scan evidence"
        } else {
            "Open full-screen calibration scan evidence"
        };
        let response = ui.add(egui::Button::new(image).frame(false));
        response.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), label)
        });
        response
    } else {
        ui.add(image)
    };
    let Some(print_to_scanner) = report.scanner_to_print.inverse() else {
        return response;
    };
    let [source_width, source_height] = report.scan_dimensions_px;
    let to_screen = |point_mm: [f64; 2]| {
        let point = print_to_scanner.apply(point_mm);
        egui::pos2(
            response.rect.left() + point[0] as f32 / source_width as f32 * response.rect.width(),
            response.rect.top() + point[1] as f32 / source_height as f32 * response.rect.height(),
        )
    };
    let observations = if validation {
        &wizard.validation_observations
    } else {
        &wizard.scan_training_observations
    };
    let painter = ui.painter();
    for target in &report.targets {
        let expected = to_screen(target.expected_center_mm);
        painter.line_segment(
            [
                expected - egui::vec2(4.0, 0.0),
                expected + egui::vec2(4.0, 0.0),
            ],
            egui::Stroke::new(1.0_f32, Color32::YELLOW),
        );
        painter.line_segment(
            [
                expected - egui::vec2(0.0, 4.0),
                expected + egui::vec2(0.0, 4.0),
            ],
            egui::Stroke::new(1.0_f32, Color32::YELLOW),
        );
        if let Some(observed_mm) = target.observed_center_mm {
            let observed = to_screen(observed_mm);
            let included = observations
                .iter()
                .find(|observation| observation.target_id == target.target_id)
                .is_some_and(|observation| observation.included);
            let color = if included {
                Color32::from_rgb(55, 220, 125)
            } else {
                Color32::from_rgb(245, 120, 65)
            };
            painter.line_segment([expected, observed], egui::Stroke::new(1.0_f32, color));
            painter.circle_stroke(observed, 6.0_f32, egui::Stroke::new(2.0_f32, color));
        }
    }
    response
}

fn render_fullscreen_scan(
    ctx: &egui::Context,
    wizard: &CalibrationWizard,
    state: &mut CalibrationUiState,
    diagnostics: CalibrationUiDiagnostics<'_>,
) {
    let Some(slot) = state.fullscreen_scan else {
        return;
    };
    let (report, preview_png, preview_sha1, validation) = match slot {
        ScanSlot::Training => (
            diagnostics.training_scan,
            diagnostics.training_scan_preview_png,
            diagnostics.training_scan_preview_sha1,
            false,
        ),
        ScanSlot::Validation => (
            diagnostics.validation_scan,
            diagnostics.validation_scan_preview_png,
            diagnostics.validation_scan_preview_sha1,
            true,
        ),
    };
    let (Some(report), Some(preview_png), Some(preview_sha1)) = (report, preview_png, preview_sha1)
    else {
        state.fullscreen_scan = None;
        return;
    };

    let mut close_requested = false;
    let modal = egui::Modal::new(egui::Id::new("calibration_evidence_fullscreen")).show(ctx, |ui| {
        let viewport = ctx.content_rect();
        let modal_size = (viewport.size() - egui::vec2(32.0, 32.0)).max(egui::Vec2::ZERO);
        ui.set_min_size(modal_size);
        ui.set_max_size(modal_size);
        ui.horizontal(|ui| {
            ui.strong(if validation {
                "Validation scan and detected cut edges"
            } else {
                "Calibration scan and detected cut edges"
            });
            ui.with_layout(
                egui::Layout::right_to_left(egui::Align::Center),
                |ui| {
                    if ui.button("Close full-screen evidence").clicked() {
                        close_requested = true;
                    }
                },
            );
        });
        ui.label("Yellow crosses are intended centers; circles show the detected physical cut edges.");

        let [source_width, source_height] = report.scan_dimensions_px;
        let available = ui.available_size() - egui::vec2(0.0, 8.0);
        let image_size = fitted_evidence_size(source_width, source_height, available);
        render_scan_overlay_image(
            ui,
            report,
            preview_png,
            preview_sha1,
            validation,
            wizard,
            image_size,
            false,
        );
    });
    if modal.should_close() || close_requested {
        state.fullscreen_scan = None;
    }
}

fn render_discard_confirmation(
    ctx: &egui::Context,
    wizard: &mut CalibrationWizard,
    state: &mut CalibrationUiState,
    now: u64,
    events: &mut Vec<CalibrationUiEvent>,
) {
    let mut keep_calibrating = false;
    let modal = egui::Modal::new(egui::Id::new("leave_calibration_confirmation")).show(ctx, |ui| {
        ui.set_width(390.0);
        ui.heading("Leave calibration?");
        ui.label(
            "Save the current progress so this calibration can be resumed, or discard the run.",
        );
        ui.add_space(8.0);
        ui.horizontal_wrapped(|ui| {
            if ui.button("Keep calibrating").clicked() {
                keep_calibrating = true;
            }
            if ui.button("Save progress and exit").clicked() {
                state.open = false;
                state.confirm_discard = false;
                events.push(CalibrationUiEvent::SaveAndExit);
                ui.close();
            }
            if ui.button("Discard run").clicked() {
                wizard.discard(now);
                state.open = false;
                state.confirm_discard = false;
                events.push(CalibrationUiEvent::Discard);
                ui.close();
            }
        });
    });
    if keep_calibrating || (modal.should_close() && state.open) {
        state.confirm_discard = false;
    }
}

fn fitted_evidence_size(
    source_width: u32,
    source_height: u32,
    available: egui::Vec2,
) -> egui::Vec2 {
    if source_width == 0 || source_height == 0 || available.x <= 0.0 || available.y <= 0.0 {
        return egui::Vec2::ZERO;
    }
    let scale = (available.x / source_width as f32)
        .min(available.y / source_height as f32)
        .max(0.0);
    egui::vec2(source_width as f32 * scale, source_height as f32 * scale)
}

fn render_print_area(
    ui: &mut Ui,
    wizard: &mut CalibrationWizard,
    state: &mut CalibrationUiState,
    now: u64,
) {
    ui.heading("Optional: record the printable area");
    ui.label("Measure the unprinted inset from each sheet edge to the first printable boundary. Leave a field blank if it cannot be measured reliably.");
    ui.add_space(8.0);
    let labels = ["Top", "Right", "Bottom", "Left"];
    for (index, label) in labels.into_iter().enumerate() {
        numeric_text_field(
            ui,
            &mut state.text_fields,
            &format!("print-area-{index}"),
            wizard.printability_insets_mm[index],
            &format!("{label} inset (mm)"),
        );
    }
    ui.horizontal_wrapped(|ui| {
        if ui.button("Save print-area measurements").clicked() {
            let parsed = std::array::from_fn(|index| {
                parse_optional_number(state.text_fields.get(&format!("print-area-{index}")))
            });
            match collect_parsed_array(parsed) {
                Ok(insets) => set_result(state, wizard.mark_print_area_reviewed(insets, now)),
                Err(message) => state.last_error = Some(message),
            }
        }
        if ui.button("Skip this optional step").clicked() {
            set_result(state, wizard.mark_print_area_reviewed([None; 4], now));
        }
    });
}

fn render_print_scale(
    ui: &mut Ui,
    wizard: &mut CalibrationWizard,
    state: &mut CalibrationUiState,
    slot: ManualSheetSlot,
    now: u64,
) {
    let print_scale = manual_sheet(wizard, slot).print_scale.clone();
    ui.heading("Measure the printed scale bars");
    ui.label("Measure the H80 bar horizontally and the V150 bar vertically. This separates printer scaling from cutter registration.");
    ui.add_space(8.0);
    let prefix = slot_key(slot);
    numeric_text_field(
        ui,
        &mut state.text_fields,
        &format!("{prefix}-scale-h"),
        print_scale.horizontal_mm,
        "H80 measured length (mm)",
    );
    numeric_text_field(
        ui,
        &mut state.text_fields,
        &format!("{prefix}-scale-v"),
        print_scale.vertical_mm,
        "V150 measured length (mm)",
    );
    ui.horizontal_wrapped(|ui| {
        if ui.button("Use measurements").clicked() {
            let horizontal =
                parse_required_number(state.text_fields.get(&format!("{prefix}-scale-h")));
            let vertical =
                parse_required_number(state.text_fields.get(&format!("{prefix}-scale-v")));
            match (horizontal, vertical) {
                (Ok(horizontal), Ok(vertical)) => {
                    set_result(
                        state,
                        wizard.set_print_scale(slot, Some([horizontal, vertical]), now),
                    );
                }
                _ => state.last_error = Some("Enter valid positive H80 and V150 lengths.".into()),
            }
        }
        if ui.button("Skip scale measurement").clicked() {
            set_result(state, wizard.set_print_scale(slot, None, now));
        }
    });
    if print_scale.state == OptionalMeasurementState::Skipped {
        ui.weak("Print scale will be treated as 1:1.");
    } else if let Some([horizontal, vertical]) = print_scale.ratios() {
        ui.label(format!(
            "Derived print ratios: horizontal {:.6} · vertical {:.6}",
            horizontal, vertical
        ));
    }
}

fn render_manual_targets(
    ui: &mut Ui,
    wizard: &mut CalibrationWizard,
    state: &mut CalibrationUiState,
    slot: ManualSheetSlot,
    now: u64,
) {
    let sheet = manual_sheet(wizard, slot).clone();
    let minimum = if slot == ManualSheetSlot::Second {
        6
    } else {
        4
    };
    ui.heading("Measure from the printed center cross to each physical cut edge");
    ui.label("For each target, place zero at the printed center cross and measure outward to the actual cut edge: Left, Right, Top, and Bottom. Do not measure between the printed box outline and the cut.");
    ui.label("Expected center-to-edge distance in every direction: 7.00 mm. Sapodilla calculates the signed center offset for you.");
    ui.label(format!(
        "Measurement uncertainty used by the fit: {:.2} mm",
        wizard.manual_uncertainty_mm
    ));
    let display_scale = if slot == ManualSheetSlot::Validation {
        let ratios = wizard.manual_primary.print_scale.ratios();
        if let Some([horizontal, vertical]) = ratios {
            ui.label(format!(
                "This validation sheet has no scale bars. Its offsets inherit the primary training sheet’s print ratios: horizontal {horizontal:.6}, vertical {vertical:.6}."
            ));
        }
        ratios
    } else {
        sheet.print_scale.ratios()
    };
    ui.label(format!(
        "Accepted: {} · minimum required: {minimum}",
        sheet.accepted_count()
    ));
    if slot != ManualSheetSlot::Second
        && let Some(reason) = wizard.manual_translation_coverage_issue(slot)
    {
        ui.colored_label(Color32::YELLOW, format!("Still needed: {reason}."));
    }
    ui.add_space(6.0);

    for target in &sheet.targets {
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.strong(&target.target_id);
                ui.weak(format!(
                    "at {:.0}, {:.0} mm",
                    target.nominal_print_mm[0], target.nominal_print_mm[1]
                ));
                if target.is_accepted() {
                    ui.colored_label(Color32::from_rgb(60, 155, 95), "Accepted");
                } else if target.skipped_damaged {
                    ui.weak("Skipped");
                }
            });
            if let Some(scale) = display_scale
                && let Some(derived) = target
                    .edges
                    .complete_measurement()
                    .and_then(|measurement| measurement.derive(scale))
            {
                ui.small(format!(
                    "Derived center offset: X {:+.3} mm · Y {:+.3} mm; cut size {:.2} × {:.2} mm",
                    derived.displacement_mm[0],
                    derived.displacement_mm[1],
                    derived.cut_size_mm[0],
                    derived.cut_size_mm[1]
                ));
            }
            let values = [
                target.edges.left_mm,
                target.edges.right_mm,
                target.edges.top_mm,
                target.edges.bottom_mm,
            ];
            let names = ["Left", "Right", "Top", "Bottom"];
            ui.horizontal_wrapped(|ui| {
                for (index, name) in names.into_iter().enumerate() {
                    numeric_text_field(
                        ui,
                        &mut state.text_fields,
                        &format!("{}-{}-{index}", slot_key(slot), target.target_id),
                        values[index],
                        name,
                    );
                }
            });
            ui.horizontal_wrapped(|ui| {
                if ui.button("Save target").clicked() {
                    let parsed = std::array::from_fn(|index| {
                        parse_required_number(state.text_fields.get(&format!(
                            "{}-{}-{index}",
                            slot_key(slot),
                            target.target_id
                        )))
                    });
                    match collect_required_array(parsed) {
                        Ok([left, right, top, bottom]) => set_result(
                            state,
                            wizard.set_manual_target(
                                slot,
                                &target.target_id,
                                ManualEdgeDraft {
                                    left_mm: Some(left),
                                    right_mm: Some(right),
                                    top_mm: Some(top),
                                    bottom_mm: Some(bottom),
                                },
                                false,
                                now,
                            ),
                        ),
                        Err(message) => state.last_error = Some(message),
                    }
                }
                if ui.button("Skip damaged target").clicked() {
                    set_result(
                        state,
                        wizard.set_manual_target(
                            slot,
                            &target.target_id,
                            target.edges.clone(),
                            true,
                            now,
                        ),
                    );
                }
            });
        });
        ui.add_space(5.0);
    }
}

fn render_second_sheet_choice(ui: &mut Ui, wizard: &mut CalibrationWizard, now: u64) {
    ui.heading("Use one sheet or improve confidence?");
    ui.label(format!(
        "The first sheet has {} accepted targets.",
        wizard.manual_primary.accepted_count()
    ));
    ui.label("A separately loaded second sheet helps average loading variation. It requires at least six accepted targets on each sheet.");
    ui.add_space(8.0);
    let mut choice = wizard.second_sheet_choice;
    if ui
        .radio_value(
            &mut choice,
            Some(SecondSheetChoice::ContinueWithOneSheet),
            "Continue with this sheet",
        )
        .changed()
    {
        wizard.choose_second_sheet(SecondSheetChoice::ContinueWithOneSheet, now);
    }
    if ui
        .radio_value(
            &mut choice,
            Some(SecondSheetChoice::MeasureAnotherSheet),
            "Print and measure another sheet",
        )
        .changed()
    {
        wizard.choose_second_sheet(SecondSheetChoice::MeasureAnotherSheet, now);
    }
}

fn render_candidate(
    ui: &mut Ui,
    wizard: &mut CalibrationWizard,
    now: u64,
    solution: Option<&CalibrationSolution>,
    events: &mut Vec<CalibrationUiEvent>,
) {
    ui.heading("Fit a candidate correction");
    ui.label(format!(
        "{} training observations are available.",
        wizard.training_observations().len()
    ));
    match &wizard.candidate {
        CandidateStatus::NotStarted => ui.weak("The candidate has not been computed."),
        CandidateStatus::Computing => {
            ui.spinner();
            ui.label("Computing candidate…")
        }
        CandidateStatus::Ready { selected_model } => ui.colored_label(
            Color32::from_rgb(60, 155, 95),
            format!("Ready · {selected_model}"),
        ),
        CandidateStatus::Failed { message } => {
            ui.colored_label(Color32::from_rgb(210, 70, 70), message)
        }
    };
    if let Some(solution) = solution {
        let selected = &solution.selected;
        ui.separator();
        ui.label(format!(
            "Training RMS {:.3} mm · p95 {:.3} mm · max {:.3} mm",
            selected.training_metrics.rms_mm,
            selected.training_metrics.p95_mm,
            selected.training_metrics.maximum_mm
        ));
        ui.label(format!(
            "Leave-one-out RMS {:.3} mm · p95 {:.3} mm · condition {:.1}",
            selected.leave_one_out_metrics.rms_mm,
            selected.leave_one_out_metrics.p95_mm,
            selected.condition
        ));
        let diagnosis = match selected.model {
            crate::calibration::CalibrationModel::Translation => {
                "The measured error is predominantly a consistent X/Y offset."
            }
            crate::calibration::CalibrationModel::IndependentAxisScale => {
                "The measurements support separate feed- and cross-axis scale correction."
            }
            crate::calibration::CalibrationModel::Affine => {
                "Two independent sheets support a position-dependent affine correction."
            }
        };
        ui.label(diagnosis);
        egui::CollapsingHeader::new("Residual details").show(ui, |ui| {
            egui::Grid::new("candidate_residuals")
                .striped(true)
                .show(ui, |ui| {
                    ui.strong("Target");
                    ui.strong("Sheet");
                    ui.strong("X / Y (mm)");
                    ui.strong("Distance");
                    ui.end_row();
                    for residual in &selected.residuals {
                        ui.label(&residual.target_id);
                        ui.label(&residual.sheet_id);
                        ui.label(format!(
                            "{:+.3} / {:+.3}",
                            residual.xy_mm[0], residual.xy_mm[1]
                        ));
                        ui.label(format!("{:.3}", residual.distance_mm));
                        ui.end_row();
                    }
                });
        });
    }
    let compute_label = match wizard.candidate {
        CandidateStatus::NotStarted => "Compute candidate",
        CandidateStatus::Computing => "Restart computation",
        CandidateStatus::Ready { .. } | CandidateStatus::Failed { .. } => "Recompute candidate",
    };
    if ui.button(compute_label).clicked() {
        wizard.mark_candidate_computing(now);
        events.push(CalibrationUiEvent::ComputeCandidate);
    }
}

#[allow(clippy::too_many_arguments)]
fn render_validation_review(
    ui: &mut Ui,
    wizard: &mut CalibrationWizard,
    now: u64,
    report: Option<&ScanAnalysisReport>,
    preview_png: Option<&Arc<[u8]>>,
    preview_sha1: Option<&str>,
    metrics: Option<&ValidationMetrics>,
    state: &mut CalibrationUiState,
    events: &mut Vec<CalibrationUiEvent>,
) {
    ui.heading("Review validation result");
    if wizard.method == Some(CalibrationMethod::FlatbedScanner) {
        render_scan_review(
            ui,
            wizard,
            state,
            true,
            now,
            report,
            preview_png.zip(preview_sha1),
        );
    }
    match &wizard.validation {
        ValidationStatus::NotStarted | ValidationStatus::Collecting => {
            ui.weak("Waiting for validation evaluation…");
            if wizard.validation_scan_reviewed
                && ui.button("Evaluate included validation targets").clicked()
            {
                events.push(CalibrationUiEvent::EvaluateValidation);
            }
        }
        ValidationStatus::Evaluating => {
            ui.spinner();
            ui.label("Evaluating the held-out validation sheet…");
        }
        ValidationStatus::Passed => {
            ui.colored_label(Color32::from_rgb(60, 155, 95), "Validation passed");
        }
        ValidationStatus::Failed { message } => {
            ui.colored_label(
                Color32::from_rgb(210, 70, 70),
                format!("Validation failed: {message}"),
            );
            ui.label(if wizard.method == Some(CalibrationMethod::FlatbedScanner) {
                "Re-import the validation scan after fixing scan setup or removed centers. If the scan is sound, use Back again to review the training scan and candidate."
            } else {
                "Recheck and re-enter the validation measurements. If they are sound, use Back again to review the training measurements and candidate."
            });
            ui.horizontal_wrapped(|ui| {
                let retry_label = if wizard.method == Some(CalibrationMethod::FlatbedScanner) {
                    "Re-import validation scan"
                } else {
                    "Re-measure validation targets"
                };
                if ui.button(retry_label).clicked() {
                    match wizard.back(now) {
                        Ok(_) => state.last_error = None,
                        Err(error) => state.last_error = Some(error.to_string()),
                    }
                }
                if ui.button("Keep current profile and exit").clicked() {
                    state.open = false;
                    events.push(CalibrationUiEvent::SaveAndExit);
                }
            });
        }
    }
    if let Some(metrics) = metrics
        && !matches!(
            wizard.validation,
            ValidationStatus::NotStarted | ValidationStatus::Collecting
        )
    {
        ui.label(format!(
            "RMS {:.3} → {:.3} mm · p95 {:.3} → {:.3} mm · max {:.3} → {:.3} mm",
            metrics.before.rms_mm,
            metrics.after.rms_mm,
            metrics.before.p95_mm,
            metrics.after.p95_mm,
            metrics.before.maximum_mm,
            metrics.after.maximum_mm
        ));
    }
}

fn render_kiss_cut_inspection(ui: &mut Ui, wizard: &mut CalibrationWizard, now: u64) {
    ui.heading("Check production-style kiss cuts");
    ui.label("Inspect the three normal kiss-cut shapes on the validation sheet. Their cut edges should follow the printed design consistently.");
    let mut result = wizard.normal_kiss_cut_passed;
    if ui
        .radio_value(&mut result, Some(true), "Yes — the kiss-cut edges align")
        .changed()
    {
        wizard.set_kiss_cut_inspection(true, now);
    }
    if ui
        .radio_value(
            &mut result,
            Some(false),
            "No — alignment is still unacceptable",
        )
        .changed()
    {
        wizard.set_kiss_cut_inspection(false, now);
    }
}

fn render_finish(
    ui: &mut Ui,
    wizard: &CalibrationWizard,
    metrics: Option<&ValidationMetrics>,
    events: &mut Vec<CalibrationUiEvent>,
) {
    ui.heading("Calibration is ready");
    ui.label("The candidate passed the independent validation sheet and can now become the active print-to-cut profile.");
    if let Some(metrics) = metrics {
        ui.label(format!(
            "Validated RMS: {:.3} → {:.3} mm; p95 after: {:.3} mm",
            metrics.before.rms_mm, metrics.after.rms_mm, metrics.after.p95_mm
        ));
    }
    if wizard.method == Some(CalibrationMethod::ManualEastBay) {
        ui.add_space(8.0);
        ui.label(EAST_BAY_RESULTS_CREDIT);
        ui.hyperlink_to("Open method source", EAST_BAY_SOURCE_URL);
    }
    ui.add_space(12.0);
    if ui
        .button(RichText::new("Activate calibration profile").strong())
        .clicked()
    {
        events.push(CalibrationUiEvent::ActivateProfile);
    }
}

fn render_footer(
    ui: &mut Ui,
    wizard: &mut CalibrationWizard,
    state: &mut CalibrationUiState,
    now: u64,
    events: &mut Vec<CalibrationUiEvent>,
) {
    if let Some(error) = &state.last_error {
        ui.colored_label(Color32::from_rgb(210, 70, 70), error);
    }

    ui.horizontal_wrapped(|ui| {
        if ui
            .add_enabled(!wizard.history.is_empty(), egui::Button::new("Back"))
            .clicked()
        {
            match wizard.back(now) {
                Ok(_) => state.last_error = None,
                Err(error) => state.last_error = Some(error.to_string()),
            }
        }

        if wizard.step != WizardStep::Finish {
            let next = next_availability(wizard, now);
            let response = ui.add_enabled(next.is_ok(), egui::Button::new("Next"));
            if response.clicked() {
                match wizard.next(now) {
                    Ok(step) => {
                        state.last_error = None;
                        if step == WizardStep::Candidate
                            && matches!(wizard.candidate, CandidateStatus::Computing)
                        {
                            events.push(CalibrationUiEvent::ComputeCandidate);
                        }
                        if step == WizardStep::ReviewValidation {
                            events.push(CalibrationUiEvent::EvaluateValidation);
                        }
                    }
                    Err(error) => state.last_error = Some(error.to_string()),
                }
            }
            if let Err(reason) = next {
                response.on_hover_text(reason);
            }
        }

        let skip_label = wizard.step.skip_label();
        let primary_job_active = wizard.step == WizardStep::PrintCalibration
            && matches!(
                wizard.primary_job,
                JobStatus::Queued | JobStatus::InProgress
            );
        let skip_button_label = if wizard.step == WizardStep::PrintCalibration {
            "Use existing sheet"
        } else {
            "Skip step"
        };
        let skip_response = ui.add_enabled(
            skip_label.is_some() && !primary_job_active,
            egui::Button::new(skip_button_label),
        );
        if skip_response.clicked() {
            if wizard.step == WizardStep::PrintCalibration {
                events.push(CalibrationUiEvent::UseExistingPrimarySheet);
            } else {
                match wizard.skip_current_step(now) {
                    Ok(step) => {
                        state.last_error = None;
                        if step == WizardStep::Candidate
                            && matches!(wizard.candidate, CandidateStatus::Computing)
                        {
                            events.push(CalibrationUiEvent::ComputeCandidate);
                        }
                    }
                    Err(error) => state.last_error = Some(error.to_string()),
                }
            }
        }
        if primary_job_active {
            skip_response.on_hover_text("Wait for or cancel the active print job first");
        } else if let Some(label) = skip_label {
            skip_response.on_hover_text(label);
        } else {
            skip_response.on_hover_text("This calibration step is required");
        }

        if ui.button("Save & Exit").clicked() {
            state.open = false;
            events.push(CalibrationUiEvent::SaveAndExit);
        }
        if ui.button("Discard…").clicked() {
            state.confirm_discard = true;
        }
    });
}

fn calibration_window_height_limits(available_height: f32) -> (f32, f32) {
    let max_height =
        (available_height - CALIBRATION_WINDOW_MARGIN).clamp(1.0, CALIBRATION_WINDOW_MAX_HEIGHT);
    (460.0_f32.min(max_height), max_height)
}

/// Returns `Ok` when the wizard's real transition guard currently permits
/// Next. This uses a clone so rendering never speculates by mutating the draft.
fn next_availability(wizard: &CalibrationWizard, now: u64) -> Result<(), String> {
    let mut probe = wizard.clone();
    probe
        .next(now)
        .map(|_| ())
        .map_err(|error| error.to_string())
}

fn numeric_text_field(
    ui: &mut Ui,
    fields: &mut BTreeMap<String, String>,
    key: &str,
    stored: Option<f64>,
    label: &str,
) {
    let value = fields.entry(key.to_owned()).or_insert_with(|| {
        stored
            .map(|number| format!("{number:.2}"))
            .unwrap_or_default()
    });
    ui.horizontal(|ui| {
        ui.label(label);
        ui.add(
            egui::TextEdit::singleline(value)
                .desired_width(70.0)
                .hint_text("mm"),
        );
    });
}

fn parse_optional_number(value: Option<&String>) -> Result<Option<f64>, String> {
    let text = value.map(String::as_str).unwrap_or_default().trim();
    if text.is_empty() {
        return Ok(None);
    }
    let number = text
        .parse::<f64>()
        .map_err(|_| format!("‘{text}’ is not a valid millimetre measurement."))?;
    if !number.is_finite() || number < 0.0 {
        return Err("Measurements must be finite and non-negative.".into());
    }
    Ok(Some(number))
}

fn parse_required_number(value: Option<&String>) -> Result<f64, String> {
    parse_optional_number(value)?.ok_or_else(|| "Complete all required measurements.".into())
}

fn collect_parsed_array(
    values: [Result<Option<f64>, String>; 4],
) -> Result<[Option<f64>; 4], String> {
    let [a, b, c, d] = values;
    Ok([a?, b?, c?, d?])
}

fn collect_required_array(values: [Result<f64, String>; 4]) -> Result<[f64; 4], String> {
    let [a, b, c, d] = values;
    Ok([a?, b?, c?, d?])
}

fn set_result<T, E: ToString>(state: &mut CalibrationUiState, result: Result<T, E>) {
    state.last_error = result.err().map(|error| error.to_string());
}

fn clear_slot_text_fields(state: &mut CalibrationUiState, slot: JobSlot) {
    state.text_fields.retain(|key, _| match slot {
        JobSlot::Primary => {
            !key.starts_with("primary-")
                && !key.starts_with("second-")
                && !key.starts_with("validation-")
                && !key.starts_with("print-area-")
        }
        JobSlot::Second => !key.starts_with("second-") && !key.starts_with("validation-"),
        JobSlot::Validation => !key.starts_with("validation-"),
    });
}

fn manual_sheet(wizard: &CalibrationWizard, slot: ManualSheetSlot) -> &ManualSheetDraft {
    match slot {
        ManualSheetSlot::Primary => &wizard.manual_primary,
        ManualSheetSlot::Second => &wizard.manual_second,
        ManualSheetSlot::Validation => &wizard.manual_validation,
    }
}

fn slot_key(slot: ManualSheetSlot) -> &'static str {
    match slot {
        ManualSheetSlot::Primary => "primary",
        ManualSheetSlot::Second => "second",
        ManualSheetSlot::Validation => "validation",
    }
}

fn job_status_label(status: JobStatus) -> &'static str {
    match status {
        JobStatus::NotStarted => "not started",
        JobStatus::Queued => "queued",
        JobStatus::InProgress => "in progress",
        JobStatus::Completed => "completed",
        JobStatus::ExistingSheet => "using an existing sheet",
        JobStatus::Failed => "failed",
    }
}

fn scan_target_status_label(status: ScanTargetStatus) -> &'static str {
    match status {
        ScanTargetStatus::Accepted => "Accepted",
        ScanTargetStatus::Review(ScanFailureReason::RetainedSlug) => "Review — center may remain",
        ScanTargetStatus::Review(ScanFailureReason::LowContrast) => "Review — low contrast",
        ScanTargetStatus::Review(ScanFailureReason::InsufficientBoundary) => {
            "Review — incomplete edge"
        }
        ScanTargetStatus::Review(ScanFailureReason::ExcessiveCircleResidual) => {
            "Review — irregular edge"
        }
        ScanTargetStatus::Review(ScanFailureReason::ImplausibleRadius) => {
            "Review — unexpected size"
        }
        ScanTargetStatus::Review(ScanFailureReason::RegionOutsideScan) => "Review — outside scan",
        ScanTargetStatus::Missing(ScanFailureReason::RetainedSlug) => "Missing — center retained",
        ScanTargetStatus::Missing(ScanFailureReason::LowContrast) => "Missing — low contrast",
        ScanTargetStatus::Missing(ScanFailureReason::InsufficientBoundary) => {
            "Missing — incomplete edge"
        }
        ScanTargetStatus::Missing(ScanFailureReason::ExcessiveCircleResidual) => {
            "Missing — irregular edge"
        }
        ScanTargetStatus::Missing(ScanFailureReason::ImplausibleRadius) => {
            "Missing — unexpected size"
        }
        ScanTargetStatus::Missing(ScanFailureReason::RegionOutsideScan) => "Missing — outside scan",
    }
}

fn scan_failure_remediation(reason: ScanFailureReason) -> &'static str {
    match reason {
        ScanFailureReason::RetainedSlug => {
            "Center retained: remove the complete center, including its liner, then rescan."
        }
        ScanFailureReason::LowContrast => {
            "Low contrast: use clean opaque white backing directly behind the liner and disable scanner cleanup."
        }
        ScanFailureReason::InsufficientBoundary => {
            "Incomplete edge: flatten the sheet with light lid pressure and check for torn or partly removed centers."
        }
        ScanFailureReason::ExcessiveCircleResidual => {
            "Irregular edge: inspect for tears, shadows, debris, or a center that was not fully removed."
        }
        ScanFailureReason::ImplausibleRadius => {
            "Unexpected size: confirm this is the matching calibration sheet and that scanner scaling is disabled."
        }
        ScanFailureReason::RegionOutsideScan => {
            "Outside scan: disable automatic cropping and include all four edges of the sheet."
        }
    }
}

fn step_summary(step: WizardStep, method: Option<CalibrationMethod>) -> &'static str {
    match step {
        WizardStep::ChooseMethod => {
            "Two guided workflows cover scanner-assisted and measurement-only calibration."
        }
        WizardStep::Prepare => {
            "Keep the calibration run identical to the production print-and-cut path."
        }
        WizardStep::PrintCalibration | WizardStep::PrintSecondCalibration => {
            "Print and cut the generated target sheet without scaling."
        }
        WizardStep::RemoveCenters | WizardStep::RemoveValidationCenters => {
            "Removed through-cut centers reveal a clean white background for strong cut-edge contrast."
        }
        WizardStep::ImportScan | WizardStep::ImportValidationScan => {
            "Import an uncorrected, full-sheet PNG from the flatbed scanner."
        }
        WizardStep::ReviewScan => "Confirm automatic localization before fitting a correction.",
        WizardStep::PrintArea => "Optionally record where printing begins near each sheet edge.",
        WizardStep::PrintScale | WizardStep::SecondPrintScale => {
            "Printed scale bars reveal printer-side stretch or shrink."
        }
        WizardStep::ManualTargets
        | WizardStep::SecondManualTargets
        | WizardStep::ValidationManualTargets => {
            "Four edge-gap measurements locate each physical cut center."
        }
        WizardStep::SecondSheetChoice => {
            "A second independent load improves confidence but is not required."
        }
        WizardStep::Candidate => "Fit the smallest correction model supported by the observations.",
        WizardStep::PrintValidation => {
            "Test the candidate on fresh output that is excluded from fitting."
        }
        WizardStep::ReviewValidation => {
            "The held-out sheet decides whether the profile can be activated."
        }
        WizardStep::KissCutInspection => {
            "Verify that aperture accuracy also carries over to normal kiss cuts."
        }
        WizardStep::Finish if method == Some(CalibrationMethod::ManualEastBay) => {
            "Review and activate the validated manual calibration profile."
        }
        WizardStep::Finish => "Review and activate the validated scanner calibration profile.",
    }
}

fn journey_steps(method: Option<CalibrationMethod>) -> &'static [WizardStep] {
    const CHOOSE: &[WizardStep] = &[WizardStep::ChooseMethod];
    const FLATBED: &[WizardStep] = &[
        WizardStep::ChooseMethod,
        WizardStep::Prepare,
        WizardStep::PrintCalibration,
        WizardStep::RemoveCenters,
        WizardStep::ImportScan,
        WizardStep::ReviewScan,
        WizardStep::Candidate,
        WizardStep::PrintValidation,
        WizardStep::RemoveValidationCenters,
        WizardStep::ImportValidationScan,
        WizardStep::ReviewValidation,
        WizardStep::KissCutInspection,
        WizardStep::Finish,
    ];
    const MANUAL: &[WizardStep] = &[
        WizardStep::ChooseMethod,
        WizardStep::Prepare,
        WizardStep::PrintCalibration,
        WizardStep::PrintArea,
        WizardStep::PrintScale,
        WizardStep::ManualTargets,
        WizardStep::SecondSheetChoice,
        WizardStep::PrintSecondCalibration,
        WizardStep::SecondPrintScale,
        WizardStep::SecondManualTargets,
        WizardStep::Candidate,
        WizardStep::PrintValidation,
        WizardStep::ValidationManualTargets,
        WizardStep::ReviewValidation,
        WizardStep::Finish,
    ];
    match method {
        Some(CalibrationMethod::FlatbedScanner) => FLATBED,
        Some(CalibrationMethod::ManualEastBay) => MANUAL,
        None => CHOOSE,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measurement_parser_accepts_blank_optional_and_rejects_bad_values() {
        assert_eq!(parse_optional_number(None), Ok(None));
        assert_eq!(
            parse_optional_number(Some(&" 7.25 ".into())),
            Ok(Some(7.25))
        );
        assert!(parse_optional_number(Some(&"-1".into())).is_err());
        assert!(parse_optional_number(Some(&"not a number".into())).is_err());
    }

    #[test]
    fn progress_paths_are_method_specific() {
        assert!(
            journey_steps(Some(CalibrationMethod::FlatbedScanner))
                .contains(&WizardStep::ImportScan)
        );
        assert!(
            !journey_steps(Some(CalibrationMethod::FlatbedScanner))
                .contains(&WizardStep::ManualTargets)
        );
        assert!(
            journey_steps(Some(CalibrationMethod::ManualEastBay))
                .contains(&WizardStep::ManualTargets)
        );
        assert!(
            !journey_steps(Some(CalibrationMethod::ManualEastBay))
                .contains(&WizardStep::ImportScan)
        );
    }

    #[test]
    fn next_availability_uses_real_wizard_guards() {
        let mut wizard = CalibrationWizard::new("ui-guard", 0).unwrap();
        assert!(next_availability(&wizard, 1).is_err());
        wizard
            .select_method(CalibrationMethod::FlatbedScanner, 1)
            .unwrap();
        assert!(next_availability(&wizard, 2).is_ok());
        wizard.next(2).unwrap();
        assert!(next_availability(&wizard, 3).is_err());
    }

    #[test]
    fn calibration_window_height_is_bounded_by_viewport_and_design_cap() {
        assert_eq!(calibration_window_height_limits(400.0), (376.0, 376.0));
        assert_eq!(calibration_window_height_limits(900.0), (460.0, 760.0));
    }

    #[test]
    fn evidence_size_preserves_source_aspect_ratio_and_fits_viewport() {
        let portrait = fitted_evidence_size(5100, 7012, egui::vec2(720.0, 480.0));
        assert!(portrait.x <= 720.0 && portrait.y <= 480.0);
        assert!((portrait.x / portrait.y - 5100.0 / 7012.0).abs() < 0.0001);

        let landscape = fitted_evidence_size(7012, 5100, egui::vec2(320.0, 700.0));
        assert!(landscape.x <= 320.0 && landscape.y <= 700.0);
        assert!((landscape.x / landscape.y - 7012.0 / 5100.0).abs() < 0.0001);
        assert_eq!(
            fitted_evidence_size(0, 5100, egui::vec2(320.0, 700.0)),
            egui::Vec2::ZERO
        );
    }

    #[test]
    fn reprint_clears_transient_measurement_text_for_affected_slots() {
        let mut state = CalibrationUiState::default();
        for key in [
            "print-area-0",
            "primary-C1-0",
            "second-C1-0",
            "validation-V1-0",
            "unrelated",
        ] {
            state.text_fields.insert(key.into(), "7".into());
        }
        clear_slot_text_fields(&mut state, JobSlot::Second);
        assert!(state.text_fields.contains_key("primary-C1-0"));
        assert!(!state.text_fields.contains_key("second-C1-0"));
        assert!(!state.text_fields.contains_key("validation-V1-0"));

        clear_slot_text_fields(&mut state, JobSlot::Primary);
        assert!(!state.text_fields.contains_key("print-area-0"));
        assert!(!state.text_fields.contains_key("primary-C1-0"));
        assert!(state.text_fields.contains_key("unrelated"));
    }

    #[test]
    fn scan_failures_have_plain_language_labels_and_recovery() {
        assert_eq!(
            scan_target_status_label(ScanTargetStatus::Missing(ScanFailureReason::RetainedSlug)),
            "Missing — center retained"
        );
        assert!(
            scan_failure_remediation(ScanFailureReason::RegionOutsideScan)
                .contains("automatic cropping")
        );
    }
}
