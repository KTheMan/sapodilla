use super::*;

use egui_kittest::{
    Harness,
    kittest::{NodeT as _, Queryable as _},
};

fn app_harness(size: Vec2) -> Harness<'static, SapodillaApp> {
    Harness::builder()
        .with_size(size)
        .build_eframe(|cc| SapodillaApp::new(cc))
}

fn open_calibration_fixture(harness: &mut Harness<'_, SapodillaApp>) {
    let wizard = CalibrationWizard::new("ui-calibration", 1).unwrap();
    harness.state_mut().calibration_session = Some(CalibrationSession {
        printer_id: "printer-a".into(),
        printer_key: PrinterCalibrationKey {
            identity: StablePrinterIdentity::SerialNumber {
                serial_number: "SERIAL-A".into(),
            },
            model: "DHP700".into(),
            firmware_revision: "1.0".into(),
            media_size: 5013,
            media_type: 2030,
        },
        wizard,
        baseline_profile_id: None,
        baseline_profile_version: default_calibration_profile_version(),
        baseline_mapping: CanvasToPlotter::legacy_pixcut_s1(2100.0),
        material: MaterialProfile::built_ins().remove(0),
        candidate: None,
        candidate_mapping: None,
        validation_metrics: None,
        training_scan_report: None,
        validation_scan_report: None,
        training_scan_preview_png: None,
        validation_scan_preview_png: None,
        training_scan_preview_sha1: None,
        validation_scan_preview_sha1: None,
        primary_queue_job: None,
        second_queue_job: None,
        validation_queue_job: None,
        historical_queue_job_ids: [None; 3],
        image_sha1: [None, None, None],
        plotter_sha1: [None, None, None],
        plotter_commands: std::array::from_fn(|_| Vec::new()),
        validation_generation: 0,
        device_job_ids: Vec::new(),
        validation_device_job_ids: Vec::new(),
        device_job_ids_by_slot: std::array::from_fn(|_| Vec::new()),
        physical_sheet_attempts: [0; 3],
        scan_request_generations: [0; 2],
    });
    harness.state_mut().calibration_ui_state = calibration_ui::CalibrationUiState::default();
    harness.run();
}

#[test]
fn current_print_route_error_releases_job_and_retains_retry_payload() {
    let mut harness = app_harness(Vec2::new(900.0, 700.0));
    let state = harness.state_mut();
    state
        .job_queue
        .add_printer(QueuePrinter::new("printer-a", "PixCut").with_capabilities(["print", "cut"]))
        .unwrap();
    let job_id = state.job_queue.enqueue(
        JobSpec::named("validation")
            .requiring(["print", "cut"])
            .restricted_to(["printer-a"]),
    );
    assert_eq!(state.job_queue.route_next().unwrap().job_id, job_id);
    state.active_queue_job = Some(job_id);
    state.active_queue_jobs.insert("printer-a".into(), job_id);
    state.send_progress = Some(0.5);
    state.pending_print_jobs.insert(
        job_id,
        build_calibration_print_job(
            ManifestIdentity::stock("route-error"),
            CalibrationMethod::FlatbedScanner,
            CalibrationJobSlot::Validation,
            MaterialProfile::default(),
            Some(CanvasToPlotter::legacy_pixcut_s1(2100.0)),
        )
        .unwrap(),
    );

    state
        .tx
        .send(Action::PrintRouteEncoded {
            printer_id: "printer-a".into(),
            job_id,
            result: Err(anyhow::anyhow!("worker validation failed")),
        })
        .unwrap();
    state.apply_actions();

    assert_eq!(
        state.job_queue.job(job_id).unwrap().status,
        QueueJobStatus::Error
    );
    assert!(!state.active_queue_jobs.contains_key("printer-a"));
    assert_eq!(state.active_queue_job, None);
    assert_eq!(state.send_progress, None);
    assert!(state.pending_print_jobs.contains_key(&job_id));
    state.job_queue.retry(job_id).unwrap();
    assert_eq!(
        state.job_queue.job(job_id).unwrap().status,
        QueueJobStatus::Queued
    );
}

#[test]
fn stale_print_route_error_cannot_disturb_the_current_route() {
    let mut harness = app_harness(Vec2::new(900.0, 700.0));
    let state = harness.state_mut();
    let stale_job_id = 41;
    let current_job_id = 42;
    state.active_queue_job = Some(current_job_id);
    state
        .active_queue_jobs
        .insert("printer-a".into(), current_job_id);
    state.send_progress = Some(0.75);

    state
        .tx
        .send(Action::PrintRouteEncoded {
            printer_id: "printer-a".into(),
            job_id: stale_job_id,
            result: Err(anyhow::anyhow!("stale worker failure")),
        })
        .unwrap();
    state.apply_actions();

    assert_eq!(state.active_queue_job, Some(current_job_id));
    assert_eq!(
        state.active_queue_jobs.get("printer-a"),
        Some(&current_job_id)
    );
    assert_eq!(state.send_progress, Some(0.75));
    assert!(state.error.is_none());
}

#[test]
fn action_backlog_is_bounded_per_frame() {
    let mut harness = app_harness(Vec2::new(900.0, 700.0));
    let state = harness.state_mut();
    for _ in 0..=MAX_ACTIONS_PER_FRAME {
        state
            .tx
            .send(Action::SendProgress {
                job_id: u64::MAX,
                progress: 0.5,
            })
            .unwrap();
    }

    assert!(state.apply_actions());
    assert!(!state.apply_actions());
}

#[test]
fn calibration_method_chooser_names_printer_media_and_east_bay_credit() {
    let mut harness = app_harness(Vec2::new(1180.0, 900.0));
    open_calibration_fixture(&mut harness);
    harness.get_by_label("Printer: DHP700 · serial SERIAL-A · firmware 1.0");
    harness.get_by_label(
        "Media: PixCut S1 · 4×7 sticker paper · Liene Photo · kiss 0 · through 0 · 0 passes",
    );
    harness.get_by_label("Flatbed Scanner");
    harness.get_by_label("Manual");
    harness.get_by_label("View the documented method");
    harness.get_by_label("Progress");
    assert!(
        harness
            .get_by_role_and_label(egui::accesskit::Role::Button, "Skip step")
            .accesskit_node()
            .is_disabled()
    );
}

#[test]
fn calibration_activation_result_is_visible_on_success_and_failure() {
    let mut harness = app_harness(Vec2::new(900.0, 700.0));
    open_calibration_fixture(&mut harness);

    harness
        .state_mut()
        .present_calibration_activation_result(Err("Profile could not be activated.".into()));
    harness.run();
    harness.get_by_label("Profile could not be activated.");
    assert!(harness.state().calibration_session.is_some());

    harness
        .state_mut()
        .present_calibration_activation_result(Ok(()));
    harness.run();
    harness.get_by_label("Calibration profiles");
    harness.get_by_label("Calibration profile activated.");
    assert!(harness.state().calibration_session.is_none());
    assert!(harness.state().show_calibration_profiles);
}

#[test]
fn scan_evidence_opens_fullscreen_and_escape_returns_to_the_wizard() {
    let mut harness = app_harness(Vec2::new(900.0, 700.0));
    open_calibration_fixture(&mut harness);
    {
        let session = harness.state_mut().calibration_session.as_mut().unwrap();
        session.wizard.method = Some(CalibrationMethod::FlatbedScanner);
        session.wizard.step = crate::calibration::WizardStep::ReviewScan;
        session.wizard.training_scan = crate::calibration::ScanImportStatus::Imported {
            file_name: "scan.png".into(),
            accepted_targets: 8,
            all_quadrants: true,
        };
        session.training_scan_report = Some(crate::calibration::ScanAnalysisReport {
            format: crate::calibration::ScanImageFormat::Png,
            scan_dimensions_px: [5100, 7012],
            orientation: crate::calibration::ScanOrientation::Degrees0,
            scanner_to_print: crate::calibration::Affine2d::IDENTITY,
            backing_rgb: [255.0; 3],
            fiducial_rms_px: 0.25,
            run_binding_sha1: "0".repeat(40),
            targets: Vec::new(),
        });
        session.training_scan_preview_png = Some(Arc::from(
            include_bytes!("../../docs/review-evidence/transform-fixture.png").as_slice(),
        ));
        session.training_scan_preview_sha1 = Some("fixture-preview".into());
    }
    harness.run();

    harness
        .get_by_role_and_label(
            egui::accesskit::Role::Button,
            "Open full-screen calibration scan evidence",
        )
        .click_accesskit();
    harness.run();
    harness.get_by_label("Calibration scan and detected cut edges");
    harness.get_by_label("Close full-screen evidence");

    harness.key_press(egui::Key::Escape);
    harness.run();
    assert_eq!(
        harness
            .query_all_by_label("Calibration scan and detected cut edges")
            .count(),
        0
    );
    harness.get_by_label("Review detected targets");
    assert_eq!(harness.query_all_by_label("Leave calibration?").count(), 0);
}

