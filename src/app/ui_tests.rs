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
