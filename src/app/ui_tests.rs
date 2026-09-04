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

#[test]
fn fresh_workspace_exposes_primary_and_contextual_entry_points() {
    let mut harness = app_harness(Vec2::new(1280.0, 900.0));

    assert_eq!(harness.query_all_by_label("+ Add artwork").count(), 2);
    harness.get_by_label("Auto-pack");
    harness.get_by_label("Save");
    harness.get_by_role_and_label(egui::accesskit::Role::Button, "Library");
    harness.get_by_role_and_label(egui::accesskit::Role::Button, "Inspector");
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

    for label in ["Auto-pack", "Save", "Library", "Inspector", "Fit sheet"] {
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

    harness.get_by_label("More");
    harness.get_by_label("Library");
    harness.get_by_label("Inspector").click();
    harness.run();
    harness.get_by_label("Switch to Print & Cut");
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

    harness.get_by_label("Layer name").scroll_to_me();
    harness.run();
    harness.get_by_role_and_label(egui::accesskit::Role::TextInput, "Layer name");
    for label in ["X:", "Y:", "W:", "H:", "Layer rotation"] {
        harness.get_by_role_and_label(egui::accesskit::Role::SpinButton, label);
    }
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

    harness.get_by_role_and_label(egui::accesskit::Role::Button, "Left");
    harness.get_by_role_and_label(egui::accesskit::Role::Button, "Right");
    harness.get_by_role_and_label(egui::accesskit::Role::Button, "Top");
    harness.get_by_role_and_label(egui::accesskit::Role::Button, "Bottom");

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