#[test]
fn incomplete_scan_import_shows_the_exact_target_and_fit_diagnostics() {
    let mut harness = app_harness(Vec2::new(1180.0, 900.0));
    open_calibration_fixture(&mut harness);
    {
        let session = harness.state_mut().calibration_session.as_mut().unwrap();
        session.wizard.method = Some(CalibrationMethod::FlatbedScanner);
        session.wizard.step = crate::calibration::WizardStep::ImportValidationScan;
        session.wizard.validation_scan = crate::calibration::ScanImportStatus::Imported {
            file_name: "physical-scan.png".into(),
            accepted_targets: 5,
            all_quadrants: true,
        };
        session.validation_scan_report = Some(crate::calibration::ScanAnalysisReport {
            format: crate::calibration::ScanImageFormat::Png,
            scan_dimensions_px: [5100, 7012],
            orientation: crate::calibration::ScanOrientation::Degrees0,
            scanner_to_print: crate::calibration::Affine2d::IDENTITY,
            backing_rgb: [245.0, 247.0, 242.0],
            fiducial_rms_px: 0.25,
            run_binding_sha1: "0".repeat(40),
            targets: (1..=6)
                .map(|index| crate::calibration::ApertureDetection {
                    target_id: format!("VA{index}"),
                    status: if index == 1 {
                        crate::calibration::ScanTargetStatus::Review(
                            crate::calibration::ScanFailureReason::LowConfidence,
                        )
                    } else {
                        crate::calibration::ScanTargetStatus::Accepted
                    },
                    expected_center_mm: [15.0, 25.0],
                    observed_center_mm: Some([14.92, 24.80]),
                    radius_mm: Some(if index == 1 { 4.6721 } else { 5.0 }),
                    circle_rms_mm: Some(if index == 1 { 0.0776 } else { 0.01 }),
                    confidence: if index == 1 { 0.4319 } else { 0.80 },
                    covariance: None,
                    boundary_points_used: 119,
                })
                .collect(),
        });
    }
    harness.run();

    harness.get_by_label("Target-by-target detection");
    harness.get_by_label("VA1");
    harness.get_by_label("Review — marginal fit confidence");
    harness.get_by_label("4.67 / 0.078 mm");
    harness.get_by_label("Marginal fit: inspect the target's radius, fit RMS, and edge-sample count below; flatten or rescan only if the cut edge is torn or obscured.");
    assert!(
        harness
            .get_by_role_and_label(egui::accesskit::Role::Button, "Next")
            .accesskit_node()
            .is_disabled()
    );
    harness.get_by_label("Choose scan image…");
}

#[test]
fn discard_uses_a_modal_confirmation_and_can_return_to_calibration() {
    let mut harness = app_harness(Vec2::new(700.0, 620.0));
    open_calibration_fixture(&mut harness);
    harness.get_by_label("Use Manual").click_accesskit();
    harness.run();

    harness.get_by_label("Discard…").click_accesskit();
    harness.run();
    harness.get_by_label("Leave calibration?");
    harness.get_by_label("Save progress and exit");
    harness.get_by_label("Discard run");
    assert!(
        harness
            .get_by_role_and_label(egui::accesskit::Role::Button, "Next")
            .accesskit_node()
            .is_disabled()
    );
    harness.get_by_label("Keep calibrating").click_accesskit();
    harness.run();

    assert_eq!(harness.query_all_by_label("Leave calibration?").count(), 0);
    harness.get_by_label("Print-to-cut calibration");
    assert!(harness.state().calibration_session.is_some());

    harness.get_by_label("Discard…").click_accesskit();
    harness.run();
    harness.key_press(egui::Key::Escape);
    harness.run();
    assert_eq!(harness.query_all_by_label("Leave calibration?").count(), 0);
    assert!(harness.state().calibration_session.is_some());
}

#[test]
fn compact_calibration_uses_progress_header_and_reaches_manual_prepare() {
    let mut harness = app_harness(Vec2::new(560.0, 720.0));
    open_calibration_fixture(&mut harness);
    harness.get_by_label("Step 1 of 1");
    harness.get_by_label("Use Manual").click_accesskit();
    harness.run();
    harness.get_by_label("Next").click_accesskit();
    harness.run();
    harness.get_by_label("Before you start");
    harness.get_by_label("View the documented method");
    assert!(
        !harness
            .get_by_role_and_label(egui::accesskit::Role::Button, "Skip step")
            .accesskit_node()
            .is_disabled()
    );
}

#[test]
fn manual_calibration_can_continue_with_an_existing_printed_sheet() {
    let mut harness = app_harness(Vec2::new(560.0, 520.0));
    open_calibration_fixture(&mut harness);
    harness.get_by_label("Use Manual").click_accesskit();
    harness.run();
    harness.get_by_label("Next").click_accesskit();
    harness.run();
    harness.get_by_label("Skip step").click_accesskit();
    harness.run();
    harness.get_by_label("Print and cut the calibration sheet");
    {
        let session = harness.state_mut().calibration_session.as_mut().unwrap();
        session.wizard.primary_job = crate::calibration::JobStatus::Queued;
    }
    harness.run();
    assert!(
        harness
            .get_by_role_and_label(egui::accesskit::Role::Button, "Use existing sheet")
            .accesskit_node()
            .is_disabled()
    );
    {
        let session = harness.state_mut().calibration_session.as_mut().unwrap();
        session.wizard.primary_job = crate::calibration::JobStatus::Failed;
        session.primary_queue_job = Some(4242);
        session.second_queue_job = Some(4243);
        session.historical_queue_job_ids[1] = Some(4243);
        session.image_sha1[1] = Some("c".repeat(40));
        session.plotter_sha1[1] = Some("d".repeat(40));
        session.plotter_commands[1].push(crate::calibration::CalibrationPlotterCommand {
            kind: crate::calibration::CalibrationPlotterCommandKind::Draw,
            plotter_units: [30, 40],
        });
        session.device_job_ids.push(101);
        session.device_job_ids_by_slot[1].push(101);
    }
    harness.run();
    harness.get_by_label("Use existing sheet").click_accesskit();
    harness.run();

    let wizard = &harness.state().calibration_session.as_ref().unwrap().wizard;
    assert_eq!(wizard.step, crate::calibration::WizardStep::PrintArea);
    assert_eq!(
        wizard.primary_job,
        crate::calibration::JobStatus::ExistingSheet
    );
    assert_eq!(
        harness
            .state()
            .calibration_session
            .as_ref()
            .unwrap()
            .primary_queue_job,
        None
    );
    let session = harness.state().calibration_session.as_ref().unwrap();
    assert_eq!(session.second_queue_job, None);
    assert_eq!(session.historical_queue_job_ids[1], None);
    assert_eq!(session.image_sha1[1], None);
    assert_eq!(session.plotter_sha1[1], None);
    assert!(session.plotter_commands[1].is_empty());
    assert!(!session.device_job_ids.contains(&101));
    assert!(session.device_job_ids_by_slot[1].is_empty());
}

#[test]
fn manual_second_sheet_can_reuse_an_earlier_print() {
    let mut harness = app_harness(Vec2::new(560.0, 520.0));
    open_calibration_fixture(&mut harness);
    {
        let session = harness.state_mut().calibration_session.as_mut().unwrap();
        session.wizard.method = Some(crate::calibration::CalibrationMethod::ManualEastBay);
        session.wizard.step = crate::calibration::WizardStep::PrintSecondCalibration;
        session.wizard.second_sheet_choice =
            Some(crate::calibration::SecondSheetChoice::MeasureAnotherSheet);
        session.wizard.second_job = crate::calibration::JobStatus::Failed;
        session.second_queue_job = Some(5252);
        session.historical_queue_job_ids[1] = Some(5252);
        session.image_sha1[1] = Some("a".repeat(40));
        session.plotter_sha1[1] = Some("b".repeat(40));
        session.plotter_commands[1].push(crate::calibration::CalibrationPlotterCommand {
            kind: crate::calibration::CalibrationPlotterCommandKind::Draw,
            plotter_units: [10, 20],
        });
        session.device_job_ids.push(99);
        session.device_job_ids_by_slot[1].push(99);
    }
    harness.run();
    harness.get_by_label("Print and cut a freshly loaded second sheet");
    harness.get_by_label("Use existing sheet").click_accesskit();
    harness.run();

    let session = harness.state().calibration_session.as_ref().unwrap();
    assert_eq!(
        session.wizard.step,
        crate::calibration::WizardStep::SecondPrintScale
    );
    assert_eq!(
        session.wizard.second_job,
        crate::calibration::JobStatus::ExistingSheet
    );
    assert_eq!(session.second_queue_job, None);
    assert_eq!(session.historical_queue_job_ids[1], None);
    assert_eq!(session.image_sha1[1], None);
    assert_eq!(session.plotter_sha1[1], None);
    assert!(session.plotter_commands[1].is_empty());
    assert!(!session.device_job_ids.contains(&99));
    assert!(session.device_job_ids_by_slot[1].is_empty());
}

fn add_selected_fixture(harness: &mut Harness<'_, SapodillaApp>) {
    let ctx = harness.ctx.clone();
    let fixture = include_bytes!("../../docs/review-evidence/transform-fixture.png");
    let image = LoadedImage::new(&ctx, fixture, Some(Pos2::new(120.0, 90.0))).unwrap();
    let state = harness.state_mut();
    state.loaded_images.push(image);
    state.selected_images = vec![0];
    harness.run();
}

fn import_fixture(harness: &mut Harness<'_, SapodillaApp>, name: &str) {
    let mut image = LoadedImage::new(
        &harness.ctx,
        include_bytes!("../../docs/review-evidence/transform-fixture.png"),
        None,
    )
    .unwrap();
    image.name = name.to_owned();
    harness
        .state()
        .tx
        .send(Action::LoadedImage(Ok(image)))
        .unwrap();
    harness.run();
}

fn fixture_image(harness: &Harness<'_, SapodillaApp>, name: &str, offset: Pos2) -> LoadedImage {
    let mut image = LoadedImage::new(
        &harness.ctx,
        include_bytes!("../../docs/review-evidence/transform-fixture.png"),
        Some(offset),
    )
    .unwrap();
    image.name = name.to_owned();
    image
}

fn rect_path(rect: egui::Rect) -> LineString<f32> {
    LineString::from(vec![
        (rect.min.x, rect.min.y),
        (rect.max.x, rect.min.y),
        (rect.max.x, rect.max.y),
        (rect.min.x, rect.max.y),
        (rect.min.x, rect.min.y),
    ])
}

fn set_fixture_size(image: &mut LoadedImage, size: Vec2) {
    image.scale = Vec2::new(
        size.x / image.sized_texture.size.x,
        size.y / image.sized_texture.size.y,
    );
}

#[test]
fn print_and_cut_auto_pack_moves_owned_cutlines_and_keeps_their_gap() {
    let mut harness = app_harness(Vec2::new(1280.0, 900.0));
    let mut first = fixture_image(&harness, "First", Pos2::new(500.0, 500.0));
    let mut second = fixture_image(&harness, "Second", Pos2::new(520.0, 520.0));
    first.scale *= 0.35;
    second.scale *= 0.35;
    let first_id = first.id.clone();
    let second_id = second.id.clone();
    let first_bounds = egui::Rect::from_min_size(first.visual_offset(), first.rotated_size());
    let second_bounds = egui::Rect::from_min_size(second.visual_offset(), second.rotated_size());
    let paths = vec![
        rect_path(first_bounds.expand(24.0)),
        rect_path(second_bounds.expand(36.0)),
    ];
    let old_offsets = [first.offset, second.offset];
    let old_path_starts = [paths[0].0[0], paths[1].0[0]];

    let state = harness.state_mut();
    state.selected_mode = DEVICES[state.selected_device]
        .modes
        .iter()
        .position(|mode| mode.mode_type.has_cutting())
        .unwrap();
    state.loaded_images = vec![first, second];
    state.cut_shapes = paths.clone();
    state.manual_cut_shapes = paths;
    state.auto_cut_count = 0;
    state.cut_modes = vec![CutMode::Kiss; 2];
    state.cutline_owners = vec![
        Some(CutlineOwner::Image(first_id)),
        Some(CutlineOwner::Image(second_id)),
    ];
    state.cutline_locked = vec![false; 2];
    state.peel_tab_positions = vec![None; 2];
    state.pack_allow_rotation = false;
    state.pack_gap_mm = 2.0;

    assert_eq!(state.auto_pack().len(), 2);
    let canvas = state.get_canvas();
    let safe_offset = (canvas.size - canvas.safe_area) / 2.0;
    let safe_rect = egui::Rect::from_min_size(safe_offset.to_pos2(), canvas.safe_area);
    let cut_bounds = state
        .cut_shapes
        .iter()
        .map(cut_path_rect)
        .collect::<Option<Vec<_>>>()
        .unwrap();
    assert!(
        cut_bounds
            .iter()
            .all(|bounds| safe_rect.contains_rect(*bounds))
    );
    let gap = state.pack_gap_mm * DEVICES[state.selected_device].dpi / 25.4;
    assert!(!cut_bounds[0].expand(gap - 0.01).intersects(cut_bounds[1]));
    for index in 0..2 {
        let image_delta = state.loaded_images[index].offset - old_offsets[index];
        let path_delta = state.cut_shapes[index].0[0] - old_path_starts[index];
        assert!((image_delta.x - path_delta.x).abs() < 0.001);
        assert!((image_delta.y - path_delta.y).abs() < 0.001);
        assert_eq!(state.manual_cut_shapes[index], state.cut_shapes[index]);
    }
}

#[test]
fn print_only_auto_pack_ignores_and_does_not_move_cutlines() {
    let mut harness = app_harness(Vec2::new(1280.0, 900.0));
    let mut image = fixture_image(&harness, "Print only", Pos2::new(500.0, 500.0));
    image.scale *= 0.35;
    let path = rect_path(egui::Rect::from_min_size(
        Pos2::new(900.0, 1_200.0),
        Vec2::new(120.0, 120.0),
    ));
    let state = harness.state_mut();
    assert!(
        !DEVICES[state.selected_device].modes[state.selected_mode]
            .mode_type
            .has_cutting()
    );
    state.loaded_images = vec![image];
    state.cut_shapes = vec![path.clone()];
    state.manual_cut_shapes = vec![path.clone()];
    state.cut_modes = vec![CutMode::Kiss];
    state.cutline_owners = vec![None];
    state.cutline_locked = vec![false];
    state.peel_tab_positions = vec![None];

    assert_eq!(state.auto_pack(), vec![0]);
    assert_eq!(state.cut_shapes, [path]);
}

#[test]
fn disabled_locked_cutline_does_not_pin_its_artwork_during_print_and_cut_pack() {
    let mut harness = app_harness(Vec2::new(1280.0, 900.0));
    let mut image = fixture_image(&harness, "Movable", Pos2::new(500.0, 500.0));
    image.scale *= 0.35;
    let image_id = image.id.clone();
    let old_offset = image.offset;
    let path = rect_path(egui::Rect::from_min_size(
        image.visual_offset(),
        image.rotated_size(),
    ));
    let state = harness.state_mut();
    state.selected_mode = DEVICES[state.selected_device]
        .modes
        .iter()
        .position(|mode| mode.mode_type.has_cutting())
        .unwrap();
    state.loaded_images = vec![image];
    state.cut_shapes = vec![path.clone()];
    state.manual_cut_shapes = vec![path];
    state.cut_modes = vec![CutMode::Disabled];
    state.cutline_owners = vec![Some(CutlineOwner::Image(image_id))];
    state.cutline_locked = vec![true];
    state.peel_tab_positions = vec![None];
    state.pack_allow_rotation = false;

    assert_eq!(state.auto_pack(), vec![0]);
    assert_ne!(state.loaded_images[0].offset, old_offset);
}

#[test]
fn enabled_locked_cutline_does_not_reject_a_packable_library_candidate() {
    let mut harness = app_harness(Vec2::new(1280.0, 900.0));
    let mut pinned = fixture_image(&harness, "Pinned", Pos2::new(500.0, 500.0));
    let mut candidate = fixture_image(&harness, "Candidate", Pos2::new(700.0, 700.0));
    set_fixture_size(&mut pinned, Vec2::splat(100.0));
    set_fixture_size(&mut candidate, Vec2::splat(100.0));
    let pinned_id = pinned.id.clone();
    let pinned_path = rect_path(egui::Rect::from_min_size(
        pinned.visual_offset(),
        pinned.rotated_size(),
    ));
    let state = harness.state_mut();
    state.selected_mode = DEVICES[state.selected_device]
        .modes
        .iter()
        .position(|mode| mode.mode_type.has_cutting())
        .unwrap();
    state.loaded_images = vec![pinned, candidate];
    state.cut_shapes = vec![pinned_path.clone()];
    state.manual_cut_shapes = vec![pinned_path];
    state.auto_cut_count = 0;
    state.cut_modes = vec![CutMode::Kiss];
    state.cutline_owners = vec![Some(CutlineOwner::Image(pinned_id))];
    state.cutline_locked = vec![true];
    state.peel_tab_positions = vec![None];
    state.pack_allow_rotation = false;
    state.pack_gap_mm = 0.0;

    let packed = state.auto_pack();
    let required_count = packed.len() + state.pack_overflow;
    assert_eq!(packed, vec![1]);
    assert!(fill_trial_succeeded(&packed, 1, required_count));
}

#[test]
fn unowned_cutline_inside_artwork_stays_fixed_during_auto_pack() {
    let mut harness = app_harness(Vec2::new(1280.0, 900.0));
    let mut image = fixture_image(&harness, "Artwork", Pos2::new(500.0, 500.0));
    set_fixture_size(&mut image, Vec2::new(180.0, 100.0));
    image.rotation_degrees = 35.0;
    let old_offset = image.offset;
    let path = rect_path(egui::Rect::from_center_size(
        image.offset + image.size() / 2.0,
        Vec2::splat(30.0),
    ));
    let state = harness.state_mut();
    state.selected_mode = DEVICES[state.selected_device]
        .modes
        .iter()
        .position(|mode| mode.mode_type.has_cutting())
        .unwrap();
    state.loaded_images = vec![image];
    state.cut_shapes = vec![path.clone()];
    state.manual_cut_shapes = vec![path.clone()];
    state.auto_cut_count = 0;
    state.cut_modes = vec![CutMode::Kiss];
    state.cutline_owners = vec![None];
    state.cutline_locked = vec![false];
    state.peel_tab_positions = vec![None];
    state.pack_allow_rotation = false;
    state.pack_gap_mm = 0.0;

    assert_eq!(state.auto_pack(), vec![0]);
    assert_ne!(state.loaded_images[0].offset, old_offset);
    assert_eq!(state.cut_shapes, [path]);
    assert_eq!(state.cutline_owners, [None]);
}

#[test]
fn rotated_auto_pack_keeps_owned_cutline_aligned() {
    let mut harness = app_harness(Vec2::new(1280.0, 900.0));
    let mut image = fixture_image(&harness, "Rotating", Pos2::new(500.0, 500.0));
    let state = harness.state_mut();
    state.selected_mode = DEVICES[state.selected_device]
        .modes
        .iter()
        .position(|mode| mode.mode_type.has_cutting())
        .unwrap();
    let canvas = state.get_canvas();
    let safe_offset = (canvas.size - canvas.safe_area) / 2.0;
    set_fixture_size(&mut image, Vec2::new(180.0, 80.0));
    let image_id = image.id.clone();
    let owned_path = rect_path(egui::Rect::from_min_size(
        image.visual_offset(),
        image.rotated_size(),
    ));
    let obstacle = rect_path(egui::Rect::from_min_size(
        safe_offset.to_pos2(),
        Vec2::new(canvas.safe_area.x - 85.0, canvas.safe_area.y),
    ));
    state.loaded_images = vec![image];
    state.cut_shapes = vec![owned_path, obstacle.clone()];
    state.manual_cut_shapes = state.cut_shapes.clone();
    state.auto_cut_count = 0;
    state.cut_modes = vec![CutMode::Kiss; 2];
    state.cutline_owners = vec![Some(CutlineOwner::Image(image_id)), None];
    state.cutline_locked = vec![false; 2];
    state.peel_tab_positions = vec![None; 2];
    state.pack_allow_rotation = true;
    state.pack_gap_mm = 0.0;

    assert_eq!(state.auto_pack(), vec![0]);
    assert_eq!(state.loaded_images[0].rotation_degrees, 90.0);
    let image_bounds = egui::Rect::from_min_size(
        state.loaded_images[0].visual_offset(),
        state.loaded_images[0].rotated_size(),
    );
    let path_bounds = cut_path_rect(&state.cut_shapes[0]).unwrap();
    assert!((image_bounds.min.x - path_bounds.min.x).abs() < 0.001);
    assert!((image_bounds.min.y - path_bounds.min.y).abs() < 0.001);
    assert!((image_bounds.max.x - path_bounds.max.x).abs() < 0.001);
    assert!((image_bounds.max.y - path_bounds.max.y).abs() < 0.001);
    assert_eq!(state.cut_shapes[1], obstacle);
}

#[test]
fn rejected_library_fill_restores_cut_generation_state_and_geometry() {
    let mut harness = app_harness(Vec2::new(1280.0, 900.0));
    let mut movable = fixture_image(&harness, "Existing", Pos2::new(500.0, 500.0));
    let mut obstacle = fixture_image(&harness, "Locked obstacle", Pos2::ZERO);
    let mut lower_obstacle = fixture_image(&harness, "Lower obstacle", Pos2::ZERO);
    let mut candidate = fixture_image(&harness, "Candidate", Pos2::ZERO);
    let state = harness.state_mut();
    state.selected_mode = DEVICES[state.selected_device]
        .modes
        .iter()
        .position(|mode| mode.mode_type.has_cutting())
        .unwrap();
    let canvas = state.get_canvas();
    let safe_offset = (canvas.size - canvas.safe_area) / 2.0;

    obstacle.offset = safe_offset.to_pos2();
    set_fixture_size(&mut movable, Vec2::splat(100.0));
    set_fixture_size(
        &mut obstacle,
        Vec2::new(canvas.safe_area.x - 110.0, canvas.safe_area.y),
    );
    set_fixture_size(
        &mut lower_obstacle,
        Vec2::new(110.0, canvas.safe_area.y - 110.0),
    );
    set_fixture_size(&mut candidate, Vec2::splat(100.0));
    obstacle.locked = true;
    lower_obstacle.locked = true;
    lower_obstacle.offset = safe_offset.to_pos2() + Vec2::new(canvas.safe_area.x - 110.0, 110.0);
    let movable_id = movable.id.clone();
    let path = rect_path(egui::Rect::from_min_size(
        movable.visual_offset(),
        movable.rotated_size(),
    ));
    state.loaded_images = vec![movable, obstacle, lower_obstacle];
    state.library = vec![candidate];
    state.cut_shapes = vec![path.clone()];
    state.manual_cut_shapes = vec![path];
    state.auto_cut_count = 0;
    state.cut_modes = vec![CutMode::Kiss];
    state.cutline_owners = vec![Some(CutlineOwner::Image(movable_id))];
    state.cutline_locked = vec![false];
    state.peel_tab_positions = vec![None];
    state.pack_allow_rotation = false;
    state.pack_gap_mm = 0.0;
    state.cut_geometry_snapshot = Some(state.current_cut_geometry());
    state.cut_validation_snapshot = Some(CutValidationSnapshot {
        geometry_hash: 77,
        canvas_size: [101, 202],
        safe_area: [99, 199],
    });
    state.cut_progress = Some((2, 5));
    state.active_cut_generation = Some(42);

    let old_transforms = state
        .loaded_images
        .iter()
        .map(|image| (image.offset, image.rotation_degrees))
        .collect::<Vec<_>>();
    let old_paths = state.cut_shapes.clone();
    let old_manual_paths = state.manual_cut_shapes.clone();
    let old_owners = state.cutline_owners.clone();
    let old_geometry_snapshot = state.cut_geometry_snapshot.clone();
    let old_validation_snapshot = state.cut_validation_snapshot.clone();

    state.add_library_to_sheet(false);

    assert_eq!(state.loaded_images.len(), 3);
    assert_eq!(
        state
            .loaded_images
            .iter()
            .map(|image| (image.offset, image.rotation_degrees))
            .collect::<Vec<_>>(),
        old_transforms
    );
    assert_eq!(state.cut_shapes, old_paths);
    assert_eq!(state.manual_cut_shapes, old_manual_paths);
    assert_eq!(state.cutline_owners, old_owners);
    assert_eq!(state.cut_geometry_snapshot, old_geometry_snapshot);
    assert_eq!(state.cut_validation_snapshot, old_validation_snapshot);
    assert_eq!(state.cut_progress, Some((2, 5)));
    assert_eq!(state.active_cut_generation, Some(42));
}

#[test]
fn fresh_workspace_exposes_primary_and_contextual_entry_points() {
    let mut harness = app_harness(Vec2::new(1280.0, 900.0));

    assert_eq!(harness.query_all_by_label("Add artwork").count(), 1);
    harness.get_by_label("Auto-pack sheet");
    harness.get_by_label("Save document");
    harness.get_by_role_and_label(egui::accesskit::Role::Button, "Library panel");
    harness.get_by_role_and_label(egui::accesskit::Role::Button, "Inspector panel");
    harness.get_by_role_and_label(egui::accesskit::Role::ComboBox, "Transport");
    harness.get_by_label("Production queue (0)");

    harness.get_by_label("Switch to Print & Cut").scroll_to_me();
    harness.run();
    harness.get_by_label("Switch to Print & Cut").click();
    harness.run();
    assert!(
        DEVICES[harness.state().selected_device].modes[harness.state().selected_mode]
            .mode_type
            .has_cutting()
    );
    harness.get_by_label("Generate Cut Lines");
    harness.get_by_label("Shape designer");
}

#[test]
fn start_artwork_help_can_be_dismissed_and_returns_for_a_new_sheet() {
    let mut harness = app_harness(Vec2::new(900.0, 700.0));
    harness.get_by_label("Start with your artwork");
    let dismiss =
        harness.get_by_role_and_label(egui::accesskit::Role::Button, "Dismiss start artwork help");
    assert!(dismiss.rect().width() >= 32.0 && dismiss.rect().height() >= 32.0);
    dismiss.click_accesskit();
    harness.run();
    assert_eq!(
        harness
            .query_all_by_label("Start with your artwork")
            .count(),
        0
    );

    harness.state_mut().start_new_sheet();
    harness.run();
    harness.get_by_label("Start with your artwork");
}

#[test]
fn compact_calibration_keeps_optional_skip_navigation_inside_the_viewport() {
    let mut harness = app_harness(Vec2::new(560.0, 520.0));
    open_calibration_fixture(&mut harness);
    {
        let wizard = &mut harness
            .state_mut()
            .calibration_session
            .as_mut()
            .unwrap()
            .wizard;
        advance_calibration_to_print_area(wizard);
    }
    harness.run();

    let skip = harness.get_by_role_and_label(egui::accesskit::Role::Button, "Skip step");
    let viewport = harness.ctx.content_rect();
    let title_rect = harness.get_by_label("Print-to-cut calibration").rect();
    let step_rect = harness
        .get_by_label("Optional: record the printable area")
        .rect();
    assert!(
        title_rect.bottom() <= viewport.bottom() && title_rect.top() >= viewport.top(),
        "calibration window should remain inside the compact viewport {viewport:?}, got {title_rect:?}"
    );
    assert!(
        skip.rect().bottom() <= viewport.bottom() && skip.rect().top() >= viewport.top(),
        "skip navigation should remain inside the compact viewport {viewport:?}; title {title_rect:?}, step {step_rect:?}, skip {:?}",
        skip.rect()
    );
    skip.click_accesskit();
    harness.run();
    assert_eq!(
        harness
            .state()
            .calibration_session
            .as_ref()
            .unwrap()
            .wizard
            .step,
        crate::calibration::WizardStep::PrintScale
    );
}

fn advance_calibration_to_print_area(wizard: &mut CalibrationWizard) {
    wizard
        .select_method(CalibrationMethod::ManualEastBay, 1)
        .unwrap();
    wizard.next(2).unwrap();
    wizard.confirm_prepared(true, 3);
    wizard.next(4).unwrap();
    wizard.set_job_status(
        CalibrationJobSlot::Primary,
        crate::calibration::JobStatus::Completed,
        5,
    );
    assert_eq!(
        wizard.next(6).unwrap(),
        crate::calibration::WizardStep::PrintArea
    );
}

#[test]
fn primary_workspace_controls_are_named_actionable_and_easy_to_target() {
    let harness = app_harness(Vec2::new(1280.0, 800.0));

    for label in [
        "Add artwork",
        "Auto-pack sheet",
        "Save document",
        "Library panel",
        "Inspector panel",
        "Fit sheet",
    ] {
        let node = harness.get_by_role_and_label(egui::accesskit::Role::Button, label);
        let accessible = node.accesskit_node();
        assert!(!accessible.is_disabled(), "{label} should be enabled");
        assert!(
            accessible
                .data()
                .supports_action(egui::accesskit::Action::Focus),
            "{label} should support accessibility focus"
        );
        assert!(
            accessible
                .data()
                .supports_action(egui::accesskit::Action::Click),
            "{label} should support accessibility click"
        );
        let rect = node.rect();
        assert!(
            rect.width() >= 32.0 && rect.height() >= 32.0,
            "{label} target should be at least 32×32 points, got {rect:?}"
        );
    }
}

#[test]
fn compact_workspace_keeps_panels_and_cut_discovery_reachable() {
    let mut harness = app_harness(Vec2::new(700.0, 720.0));

    for label in [
        "Add artwork",
        "Save document",
        "Library panel",
        "Inspector panel",
        "More toolbar actions",
    ] {
        let node = harness.get_by_role_and_label(egui::accesskit::Role::Button, label);
        let rect = node.rect();
        assert!(
            rect.width() >= 32.0 && rect.height() >= 32.0,
            "{label} target should be at least 32×32 points, got {rect:?}"
        );
        assert!(
            rect.right() <= 700.0 && rect.left() >= 0.0,
            "{label} should remain inside the compact viewport, got {rect:?}"
        );
    }
    harness.get_by_label("More toolbar actions").click();
    harness.run();
    harness.get_by_label("Auto-pack sheet");
    harness.get_by_label("Snap to guides");
    harness.get_by_label("Show grid");
    harness.get_by_label("Show rulers");
    harness.get_by_label("Show cut preview");
    harness.get_by_label("Edit cut nodes");
    harness.get_by_label("Inspector panel").click();
    harness.run();
    harness.get_by_label("Switch to Print & Cut");
}

#[test]
fn peel_tab_handle_can_be_dragged_around_the_cutline() {
    let mut harness = app_harness(Vec2::new(1280.0, 900.0));
    let state = harness.state_mut();
    state.selected_mode = DEVICES[state.selected_device]
        .modes
        .iter()
        .position(|mode| mode.mode_type.has_cutting())
        .unwrap();
    let path = LineString::from(vec![
        (400.0, 250.0),
        (800.0, 250.0),
        (800.0, 500.0),
        (400.0, 500.0),
        (400.0, 250.0),
    ]);
    state.cut_shapes = vec![path.clone()];
    state.manual_cut_shapes = vec![path];
    state.cut_modes = vec![CutMode::Kiss];
    state.cutline_owners = vec![None];
    state.cutline_locked = vec![false];
    state.peel_tab_positions = vec![None];
    state.peel_tabs = true;
    harness.run();
    harness.run();

    let handle = harness.get_by_label("Peel tab for path 1");
    // AccessKit reports the handle in the Scene's local coordinates, while
    // pointer events use global coordinates.
    let scene_to_global = harness
        .ctx
        .memory(|memory| {
            memory
                .layer_ids()
                .filter_map(|layer| harness.ctx.layer_transform_to_global(layer))
                .find(|transform| transform.scaling != 1.0)
        })
        .expect("canvas Scene should publish its global transform");
    let local_start = handle.rect().center();
    let start = scene_to_global * local_start;
    let target = scene_to_global * (local_start + Vec2::new(100.0, 0.0));

    harness.hover_at(start);
    harness.run();
    harness.drag_at(start);
    harness.run();
    harness.hover_at(target);
    harness.run();
    harness.drop_at(target);
    harness.run();

    assert!(
        harness.state().peel_tab_positions[0].is_some(),
        "dragging the named handle should store a perimeter placement"
    );
    let placed = harness.state().peel_tab_positions[0];

    harness.state_mut().cutline_locked[0] = true;
    harness.run();
    let locked_handle = harness.get_by_label("Peel tab for path 1");
    let local_start = locked_handle.rect().center();
    let start = scene_to_global * local_start;
    let target = scene_to_global * (local_start - Vec2::new(100.0, 0.0));
    harness.hover_at(start);
    harness.run();
    harness.drag_at(start);
    harness.run();
    harness.hover_at(target);
    harness.run();
    harness.drop_at(target);
    harness.run();

    assert_eq!(
        harness.state().peel_tab_positions[0],
        placed,
        "a locked template cutline must not allow its tab to move"
    );
}

#[test]
fn topbar_icon_toggles_have_clear_names_states_and_targets() {
    let mut harness = app_harness(Vec2::new(1440.0, 800.0));

    for label in [
        "Snap artwork to guides",
        "Layout grid",
        "Canvas rulers",
        "Cut preview",
        "Edit cut nodes",
        "Library panel",
        "Inspector panel",
    ] {
        let node = harness.get_by_role_and_label(egui::accesskit::Role::Button, label);
        let accessible = node.accesskit_node();
        assert!(
            accessible
                .data()
                .supports_action(egui::accesskit::Action::Focus),
            "{label} should support accessibility focus"
        );
        assert!(
            accessible
                .data()
                .supports_action(egui::accesskit::Action::Click),
            "{label} should support accessibility click"
        );
        assert!(
            accessible.data().toggled().is_some(),
            "{label} should expose its toggle state"
        );
        let rect = node.rect();
        assert!(
            rect.width() >= 32.0 && rect.height() >= 32.0,
            "{label} target should be at least 32×32 points, got {rect:?}"
        );
    }

    assert!(harness.state().show_grid);
    harness.get_by_label("Layout grid").click_accesskit();
    harness.run();
    assert!(!harness.state().show_grid);
    assert_eq!(
        harness
            .get_by_label("Layout grid")
            .accesskit_node()
            .data()
            .toggled(),
        Some(egui::accesskit::Toggled::False)
    );
}

#[test]
fn topbar_breakpoint_keeps_visible_actions_inside_the_viewport() {
    let compact = app_harness(Vec2::new(1159.0, 760.0));
    compact.get_by_label("More toolbar actions");
    for label in [
        "Add artwork",
        "Save document",
        "Library panel",
        "Inspector panel",
        "More toolbar actions",
    ] {
        let rect = compact.get_by_label(label).rect();
        assert!(
            rect.left() >= 0.0 && rect.right() <= 1159.0,
            "{label} should fit immediately below the compact breakpoint, got {rect:?}"
        );
    }

    let wide = app_harness(Vec2::new(1160.0, 760.0));
    for label in [
        "Add artwork",
        "Auto-pack sheet",
        "Save document",
        "Snap artwork to guides",
        "Layout grid",
        "Canvas rulers",
        "Cut preview",
        "Edit cut nodes",
        "Library panel",
        "Inspector panel",
    ] {
        let rect = wide.get_by_label(label).rect();
        assert!(
            rect.left() >= 0.0 && rect.right() <= 1160.0,
            "{label} should fit at the wide-layout breakpoint, got {rect:?}"
        );
    }
    assert_eq!(wide.query_all_by_label("More toolbar actions").count(), 0);
}

#[test]
fn imported_artwork_has_named_elements_and_a_usable_native_viewport() {
    let mut harness = app_harness(Vec2::new(1280.0, 800.0));
    import_fixture(&mut harness, "Accessibility fixture");

    assert_eq!(harness.state().selected_images, [0]);
    assert!(!harness.state().show_library_panel);

    let canvas = harness
        .get_by_role_and_label(egui::accesskit::Role::Pane, "Artwork canvas")
        .rect();
    assert!(
        canvas.width() >= 700.0 && canvas.height() >= 500.0,
        "native workspace should leave a useful canvas viewport, got {canvas:?}"
    );

    let artwork = harness.get_by_role_and_label(
        egui::accesskit::Role::Image,
        "Artwork: Accessibility fixture",
    );
    assert!(
        artwork
            .accesskit_node()
            .data()
            .supports_action(egui::accesskit::Action::Click)
    );
    let artwork = artwork.rect();
    assert!(
        artwork.width() >= 140.0 && artwork.height() >= 80.0,
        "new artwork should have a usable hit target, got {artwork:?}"
    );
    assert!(canvas.contains_rect(artwork));

    let fit = harness
        .get_by_role_and_label(egui::accesskit::Role::Button, "Fit sheet")
        .rect();
    assert!(
        fit.width() >= 44.0 && fit.height() >= 32.0,
        "Fit sheet should be an easy pointer target, got {fit:?}"
    );
}

#[test]
fn artwork_can_be_selected_through_accesskit() {
    let mut harness = app_harness(Vec2::new(1280.0, 800.0));
    import_fixture(&mut harness, "Keyboard fixture");
    harness.state_mut().selected_images.clear();
    harness.run();

    harness
        .get_by_role_and_label(egui::accesskit::Role::Image, "Artwork: Keyboard fixture")
        .click_accesskit();
    harness.run();
    assert_eq!(harness.state().selected_images, [0]);
}

#[test]
fn canvas_context_menu_targets_artwork_and_duplicates_then_removes_it() {
    let mut harness = app_harness(Vec2::new(1280.0, 900.0));
    add_selected_fixture(&mut harness);
    harness.state_mut().show_inspector_panel = false;
    harness.run();
    let original_id = harness.state().loaded_images[0].id.clone();
    let original_offset = harness.state().loaded_images[0].offset;

    harness
        .get_by_role_and_label(egui::accesskit::Role::Image, "Artwork: Untitled sticker")
        .click_secondary();
    harness.run();
    harness.run();
    for label in [
        "Duplicate",
        "Hide artwork",
        "Lock artwork",
        "Remove from sheet",
    ] {
        harness.get_by_label(label);
    }
    harness.get_by_label_contains("Arrange");
    harness.get_by_label_contains("Transform");
    harness.get_by_label("Duplicate").click_accesskit();
    harness.run();

    assert_eq!(harness.state().loaded_images.len(), 2);
    assert_eq!(harness.state().selected_images, [1]);
    let duplicate = &harness.state().loaded_images[1];
    assert_ne!(duplicate.id, original_id);
    assert_eq!(duplicate.offset, original_offset + Vec2::splat(20.0));

    harness
        .get_by_role_and_label(
            egui::accesskit::Role::Image,
            "Artwork: Untitled sticker copy",
        )
        .click_secondary();
    harness.run();
    harness.get_by_label("Remove from sheet").click_accesskit();
    harness.run();
    assert_eq!(harness.state().loaded_images.len(), 1);
    assert_eq!(harness.state().loaded_images[0].id, original_id);
}

#[test]
fn right_clicking_unselected_canvas_artwork_activates_that_artwork() {
    let mut harness = app_harness(Vec2::new(1280.0, 900.0));
    let first = fixture_image(&harness, "First", Pos2::new(80.0, 80.0));
    let second = fixture_image(&harness, "Second", Pos2::new(700.0, 900.0));
    harness.state_mut().loaded_images = vec![first, second];
    harness.state_mut().selected_images = vec![0];
    harness.state_mut().show_inspector_panel = false;
    harness.run();

    harness
        .get_by_role_and_label(egui::accesskit::Role::Image, "Artwork: Second")
        .click_secondary();
    harness.run();
    harness.run();
    assert_eq!(harness.state().selected_images, [1]);
    harness.get_by_label("Duplicate");
}

#[test]
fn locked_artwork_context_disables_transform_arrange_and_removal() {
    let mut harness = app_harness(Vec2::new(1280.0, 900.0));
    add_selected_fixture(&mut harness);
    harness.state_mut().loaded_images[0].locked = true;
    harness.state_mut().show_inspector_panel = false;
    harness.run();

    harness
        .get_by_role_and_label(egui::accesskit::Role::Image, "Artwork: Untitled sticker")
        .click_secondary();
    harness.run();
    harness.run();
    for node in [
        harness.get_by_label_contains("Arrange"),
        harness.get_by_label_contains("Transform"),
        harness.get_by_label("Remove from sheet"),
    ] {
        assert!(node.accesskit_node().is_disabled());
    }
    assert!(
        !harness
            .get_by_label("Unlock artwork")
            .accesskit_node()
            .is_disabled()
    );
}

#[test]
fn active_layer_actions_are_keyboard_accessible_and_can_restore_hidden_artwork() {
    let mut harness = app_harness(Vec2::new(1280.0, 900.0));
    add_selected_fixture(&mut harness);

    harness
        .get_by_label("Actions for layer Untitled sticker")
        .scroll_to_me();
    harness.run();
    harness
        .get_by_role_and_label(
            egui::accesskit::Role::Button,
            "Actions for layer Untitled sticker",
        )
        .click_accesskit();
    harness.run();
    harness.get_by_label("Hide artwork").click_accesskit();
    harness.run();
    assert!(!harness.state().loaded_images[0].visible);
    assert_eq!(
        harness
            .query_all_by_label("Artwork: Untitled sticker")
            .count(),
        0
    );

    harness
        .get_by_role_and_label(
            egui::accesskit::Role::Button,
            "Actions for layer Untitled sticker",
        )
        .click_accesskit();
    harness.run();
    harness.get_by_label("Show artwork").click_accesskit();
    harness.run();
    assert!(harness.state().loaded_images[0].visible);
}

#[test]
fn layer_preview_right_click_exposes_the_shared_artwork_menu() {
    let mut harness = app_harness(Vec2::new(1280.0, 900.0));
    add_selected_fixture(&mut harness);

    harness
        .get_by_label("Layer preview: Untitled sticker")
        .scroll_to_me();
    harness.run();
    harness
        .get_by_role_and_label(
            egui::accesskit::Role::Image,
            "Layer preview: Untitled sticker",
        )
        .click_secondary();
    harness.run();
    harness.get_by_label("Hide artwork");
    harness.get_by_label("Remove from sheet");
}

#[test]
fn layers_have_a_large_default_viewport() {
    let mut harness = app_harness(Vec2::new(1280.0, 900.0));
    add_selected_fixture(&mut harness);

    let layers = harness.get_by_label("Layers").rect();
    let selection = harness.get_by_label("Selection").rect();
    assert!(
        selection.top() - layers.bottom() >= 350.0,
        "Layers should reserve a tall default viewport, got {layers:?} to {selection:?}"
    );
}

#[test]
fn long_layer_filename_is_truncated_without_covering_actions() {
    let mut harness = app_harness(Vec2::new(1280.0, 900.0));
    let name =
        "A very long sticker filename that should be truncated before the layer action buttons.png";
    let select_label = format!("Select layer {name}");
    let visibility_label = format!("Hide {name}");
    let actions_label = format!("Actions for layer {name}");
    import_fixture(&mut harness, name);

    harness.get_by_label(&select_label).scroll_to_me();
    harness.run();

    let filename = harness
        .get_by_role_and_label(egui::accesskit::Role::Button, &select_label)
        .rect();
    let visibility = harness
        .get_by_role_and_label(egui::accesskit::Role::Button, &visibility_label)
        .rect();
    harness.get_by_role_and_label(egui::accesskit::Role::Button, &actions_label);

    assert!(
        filename.height() <= 24.0,
        "a long filename should remain on one line, got {filename:?}"
    );
    assert!(
        filename.right() <= visibility.left(),
        "the truncated filename must not overlap layer actions: {filename:?} vs {visibility:?}"
    );
}

#[test]
fn artwork_commands_preserve_block_order_selection_and_lock_protection() {
    let mut harness = app_harness(Vec2::new(1280.0, 900.0));
    let images = ["A", "B", "C", "D"]
        .into_iter()
        .enumerate()
        .map(|(index, name)| fixture_image(&harness, name, Pos2::new(index as f32 * 50.0, 0.0)))
        .collect::<Vec<_>>();
    let selected_ids = vec![images[1].id.clone(), images[2].id.clone()];
    harness.state_mut().loaded_images = images;
    harness.state_mut().selected_images = vec![1, 2];

    harness
        .state_mut()
        .apply_artwork_menu_action(views::ArtworkMenuAction {
            image_ids: selected_ids.clone(),
            command: views::ArtworkMenuCommand::BringToFront,
        });
    assert_eq!(
        harness
            .state()
            .loaded_images
            .iter()
            .map(|image| image.name.as_str())
            .collect::<Vec<_>>(),
        ["A", "D", "B", "C"]
    );
    assert_eq!(harness.state().selected_images, [2, 3]);

    harness.state_mut().loaded_images[2].locked = true;
    harness
        .state_mut()
        .apply_artwork_menu_action(views::ArtworkMenuAction {
            image_ids: selected_ids,
            command: views::ArtworkMenuCommand::Remove,
        });
    assert_eq!(harness.state().loaded_images.len(), 4);
}

#[test]
fn removing_artwork_clears_template_and_cutline_relationships() {
    let mut harness = app_harness(Vec2::new(1280.0, 900.0));
    let image = fixture_image(&harness, "Assigned", Pos2::ZERO);
    let image_id = image.id.clone();
    harness.state_mut().loaded_images = vec![image];
    harness.state_mut().selected_images = vec![0];
    harness
        .state_mut()
        .template_placeholders
        .push(TemplatePlaceholder {
            id: "slot".into(),
            name: "Slot".into(),
            bounds: [0.0, 0.0, 100.0, 100.0],
            rotation_degrees: 0.0,
            fit: PlaceholderFit::Contain,
            assigned_image_id: Some(image_id.clone()),
        });
    let owned_path = LineString::from(vec![(0.0, 0.0), (10.0, 10.0)]);
    harness.state_mut().cut_shapes.push(owned_path.clone());
    harness.state_mut().manual_cut_shapes.push(owned_path);
    harness.state_mut().cut_modes.push(CutMode::Kiss);
    harness
        .state_mut()
        .cutline_owners
        .push(Some(CutlineOwner::Image(image_id.clone())));
    harness.state_mut().cutline_locked.push(false);

    harness
        .state_mut()
        .apply_artwork_menu_action(views::ArtworkMenuAction {
            image_ids: vec![image_id],
            command: views::ArtworkMenuCommand::Remove,
        });

    assert!(harness.state().loaded_images.is_empty());
    assert!(harness.state().selected_images.is_empty());
    assert_eq!(
        harness.state().template_placeholders[0].assigned_image_id,
        None
    );
    assert!(harness.state().cut_shapes.is_empty());
    assert!(harness.state().manual_cut_shapes.is_empty());
    assert!(harness.state().cutline_owners.is_empty());
}

#[test]
fn compact_import_keeps_canvas_artwork_and_fit_control_reachable() {
    let mut harness = app_harness(Vec2::new(700.0, 720.0));
    import_fixture(&mut harness, "Compact fixture");

    let canvas = harness
        .get_by_role_and_label(egui::accesskit::Role::Pane, "Artwork canvas")
        .rect();
    assert!(canvas.width() >= 600.0 && canvas.height() >= 450.0);
    let artwork = harness
        .get_by_role_and_label(egui::accesskit::Role::Image, "Artwork: Compact fixture")
        .rect();
    assert!(artwork.width() >= 120.0 && artwork.height() >= 70.0);
    harness.get_by_label("Fit sheet").click();
    harness.run();
    assert!(!harness.state().canvas_fit_requested);
}

#[test]
fn selected_artwork_exposes_image_tools_and_template_fit() {
    let mut harness = app_harness(Vec2::new(1280.0, 900.0));
    add_selected_fixture(&mut harness);

    harness
        .get_by_role_and_label(egui::accesskit::Role::ComboBox, "Template slot fit")
        .scroll_to_me();
    harness.run();
    let fit = harness.get_by_role_and_label(egui::accesskit::Role::ComboBox, "Template slot fit");
    assert_eq!(fit.value().as_deref(), Some("Cover"));
    fit.click();
    harness.run();
    harness
        .get_by_role_and_label(egui::accesskit::Role::Button, "Stretch")
        .click();
    harness.run();
    assert_eq!(
        harness.state().loaded_images[0].template_fit,
        PlaceholderFit::Stretch
    );

    harness.get_by_label("Image adjustments").scroll_to_me();
    harness.run();
    harness.get_by_label("Image adjustments").click();
    harness.run();
    harness.get_by_role_and_label(egui::accesskit::Role::Slider, "Brightness");
    harness.get_by_role_and_label(egui::accesskit::Role::Button, "Apply");

    harness.get_by_label("Background removal").scroll_to_me();
    harness.run();
    harness.get_by_label("Background removal").click();
    harness.run();
    harness.get_by_role_and_label(egui::accesskit::Role::Button, "Remove edge background");

    harness.get_by_label("Selected artwork name").scroll_to_me();
    harness.run();
    harness.get_by_role_and_label(egui::accesskit::Role::TextInput, "Selected artwork name");
}

#[test]
fn library_thumbnail_button_has_an_action_specific_accessible_name() {
    let mut harness = app_harness(Vec2::new(1280.0, 900.0));
    let mut image = LoadedImage::new(
        &harness.ctx,
        include_bytes!("../../docs/review-evidence/transform-fixture.png"),
        None,
    )
    .unwrap();
    image.name = "Library fixture".to_owned();
    harness.state_mut().library.push(image);
    harness.run();

    harness.get_by_role_and_label(
        egui::accesskit::Role::Button,
        "Remove Library fixture from Library",
    );
    harness
        .get_by_role_and_label(
            egui::accesskit::Role::Button,
            "Add Library fixture to sheet",
        )
        .click_accesskit();
    harness.run();
    assert_eq!(harness.state().loaded_images.len(), 1);
    assert_eq!(harness.state().selected_images, [0]);
}

#[test]
fn layer_transform_fields_have_associated_accessible_names() {
    let mut harness = app_harness(Vec2::new(1280.0, 900.0));
    add_selected_fixture(&mut harness);

    harness.get_by_label("Selected artwork name").scroll_to_me();
    harness.run();
    harness.get_by_role_and_label(egui::accesskit::Role::TextInput, "Selected artwork name");
    for label in ["X:", "Y:", "W:", "H:"] {
        harness.get_by_role_and_label(egui::accesskit::Role::SpinButton, label);
    }
    harness.get_by_role_and_label(egui::accesskit::Role::Slider, "Rotation");
    harness.get_by_role_and_label(egui::accesskit::Role::Button, "Unlock artwork proportions");
}

#[test]
fn sidebar_icon_controls_have_clear_names_states_and_targets() {
    let mut harness = app_harness(Vec2::new(1280.0, 900.0));
    add_selected_fixture(&mut harness);

    harness.get_by_label("Align artwork left").scroll_to_me();
    harness.run();
    for label in [
        "Align artwork left",
        "Center artwork horizontally",
        "Align artwork right",
        "Align artwork top",
        "Center artwork vertically",
        "Align artwork bottom",
        "Unlock artwork proportions",
    ] {
        let node = harness.get_by_role_and_label(egui::accesskit::Role::Button, label);
        let rect = node.rect();
        assert!(
            rect.width() >= 32.0 && rect.height() >= 32.0,
            "{label} target should be at least 32×32 points, got {rect:?}"
        );
    }

    harness.get_by_label("Hide Untitled sticker").scroll_to_me();
    harness.run();
    let visibility =
        harness.get_by_role_and_label(egui::accesskit::Role::Button, "Hide Untitled sticker");
    assert_eq!(
        visibility.accesskit_node().data().toggled(),
        Some(egui::accesskit::Toggled::True)
    );
    assert!(visibility.rect().width() >= 32.0 && visibility.rect().height() >= 32.0);
    harness.get_by_role_and_label(egui::accesskit::Role::Button, "Lock Untitled sticker");
    harness.get_by_role_and_label(
        egui::accesskit::Role::Button,
        "Actions for layer Untitled sticker",
    );
}

#[test]
fn compact_layer_row_keeps_every_icon_action_inside_the_viewport() {
    let mut harness = app_harness(Vec2::new(700.0, 720.0));
    add_selected_fixture(&mut harness);
    harness.get_by_label("Inspector panel").click();
    harness.run();

    harness
        .get_by_label("Actions for layer Untitled sticker")
        .scroll_to_me();
    harness.run();
    for label in [
        "Hide Untitled sticker",
        "Lock Untitled sticker",
        "Actions for layer Untitled sticker",
    ] {
        let rect = harness
            .get_by_role_and_label(egui::accesskit::Role::Button, label)
            .rect();
        assert!(
            rect.left() >= 0.0 && rect.right() <= 700.0,
            "{label} should remain inside the compact viewport, got {rect:?}"
        );
        assert!(rect.width() >= 32.0 && rect.height() >= 32.0);
    }
}

#[test]
fn adjacent_layers_use_compact_scannable_rows() {
    let mut harness = app_harness(Vec2::new(1280.0, 900.0));
    let first_layer = fixture_image(&harness, "First layer", Pos2::ZERO);
    let second_layer = fixture_image(&harness, "Second layer", Pos2::new(50.0, 50.0));
    harness.state_mut().loaded_images = vec![first_layer, second_layer];
    harness.state_mut().selected_images = vec![0];
    harness.run();

    harness
        .get_by_label("Actions for layer First layer")
        .scroll_to_me();
    harness.run();
    let first = harness.get_by_label("Actions for layer First layer").rect();
    let second = harness
        .get_by_label("Actions for layer Second layer")
        .rect();
    let row_pitch = (second.center().y - first.center().y).abs();
    assert!(
        row_pitch <= 80.0,
        "adjacent layers should stay compact and scannable, got {row_pitch:.1} points"
    );
}

#[test]
fn layer_thumbnail_accessible_action_selects_the_promised_layer() {
    let mut harness = app_harness(Vec2::new(1280.0, 900.0));
    let first_layer = fixture_image(&harness, "First layer", Pos2::ZERO);
    let second_layer = fixture_image(&harness, "Second layer", Pos2::new(50.0, 50.0));
    harness.state_mut().loaded_images = vec![first_layer, second_layer];
    harness.state_mut().selected_images = vec![0];
    harness.run();

    harness
        .get_by_label("Select layer Second layer from thumbnail")
        .scroll_to_me();
    harness.run();
    harness
        .get_by_role_and_label(
            egui::accesskit::Role::Button,
            "Select layer Second layer from thumbnail",
        )
        .click_accesskit();
    harness.run();

    assert_eq!(harness.state().selected_images, [1]);
}

#[test]
fn replacement_preserves_fit_through_template_save() {
    let mut harness = app_harness(Vec2::new(1280.0, 900.0));
    add_selected_fixture(&mut harness);
    harness.state_mut().loaded_images[0].template_fit = PlaceholderFit::Stretch;
    let image_id = harness.state().loaded_images[0].id.clone();
    let replacement = LoadedImage::new(
        &harness.ctx,
        include_bytes!("../../docs/review-evidence/transform-fixture.png"),
        None,
    )
    .unwrap();
    assert_eq!(replacement.template_fit, PlaceholderFit::Cover);

    harness
        .state()
        .tx
        .send(Action::ReplacedImage {
            image_id,
            result: Ok(replacement),
        })
        .unwrap();
    harness.run();

    assert_eq!(
        harness.state().loaded_images[0].template_fit,
        PlaceholderFit::Stretch
    );
    let document = harness.state().document(DocumentKind::Template).unwrap();
    assert_eq!(document.images[0].template_fit, PlaceholderFit::Stretch);
    assert_eq!(
        document.template_placeholders[0].fit,
        PlaceholderFit::Stretch
    );
}

#[test]
fn rotated_alignment_buttons_use_visible_bounds() {
    let mut harness = app_harness(Vec2::new(1280.0, 900.0));
    add_selected_fixture(&mut harness);
    harness.state_mut().loaded_images[0].rotation_degrees = 37.0;
    harness.run();

    harness.get_by_role_and_label(egui::accesskit::Role::Button, "Align artwork left");
    harness.get_by_role_and_label(egui::accesskit::Role::Button, "Align artwork right");
    harness.get_by_role_and_label(egui::accesskit::Role::Button, "Align artwork top");
    harness.get_by_role_and_label(egui::accesskit::Role::Button, "Align artwork bottom");

    let canvas = harness.state().get_canvas().size;
    align_image_to_sheet(
        &mut harness.state_mut().loaded_images[0],
        canvas,
        SheetAlignment::Left,
    );
    let image = &harness.state().loaded_images[0];
    assert!(image.visual_offset().x.abs() < 0.01);

    align_image_to_sheet(
        &mut harness.state_mut().loaded_images[0],
        canvas,
        SheetAlignment::Top,
    );
    let image = &harness.state().loaded_images[0];
    assert!(image.visual_offset().y.abs() < 0.01);

    align_image_to_sheet(
        &mut harness.state_mut().loaded_images[0],
        canvas,
        SheetAlignment::Right,
    );
    let state = harness.state();
    let image = &state.loaded_images[0];
    assert!(
        (image.visual_offset().x + image.rotated_size().x - state.get_canvas().size.x).abs() < 0.01
    );

    align_image_to_sheet(
        &mut harness.state_mut().loaded_images[0],
        canvas,
        SheetAlignment::Bottom,
    );
    let state = harness.state();
    let image = &state.loaded_images[0];
    assert!(
        (image.visual_offset().y + image.rotated_size().y - state.get_canvas().size.y).abs() < 0.01
    );
}

#[test]
fn production_queue_header_reports_job_count() {
    let mut harness = app_harness(Vec2::new(1280.0, 900.0));
    harness
        .state_mut()
        .job_queue
        .enqueue(JobSpec::named("Reachability test"));
    harness.run();

    harness.get_by_label("Production queue (1)").click();
    harness.run();
    harness.get_by_label_contains("Reachability test");
}
