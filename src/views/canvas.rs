use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
};

use egui::{
    Align2, Color32, CursorIcon, FontId, Frame, Id, Key, KeyboardShortcut, Modifiers, Painter,
    Pos2, Rect, Response, Scene, Sense, Shape, Stroke, Ui, Vec2, WidgetInfo, WidgetType,
    emath::{self, RectTransform},
};
use geo::LineString;
use tracing::instrument;

use crate::{
    SapodillaApp,
    app::CanvasUnit,
    cut::apply_overcut,
    export::toolpath_stats_iter,
    peel_tab::{PeelTab, nearest_perimeter_position, peel_tabs as build_peel_tabs},
    protocol::DEVICES,
    toolpath::{CutMode, CutPhase, effective_cut_modes, plan_cut_phases},
};

const CUT_LINE_WIDTH: f32 = 3.0;
const MIN_GRID_SCREEN_SPACING: f32 = 24.0;
const MAX_GRID_LINES_PER_AXIS: usize = 512;
const RULER_SIZE: f32 = 24.0;
const CANVAS_BORDER_WIDTH: f32 = 4.0;
const MIN_SCENE_SCALE: f32 = 0.1;
const MAX_SCENE_SCALE: f32 = 3.0;
const TRANSFORM_HANDLE_SIZE: f32 = 9.0;
const TRANSFORM_HIT_SIZE: f32 = 32.0;
const ROTATION_HANDLE_OFFSET: f32 = 32.0;
const CORNER_ROTATION_ZONE_OFFSET: f32 = 30.0;
const MIN_IMAGE_SIZE: f32 = 1.0;
const MAX_IMAGE_SCALE: f32 = 1_000.0;

const DELETE_SHORTCUT: KeyboardShortcut = KeyboardShortcut::new(Modifiers::NONE, Key::Delete);
const BACKSPACE_SHORTCUT: KeyboardShortcut = KeyboardShortcut::new(Modifiers::NONE, Key::Backspace);

const NORMAL_UV: Rect = Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(1.0, 1.0));

#[derive(Clone, Debug)]
pub(crate) enum TransformGesture {
    Resize {
        image_id: String,
        handle: ResizeHandle,
        offset: Pos2,
        size: Vec2,
        scale: Vec2,
        natural_size: Vec2,
        rotation_degrees: f32,
        aspect_locked: bool,
    },
    Rotate {
        image_id: String,
        center: Pos2,
        start_pointer_degrees: f32,
        initial_rotation_degrees: f32,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ResizeHandle {
    NorthWest,
    North,
    NorthEast,
    East,
    SouthEast,
    South,
    SouthWest,
    West,
}

impl ResizeHandle {
    const ALL: [Self; 8] = [
        Self::NorthWest,
        Self::North,
        Self::NorthEast,
        Self::East,
        Self::SouthEast,
        Self::South,
        Self::SouthWest,
        Self::West,
    ];

    const fn signs(self) -> Vec2 {
        match self {
            Self::NorthWest => Vec2::new(-1.0, -1.0),
            Self::North => Vec2::new(0.0, -1.0),
            Self::NorthEast => Vec2::new(1.0, -1.0),
            Self::East => Vec2::new(1.0, 0.0),
            Self::SouthEast => Vec2::new(1.0, 1.0),
            Self::South => Vec2::new(0.0, 1.0),
            Self::SouthWest => Vec2::new(-1.0, 1.0),
            Self::West => Vec2::new(-1.0, 0.0),
        }
    }

    const fn id(self) -> &'static str {
        match self {
            Self::NorthWest => "north-west",
            Self::North => "north",
            Self::NorthEast => "north-east",
            Self::East => "east",
            Self::SouthEast => "south-east",
            Self::South => "south",
            Self::SouthWest => "south-west",
            Self::West => "west",
        }
    }

    const fn opposite(self) -> Self {
        match self {
            Self::NorthWest => Self::SouthEast,
            Self::North => Self::South,
            Self::NorthEast => Self::SouthWest,
            Self::East => Self::West,
            Self::SouthEast => Self::NorthWest,
            Self::South => Self::North,
            Self::SouthWest => Self::NorthEast,
            Self::West => Self::East,
        }
    }
}

/// Rebuild dense perforation/overcut preview geometry only when its source
/// paths or settings actually change. A compact fingerprint is still computed
/// each frame so direct node editing cannot leave stale cached geometry.
pub(crate) fn synchronize_cut_preview(state: &mut SapodillaApp) {
    let mut hasher = DefaultHasher::new();
    state.cut_shapes.len().hash(&mut hasher);
    for path in &state.cut_shapes {
        path.0.len().hash(&mut hasher);
        for point in &path.0 {
            point.x.to_bits().hash(&mut hasher);
            point.y.to_bits().hash(&mut hasher);
        }
    }
    for mode in &state.cut_modes {
        match mode {
            CutMode::Kiss => 0u8,
            CutMode::Perforation => 1,
            CutMode::Disabled => 2,
        }
        .hash(&mut hasher);
    }
    state.perf_cut.hash(&mut hasher);
    state.selected_device.hash(&mut hasher);
    state.perf_dash_mm.to_bits().hash(&mut hasher);
    state.perf_gap_mm.to_bits().hash(&mut hasher);
    state.peel_tabs.hash(&mut hasher);
    if state.peel_tabs {
        for position in &state.peel_tab_positions {
            position.map(f32::to_bits).hash(&mut hasher);
        }
    }
    state.overcut.enabled.hash(&mut hasher);
    state.overcut.steps.hash(&mut hasher);
    state
        .overcut
        .maximum_angle_degrees
        .to_bits()
        .hash(&mut hasher);
    state.overcut.reach_pixels.to_bits().hash(&mut hasher);
    state.overcut.snap_to_pixels.hash(&mut hasher);
    let key = hasher.finish();
    if state.cut_preview_cache_key == Some(key) {
        return;
    }

    let dpi = DEVICES[state.selected_device].dpi;
    let modes = effective_cut_modes(state.cut_shapes.len(), &state.cut_modes, state.perf_cut);
    let enabled = modes
        .iter()
        .map(|mode| *mode != CutMode::Disabled)
        .collect::<Vec<_>>();
    let tabs = if state.peel_tabs {
        build_peel_tabs(&state.cut_shapes, &enabled, &state.peel_tab_positions)
    } else {
        Vec::new()
    };
    let phases = preview_cut_phases(
        &state.cut_shapes,
        &state.cut_modes,
        state.perf_cut,
        state.perf_dash_mm * dpi / 25.4,
        state.perf_gap_mm * dpi / 25.4,
        state.overcut,
        &tabs,
    );
    state.cut_preview_stats =
        toolpath_stats_iter(phases.iter().flat_map(|phase| phase.paths.iter()));
    state.cut_preview_cache = phases;
    state.cut_preview_tabs = tabs;
    state.cut_preview_cache_key = Some(key);
}

pub fn canvas_editor(ui: &mut Ui, state: &mut SapodillaApp) -> Option<super::ArtworkMenuAction> {
    let scene = Scene::new().zoom_range(MIN_SCENE_SCALE..=MAX_SCENE_SCALE);
    let viewport_size = ui.available_size_before_wrap();

    let mut canvas_content_rect = Rect::NAN;
    let mut canvas_rect = state.canvas_rect;
    let mut artwork_action = None;

    let response = scene
        .show(ui, &mut canvas_rect, |ui| {
            let document = Frame::canvas(ui.style())
                .fill(state.background_color32())
                .inner_margin(0.0)
                .stroke(Stroke::new(CANVAS_BORDER_WIDTH, Color32::BLACK))
                .show(ui, |ui| frame(ui, state));
            let (action, ruler_bounds, content_rect) = document.inner;
            artwork_action = action;
            canvas_content_rect = content_rect;
            if let Some(ruler_bounds) = ruler_bounds {
                ui.expand_to_include_rect(ruler_bounds);
            }
        })
        .response;
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Panel, true, "Artwork canvas"));

    state.canvas_rect = canvas_rect;

    if response.double_clicked()
        || state.canvas_fit_requested
        || state.previous_canvas_size != state.get_canvas().size
    {
        state.canvas_rect = fitted_scene_rect(
            canvas_content_rect,
            viewport_size,
            state.show_rulers,
            ui.style().spacing.menu_spacing,
        );
        state.previous_canvas_size = state.get_canvas().size;
        state.canvas_fit_requested = false;
    }
    artwork_action
}

fn frame(
    ui: &mut Ui,
    state: &mut SapodillaApp,
) -> (Option<super::ArtworkMenuAction>, Option<Rect>, Rect) {
    let size = state.get_canvas().size;

    ui.set_min_size(size);
    ui.set_max_size(size);

    let (response, mut painter) = ui.allocate_painter(size, Sense::empty());
    let scene_painter = ui.painter().clone();

    let to_screen = emath::RectTransform::from_to(
        Rect::from_min_size(Pos2::ZERO, response.rect.size()),
        response.rect,
    );

    // Resolve the topmost hit before mutable painting. This makes a secondary
    // click on an unselected object activate that object before its menu opens.
    let secondary_target = if !state.edit_cutlines
        && ui.input(|input| input.pointer.secondary_clicked())
        && let Some(pointer) = ui.input(|input| input.pointer.interact_pos())
        && let Some(index) =
            state
                .loaded_images
                .iter()
                .enumerate()
                .rev()
                .find_map(|(index, image)| {
                    (image.visible
                        && Rect::from_min_size(
                            to_screen.transform_pos(image.visual_offset()),
                            image.rotated_size(),
                        )
                        .contains(pointer))
                    .then_some(index)
                }) {
        if !state.selected_images.contains(&index) {
            state.selected_images = vec![index];
        }
        Some(index)
    } else {
        None
    };
    let menu_context = super::ArtworkMenuContext::new(
        &state.loaded_images,
        &state.selected_images,
        DEVICES[state.selected_device].modes[state.selected_mode].mode_type,
    );
    let mut menu_action = None;

    let scene_scale = ui
        .ctx()
        .layer_transform_to_global(ui.layer_id())
        .map(|transform| transform.scaling)
        .unwrap_or(1.0)
        .max(0.001);
    let dpi = DEVICES[state.selected_device].dpi;
    if state.show_grid {
        paint_grid(
            &painter,
            response.rect,
            size,
            dpi,
            state.grid_spacing_mm,
            state.ruler_unit,
            scene_scale,
        );
    }

    let mut artwork_transform_changed = false;
    let transform_active_image_id = state
        .canvas_transform_gesture
        .as_ref()
        .map(transform_gesture_image_id)
        .map(str::to_owned);

    let mut hovers = Vec::new();
    let mut remove = None;
    let mut selection_translation = None;

    for (idx, image) in state.loaded_images.iter_mut().enumerate() {
        if !image.visible {
            continue;
        }
        let pos_in_screen = to_screen.transform_pos(image.visual_offset());
        let image_rect = Rect::from_min_size(pos_in_screen, image.rotated_size());

        let rect_id = response.id.with(("artwork", image.id.as_str()));
        let sense = if state.edit_cutlines {
            Sense::hover()
        } else {
            Sense::click_and_drag()
        };
        let rect_response = ui.interact(image_rect, rect_id, sense);
        let artwork_label = format!("Artwork: {}", image.name);
        rect_response
            .widget_info(|| WidgetInfo::labeled(WidgetType::Image, true, artwork_label.clone()));
        let mut popup = egui::Popup::context_menu(&rect_response);
        if secondary_target == Some(idx) {
            popup = popup.open_memory(egui::SetOpenCommand::Bool(true));
        }
        popup.show(|ui| {
            if let Some(context) = menu_context.as_ref()
                && let Some(action) = super::artwork_context_menu(ui, context)
            {
                menu_action = Some(action);
            }
        });

        if rect_response.clicked() {
            let command = ui.input(|i| i.modifiers.command || i.modifiers.shift);
            if command {
                if let Some(position) = state
                    .selected_images
                    .iter()
                    .position(|selected| *selected == idx)
                {
                    state.selected_images.remove(position);
                } else {
                    state.selected_images.push(idx);
                }
            } else {
                state.selected_images.clear();
                state.selected_images.push(idx);
            }
        }

        // Direct manipulation starts by selecting the object under the pointer,
        // rather than moving an unselected object while handles remain elsewhere.
        if rect_response.drag_started() && !state.selected_images.contains(&idx) {
            let additive = ui.input(|i| i.modifiers.command || i.modifiers.shift);
            if !additive {
                state.selected_images.clear();
            }
            state.selected_images.push(idx);
        }

        let handle_owns_gesture = transform_active_image_id.as_deref() == Some(&image.id);
        if !image.locked && !handle_owns_gesture && !state.edit_cutlines {
            if rect_response.drag_delta() != Vec2::ZERO {
                artwork_transform_changed = true;
            }
            let drag_delta = rect_response.drag_delta();
            let moving_selection =
                state.selected_images.contains(&idx) && state.selected_images.len() > 1;
            if moving_selection && drag_delta != Vec2::ZERO {
                selection_translation = Some(drag_delta);
            } else {
                image.offset += drag_delta;
            }
            if !moving_selection && state.snap_to_guides && rect_response.dragged() {
                let snap_tolerance = 6.0 / scene_scale;
                let visual = image.visual_offset();
                let visual_size = image.rotated_size();
                let targets_x = [0.0, size.x / 2.0, size.x];
                let targets_y = [0.0, size.y / 2.0, size.y];
                let anchors_x = [
                    visual.x,
                    visual.x + visual_size.x / 2.0,
                    visual.x + visual_size.x,
                ];
                let anchors_y = [
                    visual.y,
                    visual.y + visual_size.y / 2.0,
                    visual.y + visual_size.y,
                ];
                for anchor in anchors_x {
                    for target in targets_x {
                        if (anchor - target).abs() <= snap_tolerance {
                            image.offset.x += target - anchor;
                        }
                    }
                }
                for anchor in anchors_y {
                    for target in targets_y {
                        if (anchor - target).abs() <= snap_tolerance {
                            image.offset.y += target - anchor;
                        }
                    }
                }
            }
        }

        if rect_response.hovered() && !state.selected_images.contains(&idx) {
            hovers.push(image_rect);
        }
        if state.selected_images.contains(&idx)
            && state.selected_images.len() != 1
            && !hovers.contains(&image_rect)
        {
            hovers.push(image_rect);
        }

        if rect_response.hovered()
            && !image.locked
            && ui.input_mut(|i| {
                i.consume_shortcut(&DELETE_SHORTCUT) || i.consume_shortcut(&BACKSPACE_SHORTCUT)
            })
        {
            remove = Some(idx);
        } else {
            paint_rotated_image(
                &mut painter,
                image.sized_texture.id,
                to_screen.transform_pos(image.offset),
                image.size(),
                image.rotation_degrees,
            );
        }
    }

    if let Some(delta) = selection_translation {
        for &selected in &state.selected_images {
            if let Some(image) = state.loaded_images.get_mut(selected)
                && image.visible
                && !image.locked
            {
                image.offset += delta;
            }
        }
        artwork_transform_changed = true;
    }

    artwork_transform_changed |=
        interact_transform_handles(ui, state, response.id, &to_screen, scene_scale);
    if artwork_transform_changed {
        state.synchronize_cut_geometry();
    }

    if state.show_cutlines {
        for phase in &state.cut_preview_cache {
            let stroke = match phase.mode {
                CutMode::Kiss => Stroke::new(CUT_LINE_WIDTH, Color32::from_rgb(67, 170, 139)),
                CutMode::Perforation => Stroke::new(CUT_LINE_WIDTH, Color32::from_rgb(249, 65, 68)),
                CutMode::Disabled => continue,
            };
            paint_polygons(&to_screen, &painter, &phase.paths, stroke);
        }
    }
    if state.peel_tabs && state.show_cutlines {
        for (path_index, tab) in &state.cut_preview_tabs {
            let path_index = *path_index;
            let path = &state.cut_shapes[path_index];
            let locked = state
                .cutline_locked
                .get(path_index)
                .copied()
                .unwrap_or(false);
            let handle_position = to_screen.transform_pos(Pos2::new(tab.handle.x, tab.handle.y));
            let (handle, dragged_position) = interact_peel_tab_handle(
                ui,
                response.id.with(("peel-tab", path_index)),
                handle_position,
                24.0 / scene_scale.max(0.1),
                path,
                &to_screen,
                !locked,
            );
            let handle = if locked {
                handle.on_hover_text("This cutline is locked")
            } else {
                handle
                    .on_hover_cursor(CursorIcon::Grab)
                    .on_hover_text("Drag around the cutline to place the peel tab")
            };
            handle.widget_info(|| {
                WidgetInfo::labeled(
                    WidgetType::DragValue,
                    !locked,
                    format!("Peel tab for path {}", path_index + 1),
                )
            });
            if !locked && (handle.clicked() || handle.drag_started()) {
                state.selected_cut_path = Some(path_index);
                state.selected_cut_node = None;
            }
            if !locked
                && let Some(position) = dragged_position
                && let Some(stored) = state.peel_tab_positions.get_mut(path_index)
            {
                *stored = Some(position);
                state.cut_preview_cache_key = None;
            }
            painter.circle_filled(
                handle_position,
                6.0 / scene_scale.max(0.1),
                Color32::from_rgb(255, 196, 64),
            );
            painter.circle_stroke(
                handle_position,
                6.0 / scene_scale.max(0.1),
                Stroke::new(1.5 / scene_scale.max(0.1), Color32::BLACK),
            );
        }
    }
    if state.edit_cutlines
        && let Some(path_index) = state.selected_cut_path
        && !state
            .cutline_locked
            .get(path_index)
            .copied()
            .unwrap_or(false)
        && let Some(path) = state.cut_shapes.get_mut(path_index)
    {
        let was_closed = path.0.first() == path.0.last();
        for (node_index, point) in path.0.iter_mut().enumerate() {
            let position = to_screen.transform_pos(Pos2::new(point.x, point.y));
            let rect = Rect::from_center_size(position, Vec2::splat(12.0));
            let node = ui.interact(
                rect,
                response.id.with(("cut-node", path_index, node_index)),
                Sense::drag(),
            );
            if node.clicked() {
                state.selected_cut_node = Some(node_index);
            }
            if node.dragged() {
                point.x += node.drag_delta().x;
                point.y += node.drag_delta().y;
            }
            painter.circle_filled(position, 5.0, Color32::from_rgb(255, 78, 145));
            painter.circle_stroke(position, 5.0, Stroke::new(1.5_f32, Color32::WHITE));
        }
        if was_closed && path.0.len() > 1 {
            let first = path.0[0];
            *path.0.last_mut().unwrap() = first;
        }
    }

    let safe_area = DEVICES[state.selected_device].modes[state.selected_mode].canvas_sizes
        [state.selected_canvas_size]
        .safe_area;

    if state.show_safe_area && safe_area != size {
        let safe_lines = Rect::from_center_size((size / 2.0).to_pos2(), safe_area);

        painter.rect_stroke(
            to_screen.transform_rect(safe_lines),
            0,
            Stroke::new(5.0_f32, Color32::from_rgba_unmultiplied(139, 0, 0, 128)),
            egui::StrokeKind::Outside,
        );
    }

    painter.set_clip_rect(ui.clip_rect());

    let stroke = Stroke::new(5.0_f32, Color32::from_rgba_unmultiplied(173, 216, 230, 192));
    for rect in hovers {
        painter.rect_stroke(rect, 0, stroke, egui::StrokeKind::Outside);
    }

    paint_transform_controls(&painter, state, &to_screen, scene_scale);

    let ruler_bounds = if state.show_rulers {
        let layout = RulerLayout::outside(response.rect, scene_scale);
        paint_rulers(
            &scene_painter,
            layout,
            ui.clip_rect(),
            size,
            dpi,
            state.grid_spacing_mm,
            state.ruler_unit,
            scene_scale,
        );
        Some(layout.bounds())
    } else {
        None
    };

    if let Some(remove) = remove {
        let context = super::ArtworkMenuContext::single(
            &state.loaded_images[remove],
            remove,
            state.loaded_images.len(),
            DEVICES[state.selected_device].modes[state.selected_mode].mode_type,
        );
        menu_action = Some(context.action(super::ArtworkMenuCommand::Remove));
    }
    (menu_action, ruler_bounds, response.rect)
}

fn interact_peel_tab_handle(
    ui: &mut Ui,
    id: Id,
    handle_position: Pos2,
    hit_size: f32,
    path: &LineString<f32>,
    to_screen: &RectTransform,
    enabled: bool,
) -> (Response, Option<f32>) {
    let response = ui.interact(
        Rect::from_center_size(handle_position, Vec2::splat(hit_size)),
        id,
        if enabled {
            Sense::drag()
        } else {
            Sense::hover()
        },
    );
    let position = response
        .dragged()
        .then(|| response.interact_pointer_pos())
        .flatten()
        .and_then(|pointer| {
            nearest_perimeter_position(path, to_screen.inverse().transform_pos(pointer))
        });
    (response, position)
}

fn transform_gesture_image_id(gesture: &TransformGesture) -> &str {
    match gesture {
        TransformGesture::Resize { image_id, .. } | TransformGesture::Rotate { image_id, .. } => {
            image_id
        }
    }
}

fn canceled_transform(
    gesture: &TransformGesture,
    current_offset: Pos2,
    current_scale: Vec2,
) -> (Pos2, Vec2, f32) {
    match gesture {
        TransformGesture::Resize {
            offset,
            scale,
            rotation_degrees,
            ..
        } => (*offset, *scale, *rotation_degrees),
        TransformGesture::Rotate {
            initial_rotation_degrees,
            ..
        } => (current_offset, current_scale, *initial_rotation_degrees),
    }
}

fn rotate_vector(vector: Vec2, degrees: f32) -> Vec2 {
    let (sin, cos) = degrees.to_radians().sin_cos();
    Vec2::new(
        cos * vector.x - sin * vector.y,
        sin * vector.x + cos * vector.y,
    )
}

fn oriented_corners(offset: Pos2, size: Vec2, degrees: f32) -> [Pos2; 4] {
    let center = offset + size / 2.0;
    [
        Vec2::new(-size.x / 2.0, -size.y / 2.0),
        Vec2::new(size.x / 2.0, -size.y / 2.0),
        Vec2::new(size.x / 2.0, size.y / 2.0),
        Vec2::new(-size.x / 2.0, size.y / 2.0),
    ]
    .map(|relative| center + rotate_vector(relative, degrees))
}

fn rotation_handle_position(offset: Pos2, size: Vec2, degrees: f32, distance: f32) -> Pos2 {
    let center = offset + size / 2.0;
    center + rotate_vector(Vec2::new(0.0, -size.y / 2.0 - distance), degrees)
}

fn resize_handle_position(offset: Pos2, size: Vec2, degrees: f32, handle: ResizeHandle) -> Pos2 {
    let center = offset + size / 2.0;
    let signs = handle.signs();
    center
        + rotate_vector(
            Vec2::new(signs.x * size.x / 2.0, signs.y * size.y / 2.0),
            degrees,
        )
}

fn resize_cursor(handle: ResizeHandle, rotation_degrees: f32) -> CursorIcon {
    let direction = rotate_vector(handle.signs(), rotation_degrees);
    let octant =
        ((direction.y.atan2(direction.x).to_degrees() / 45.0).round() as i32).rem_euclid(4);
    match octant {
        0 => CursorIcon::ResizeHorizontal,
        1 => CursorIcon::ResizeNwSe,
        2 => CursorIcon::ResizeVertical,
        _ => CursorIcon::ResizeNeSw,
    }
}

fn normalize_degrees(degrees: f32) -> f32 {
    (degrees + 180.0).rem_euclid(360.0) - 180.0
}

fn pointer_rotation_degrees(center: Pos2, pointer: Pos2) -> f32 {
    let delta = pointer - center;
    normalize_degrees(delta.y.atan2(delta.x).to_degrees() + 90.0)
}

fn rotation_from_gesture(
    initial_rotation_degrees: f32,
    start_pointer_degrees: f32,
    center: Pos2,
    pointer: Pos2,
    snap: bool,
) -> f32 {
    let delta =
        normalize_degrees(pointer_rotation_degrees(center, pointer) - start_pointer_degrees);
    let degrees = normalize_degrees(initial_rotation_degrees + delta);
    if snap {
        normalize_degrees((degrees / 15.0).round() * 15.0)
    } else {
        degrees
    }
}

#[allow(clippy::too_many_arguments)]
fn resize_from_handle(
    offset: Pos2,
    size: Vec2,
    scale: Vec2,
    natural_size: Vec2,
    rotation_degrees: f32,
    handle: ResizeHandle,
    pointer: Pos2,
    aspect_locked: bool,
    from_center: bool,
) -> Option<(Pos2, Vec2)> {
    if !size.x.is_finite()
        || !size.y.is_finite()
        || size.x <= 0.0
        || size.y <= 0.0
        || natural_size.x <= 0.0
        || natural_size.y <= 0.0
    {
        return None;
    }
    let sign = handle.signs();
    let original_center = offset + size / 2.0;
    let anchor = if from_center {
        original_center
    } else {
        resize_handle_position(offset, size, rotation_degrees, handle.opposite())
    };
    let local_delta = rotate_vector(pointer - anchor, -rotation_degrees);
    let center_multiplier = if from_center { 2.0 } else { 1.0 };

    let (new_size, new_scale) = if aspect_locked {
        let reference = if sign.x != 0.0 && sign.y != 0.0 {
            Vec2::new(
                sign.x * size.x / center_multiplier,
                sign.y * size.y / center_multiplier,
            )
        } else if sign.x != 0.0 {
            Vec2::new(sign.x * size.x / center_multiplier, 0.0)
        } else {
            Vec2::new(0.0, sign.y * size.y / center_multiplier)
        };
        let denominator = reference.length_sq().max(f32::EPSILON);
        let minimum = (MIN_IMAGE_SIZE / size.x)
            .max(MIN_IMAGE_SIZE / size.y)
            .max(f32::EPSILON);
        let maximum = (MAX_IMAGE_SCALE / scale.x.abs().max(f32::EPSILON))
            .min(MAX_IMAGE_SCALE / scale.y.abs().max(f32::EPSILON));
        let requested_factor = local_delta.dot(reference) / denominator;
        let factor = if minimum <= maximum {
            requested_factor.clamp(minimum, maximum)
        } else {
            // A persisted non-uniform scale can make the minimum-size and
            // maximum-scale constraints mutually exclusive. Prefer a finite,
            // positive image over panicking while the user repairs the scale.
            minimum
        };
        (size * factor, scale * factor)
    } else {
        let width = if sign.x == 0.0 {
            size.x
        } else {
            (center_multiplier * sign.x * local_delta.x)
                .clamp(MIN_IMAGE_SIZE, natural_size.x * MAX_IMAGE_SCALE)
        };
        let height = if sign.y == 0.0 {
            size.y
        } else {
            (center_multiplier * sign.y * local_delta.y)
                .clamp(MIN_IMAGE_SIZE, natural_size.y * MAX_IMAGE_SCALE)
        };
        (
            Vec2::new(width, height),
            Vec2::new(width / natural_size.x, height / natural_size.y),
        )
    };

    if !new_size.is_finite() || !new_scale.is_finite() {
        return None;
    }
    let center = if from_center {
        original_center
    } else {
        anchor
            + rotate_vector(
                Vec2::new(sign.x * new_size.x / 2.0, sign.y * new_size.y / 2.0),
                rotation_degrees,
            )
    };
    Some((center - new_size / 2.0, new_scale))
}

fn interact_transform_handles(
    ui: &mut Ui,
    state: &mut SapodillaApp,
    canvas_id: Id,
    to_screen: &RectTransform,
    scene_scale: f32,
) -> bool {
    let Some(&index) = state.selected_images.as_slice().first() else {
        state.canvas_transform_gesture = None;
        return false;
    };
    if state.selected_images.len() != 1 || state.edit_cutlines {
        state.canvas_transform_gesture = None;
        return false;
    }
    let Some(image) = state.loaded_images.get(index) else {
        state.canvas_transform_gesture = None;
        return false;
    };
    if !image.visible || image.locked {
        state.canvas_transform_gesture = None;
        return false;
    }

    let image_id = image.id.clone();
    let offset = image.offset;
    let size = image.size();
    let scale = image.scale;
    let natural_size = image.sized_texture.size;
    let rotation_degrees = image.rotation_degrees;
    let aspect_locked = image.scale_locked;
    let hit_size = TRANSFORM_HIT_SIZE / scene_scale;
    let rotation_distance = ROTATION_HANDLE_OFFSET / scene_scale;
    let rotate_handle = rotation_handle_position(offset, size, rotation_degrees, rotation_distance);
    let mut gesture = state.canvas_transform_gesture.take();
    let mut changed = false;
    let mut pending_resize = None;
    let mut pending_rotation = None;

    if let Some(active_gesture) = gesture.as_ref()
        && ui.input_mut(|input| input.consume_key(Modifiers::NONE, Key::Escape))
    {
        if let Some(image) = state.loaded_images.get_mut(index) {
            let (offset, scale, rotation) =
                canceled_transform(active_gesture, image.offset, image.scale);
            changed = image.offset != offset
                || image.scale != scale
                || image.rotation_degrees != rotation;
            image.offset = offset;
            image.scale = scale;
            image.rotation_degrees = rotation;
        }
        state.canvas_transform_gesture = None;
        return changed;
    }

    for handle in ResizeHandle::ALL {
        let position = to_screen.transform_pos(resize_handle_position(
            offset,
            size,
            rotation_degrees,
            handle,
        ));
        let response = ui
            .interact(
                Rect::from_center_size(position, Vec2::splat(hit_size)),
                canvas_id.with(("resize", image_id.as_str(), handle.id())),
                Sense::drag(),
            )
            .on_hover_cursor(resize_cursor(handle, rotation_degrees))
            .on_hover_text(
                "Drag to resize · Shift toggles proportions · Alt/Option scales from center",
            );
        if response.drag_started() {
            gesture = Some(TransformGesture::Resize {
                image_id: image_id.clone(),
                handle,
                offset,
                size,
                scale,
                natural_size,
                rotation_degrees,
                aspect_locked,
            });
        }
        if response.dragged()
            && let Some(pointer) = response.interact_pointer_pos()
            && let Some(TransformGesture::Resize {
                image_id: active_id,
                handle,
                offset,
                size,
                scale,
                natural_size,
                rotation_degrees,
                aspect_locked,
            }) = gesture.as_ref()
            && active_id == &image_id
        {
            let pointer = to_screen.inverse().transform_pos(pointer);
            let (invert_lock, from_center) =
                ui.input(|input| (input.modifiers.shift, input.modifiers.alt));
            pending_resize = resize_from_handle(
                *offset,
                *size,
                *scale,
                *natural_size,
                *rotation_degrees,
                *handle,
                pointer,
                *aspect_locked ^ invert_lock,
                from_center,
            );
        }
    }

    let mut rotation_responses = Vec::with_capacity(5);
    for handle in [
        ResizeHandle::NorthWest,
        ResizeHandle::NorthEast,
        ResizeHandle::SouthEast,
        ResizeHandle::SouthWest,
    ] {
        let signs = handle.signs().normalized();
        let corner = resize_handle_position(offset, size, rotation_degrees, handle);
        let zone = corner
            + rotate_vector(
                signs * (CORNER_ROTATION_ZONE_OFFSET / scene_scale),
                rotation_degrees,
            );
        rotation_responses.push(
            ui.interact(
                Rect::from_center_size(to_screen.transform_pos(zone), Vec2::splat(hit_size)),
                canvas_id.with(("rotate-corner", image_id.as_str(), handle.id())),
                Sense::drag(),
            )
            .on_hover_cursor(CursorIcon::Grab)
            .on_hover_text("Drag outside a corner to rotate · Hold Shift to snap to 15°"),
        );
    }
    rotation_responses.push(
        ui.interact(
            Rect::from_center_size(
                to_screen.transform_pos(rotate_handle),
                Vec2::splat(hit_size),
            ),
            canvas_id.with(("rotate", image_id.as_str())),
            Sense::drag(),
        )
        .on_hover_cursor(CursorIcon::Grab)
        .on_hover_text("Drag to rotate · Hold Shift to snap to 15°"),
    );
    for rotate_response in rotation_responses {
        if rotate_response.drag_started()
            && let Some(pointer) = rotate_response.interact_pointer_pos()
        {
            let pointer = to_screen.inverse().transform_pos(pointer);
            let center = offset + size / 2.0;
            gesture = Some(TransformGesture::Rotate {
                image_id: image_id.clone(),
                center,
                start_pointer_degrees: pointer_rotation_degrees(center, pointer),
                initial_rotation_degrees: rotation_degrees,
            });
        }
        if rotate_response.dragged()
            && let Some(pointer) = rotate_response.interact_pointer_pos()
            && let Some(TransformGesture::Rotate {
                image_id: active_id,
                center,
                start_pointer_degrees,
                initial_rotation_degrees,
            }) = gesture.as_ref()
            && active_id == &image_id
        {
            let pointer = to_screen.inverse().transform_pos(pointer);
            pending_rotation = Some(rotation_from_gesture(
                *initial_rotation_degrees,
                *start_pointer_degrees,
                *center,
                pointer,
                ui.input(|input| input.modifiers.shift),
            ));
        }
    }

    if let Some(image) = state.loaded_images.get_mut(index) {
        if let Some((new_offset, new_scale)) = pending_resize {
            changed = image.offset != new_offset || image.scale != new_scale;
            image.offset = new_offset;
            image.scale = new_scale;
        }
        if let Some(new_rotation) = pending_rotation {
            changed |= image.rotation_degrees != new_rotation;
            image.rotation_degrees = new_rotation;
        }
    }

    if !ui.input(|input| input.pointer.primary_down()) {
        gesture = None;
    }
    state.canvas_transform_gesture = gesture;
    changed
}

fn paint_transform_controls(
    painter: &Painter,
    state: &SapodillaApp,
    to_screen: &RectTransform,
    scene_scale: f32,
) {
    let [index] = state.selected_images.as_slice() else {
        return;
    };
    let Some(image) = state.loaded_images.get(*index) else {
        return;
    };
    if !image.visible {
        return;
    }

    let color = if image.locked {
        Color32::from_rgb(130, 150, 165)
    } else {
        Color32::from_rgb(25, 145, 235)
    };
    let corners = oriented_corners(image.offset, image.size(), image.rotation_degrees)
        .map(|position| to_screen.transform_pos(position));
    let mut outline = corners.to_vec();
    outline.push(corners[0]);
    painter.add(Shape::line(
        outline.clone(),
        Stroke::new(3.5 / scene_scale, Color32::WHITE),
    ));
    painter.add(Shape::line(outline, Stroke::new(1.5 / scene_scale, color)));
    if image.locked || state.edit_cutlines {
        return;
    }

    let visual_size = TRANSFORM_HANDLE_SIZE / scene_scale;
    for handle in ResizeHandle::ALL {
        let position = to_screen.transform_pos(resize_handle_position(
            image.offset,
            image.size(),
            image.rotation_degrees,
            handle,
        ));
        painter.rect_filled(
            Rect::from_center_size(position, Vec2::splat(visual_size)),
            1.5 / scene_scale,
            Color32::WHITE,
        );
        painter.rect_stroke(
            Rect::from_center_size(position, Vec2::splat(visual_size)),
            1.5 / scene_scale,
            Stroke::new(1.5 / scene_scale, color),
            egui::StrokeKind::Inside,
        );
    }
    let top_middle = corners[0].lerp(corners[1], 0.5);
    let rotate_handle = to_screen.transform_pos(rotation_handle_position(
        image.offset,
        image.size(),
        image.rotation_degrees,
        ROTATION_HANDLE_OFFSET / scene_scale,
    ));
    painter.line_segment(
        [top_middle, rotate_handle],
        Stroke::new(1.5 / scene_scale, color),
    );
    painter.circle_filled(rotate_handle, visual_size / 2.0, Color32::WHITE);
    painter.circle_stroke(
        rotate_handle,
        visual_size / 2.0,
        Stroke::new(1.5 / scene_scale, color),
    );
    painter.text(
        rotate_handle,
        Align2::CENTER_CENTER,
        "↻",
        FontId::proportional(8.0 / scene_scale),
        color,
    );

    let center = to_screen.transform_pos(image.offset + image.size() / 2.0);
    let pivot_radius = 4.0 / scene_scale;
    painter.line_segment(
        [
            center - Vec2::new(pivot_radius, 0.0),
            center + Vec2::new(pivot_radius, 0.0),
        ],
        Stroke::new(1.0 / scene_scale, color),
    );
    painter.line_segment(
        [
            center - Vec2::new(0.0, pivot_radius),
            center + Vec2::new(0.0, pivot_radius),
        ],
        Stroke::new(1.0 / scene_scale, color),
    );

    paint_transform_feedback(painter, state, image, corners, scene_scale);
}

fn paint_transform_feedback(
    painter: &Painter,
    state: &SapodillaApp,
    image: &crate::app::LoadedImage,
    corners: [Pos2; 4],
    scene_scale: f32,
) {
    let Some(gesture) = state.canvas_transform_gesture.as_ref() else {
        return;
    };
    if transform_gesture_image_id(gesture) != image.id {
        return;
    }

    let dpi = DEVICES[state.selected_device].dpi.max(f32::EPSILON);
    let size_mm = image.size() / dpi * 25.4;
    let label = match gesture {
        TransformGesture::Resize { .. } => {
            format!("W {:.1} mm  ·  H {:.1} mm", size_mm.x, size_mm.y)
        }
        TransformGesture::Rotate { .. } => {
            format!("Rotation {:.1}°", normalize_degrees(image.rotation_degrees))
        }
    };

    let font = FontId::proportional(12.0 / scene_scale);
    let text_color = Color32::WHITE;
    let galley = painter.layout_no_wrap(label, font, text_color);
    let padding = Vec2::new(9.0, 6.0) / scene_scale;
    let tooltip_size = galley.size() + padding * 2.0;
    let min_x = corners
        .iter()
        .map(|point| point.x)
        .fold(f32::INFINITY, f32::min);
    let max_x = corners
        .iter()
        .map(|point| point.x)
        .fold(f32::NEG_INFINITY, f32::max);
    let min_y = corners
        .iter()
        .map(|point| point.y)
        .fold(f32::INFINITY, f32::min);
    let max_y = corners
        .iter()
        .map(|point| point.y)
        .fold(f32::NEG_INFINITY, f32::max);
    let gap = 14.0 / scene_scale;
    let min_left = painter.clip_rect().left();
    let max_left = (painter.clip_rect().right() - tooltip_size.x).max(min_left);
    let centered_x = ((min_x + max_x) / 2.0 - tooltip_size.x / 2.0).clamp(min_left, max_left);
    let below_y = max_y + gap;
    let top = if below_y + tooltip_size.y <= painter.clip_rect().bottom() {
        below_y
    } else {
        min_y - gap - tooltip_size.y
    };
    let tooltip = Rect::from_min_size(Pos2::new(centered_x, top), tooltip_size);
    painter.rect(
        tooltip,
        7.0 / scene_scale,
        Color32::from_rgba_unmultiplied(11, 11, 16, 235),
        Stroke::new(1.0 / scene_scale, Color32::from_rgb(39, 221, 235)),
        egui::StrokeKind::Outside,
    );
    painter.galley(tooltip.min + padding, galley, text_color);
}

fn nice_step_ceiling(value: f32) -> f32 {
    if !value.is_finite() || value <= 0.0 {
        return 1.0;
    }
    let magnitude = 10.0_f32.powf(value.log10().floor());
    let normalized = value / magnitude;
    let nice = if normalized <= 1.0 {
        1.0
    } else if normalized <= 2.0 {
        2.0
    } else if normalized <= 5.0 {
        5.0
    } else {
        10.0
    };
    nice * magnitude
}

fn adaptive_grid_step(
    requested: f32,
    pixels_per_unit: f32,
    scene_scale: f32,
    extent_pixels: f32,
) -> f32 {
    let base = requested.max(f32::EPSILON);
    let screen_requirement = MIN_GRID_SCREEN_SPACING / (pixels_per_unit * scene_scale).max(0.001);
    let count_requirement = extent_pixels / pixels_per_unit / MAX_GRID_LINES_PER_AXIS as f32;
    let required = screen_requirement.max(count_requirement);
    if required <= base {
        base
    } else {
        base * nice_step_ceiling(required / base)
    }
}

fn tick_positions(extent_pixels: f32, step_pixels: f32) -> Vec<f32> {
    if !extent_pixels.is_finite()
        || !step_pixels.is_finite()
        || extent_pixels < 0.0
        || step_pixels <= 0.0
    {
        return Vec::new();
    }
    let count = (extent_pixels / step_pixels).floor() as usize;
    (0..=count)
        .take(MAX_GRID_LINES_PER_AXIS)
        .map(|index| index as f32 * step_pixels)
        .collect()
}

fn paint_grid(
    painter: &Painter,
    canvas_rect: Rect,
    canvas_size: Vec2,
    dpi: f32,
    requested_mm: f32,
    unit: CanvasUnit,
    scene_scale: f32,
) {
    let pixels_per_unit = unit.pixels_per_unit(dpi);
    let step = adaptive_grid_step(
        unit.from_mm(requested_mm, dpi),
        pixels_per_unit,
        scene_scale,
        canvas_size.x.max(canvas_size.y),
    );
    let step_pixels = step * pixels_per_unit;
    let minor = Stroke::new(
        0.75 / scene_scale,
        Color32::from_rgba_unmultiplied(80, 96, 112, 42),
    );
    let major = Stroke::new(
        1.25 / scene_scale,
        Color32::from_rgba_unmultiplied(60, 80, 100, 78),
    );
    let clipped = painter.with_clip_rect(canvas_rect);
    for (index, x) in tick_positions(canvas_size.x, step_pixels)
        .into_iter()
        .enumerate()
    {
        let stroke = if index % 5 == 0 { major } else { minor };
        let x = canvas_rect.left() + x;
        clipped.line_segment(
            [
                Pos2::new(x, canvas_rect.top()),
                Pos2::new(x, canvas_rect.bottom()),
            ],
            stroke,
        );
    }
    for (index, y) in tick_positions(canvas_size.y, step_pixels)
        .into_iter()
        .enumerate()
    {
        let stroke = if index % 5 == 0 { major } else { minor };
        let y = canvas_rect.top() + y;
        clipped.line_segment(
            [
                Pos2::new(canvas_rect.left(), y),
                Pos2::new(canvas_rect.right(), y),
            ],
            stroke,
        );
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct RulerLayout {
    canvas: Rect,
    top: Rect,
    left: Rect,
    corner: Rect,
}

fn fitted_scene_rect(
    canvas_rect: Rect,
    viewport_size: Vec2,
    show_rulers: bool,
    padding_screen: f32,
) -> Rect {
    let document_bounds = canvas_rect.expand(CANVAS_BORDER_WIDTH);
    let ruler_screen = if show_rulers { RULER_SIZE } else { 0.0 };
    let reserved_screen = ruler_screen + padding_screen.max(0.0) * 2.0;
    let available = Vec2::new(
        (viewport_size.x - reserved_screen).max(1.0),
        (viewport_size.y - reserved_screen).max(1.0),
    );
    let scale = (available / document_bounds.size())
        .min_elem()
        .clamp(MIN_SCENE_SCALE, MAX_SCENE_SCALE);
    let ruler_scene = ruler_screen / scale;
    let padding_scene = padding_screen.max(0.0) / scale;
    Rect::from_min_max(
        document_bounds.min - Vec2::splat(ruler_scene),
        document_bounds.max,
    )
    .expand(padding_scene)
}

impl RulerLayout {
    fn outside(canvas_rect: Rect, scene_scale: f32) -> Self {
        let band = RULER_SIZE / scene_scale.max(0.001);
        let document_edge = canvas_rect.expand(CANVAS_BORDER_WIDTH);
        Self {
            canvas: canvas_rect,
            top: Rect::from_min_max(
                Pos2::new(document_edge.left(), document_edge.top() - band),
                document_edge.right_top(),
            ),
            left: Rect::from_min_max(
                Pos2::new(document_edge.left() - band, document_edge.top()),
                document_edge.left_bottom(),
            ),
            corner: Rect::from_min_max(document_edge.min - Vec2::splat(band), document_edge.min),
        }
    }

    fn bounds(self) -> Rect {
        self.top.union(self.left).union(self.corner)
    }
}

fn paint_rulers(
    painter: &Painter,
    layout: RulerLayout,
    viewport_rect: Rect,
    canvas_size: Vec2,
    dpi: f32,
    requested_mm: f32,
    unit: CanvasUnit,
    scene_scale: f32,
) {
    let canvas_rect = layout.canvas;
    if !layout.bounds().intersects(viewport_rect) {
        return;
    }

    let pixels_per_unit = unit.pixels_per_unit(dpi);
    let step = adaptive_grid_step(
        unit.from_mm(requested_mm, dpi),
        pixels_per_unit,
        scene_scale,
        canvas_size.x.max(canvas_size.y),
    );
    let step_pixels = step * pixels_per_unit;
    let band = layout.top.height();
    let stroke_width = 1.0 / scene_scale;
    let overlay = painter.with_clip_rect(viewport_rect);
    let fill = Color32::from_rgba_unmultiplied(245, 248, 250, 232);
    overlay.rect_filled(layout.top, 0.0, fill);
    overlay.rect_filled(layout.left, 0.0, fill);
    overlay.line_segment(
        [layout.top.left_bottom(), layout.top.right_bottom()],
        Stroke::new(stroke_width, Color32::from_gray(105)),
    );
    overlay.line_segment(
        [layout.left.right_top(), layout.left.right_bottom()],
        Stroke::new(stroke_width, Color32::from_gray(105)),
    );

    let font = FontId::monospace(9.0 / scene_scale);
    let tick_color = Color32::from_gray(75);
    let visible_left = viewport_rect.left().max(canvas_rect.left());
    let visible_right = viewport_rect.right().min(canvas_rect.right());
    let first_x = (((visible_left - canvas_rect.left()) / step_pixels).floor() as isize).max(0);
    let last_x = (((visible_right - canvas_rect.left()) / step_pixels).ceil() as usize)
        .min(MAX_GRID_LINES_PER_AXIS.saturating_sub(1));
    for index in first_x as usize..=last_x {
        let x = canvas_rect.left() + index as f32 * step_pixels;
        if x > canvas_rect.right() || visible_left > visible_right {
            break;
        }
        let major = index % 5 == 0;
        let tick = if major { band * 0.5 } else { band * 0.25 };
        overlay.line_segment(
            [
                Pos2::new(x, layout.top.bottom()),
                Pos2::new(x, layout.top.bottom() - tick),
            ],
            Stroke::new(stroke_width, tick_color),
        );
        if major {
            overlay.text(
                Pos2::new(x + 2.0 / scene_scale, layout.top.top() + 2.0 / scene_scale),
                Align2::LEFT_TOP,
                format_tick(index as f32 * step, unit),
                font.clone(),
                tick_color,
            );
        }
    }

    let visible_top = viewport_rect.top().max(canvas_rect.top());
    let visible_bottom = viewport_rect.bottom().min(canvas_rect.bottom());
    let first_y = (((visible_top - canvas_rect.top()) / step_pixels).floor() as isize).max(0);
    let last_y = (((visible_bottom - canvas_rect.top()) / step_pixels).ceil() as usize)
        .min(MAX_GRID_LINES_PER_AXIS.saturating_sub(1));
    for index in first_y as usize..=last_y {
        let y = canvas_rect.top() + index as f32 * step_pixels;
        if y > canvas_rect.bottom() || visible_top > visible_bottom {
            break;
        }
        let major = index % 5 == 0;
        let tick = if major { band * 0.5 } else { band * 0.25 };
        overlay.line_segment(
            [
                Pos2::new(layout.left.right(), y),
                Pos2::new(layout.left.right() - tick, y),
            ],
            Stroke::new(stroke_width, tick_color),
        );
        if major {
            overlay.text(
                Pos2::new(
                    layout.left.left() + 2.0 / scene_scale,
                    y + 2.0 / scene_scale,
                ),
                Align2::LEFT_TOP,
                format_tick(index as f32 * step, unit),
                font.clone(),
                tick_color,
            );
        }
    }
    overlay.rect_filled(layout.corner, 0.0, Color32::from_gray(225));
    overlay.text(
        layout.corner.center(),
        Align2::CENTER_CENTER,
        unit.label(),
        font,
        tick_color,
    );
}

fn format_tick(value: f32, unit: CanvasUnit) -> String {
    if value.fract().abs() < 0.001 {
        format!("{value:.0}")
    } else if unit == CanvasUnit::In {
        format!("{value:.2}")
    } else {
        format!("{value:.1}")
    }
}

fn paint_rotated_image(
    painter: &mut Painter,
    texture_id: egui::TextureId,
    top_left: Pos2,
    size: Vec2,
    degrees: f32,
) {
    if degrees.abs() < 0.001 {
        painter.image(
            texture_id,
            Rect::from_min_size(top_left, size),
            NORMAL_UV,
            Color32::WHITE,
        );
        return;
    }
    let center = top_left + size / 2.0;
    let radians = degrees.to_radians();
    let (sin, cos) = radians.sin_cos();
    let rotate = |point: Pos2| {
        let delta = point - center;
        center + Vec2::new(cos * delta.x - sin * delta.y, sin * delta.x + cos * delta.y)
    };
    let positions = [
        rotate(top_left),
        rotate(top_left + Vec2::new(size.x, 0.0)),
        rotate(top_left + size),
        rotate(top_left + Vec2::new(0.0, size.y)),
    ];
    let uvs = [
        Pos2::new(0.0, 0.0),
        Pos2::new(1.0, 0.0),
        Pos2::new(1.0, 1.0),
        Pos2::new(0.0, 1.0),
    ];
    let mut mesh = egui::Mesh::with_texture(texture_id);
    for (pos, uv) in positions.into_iter().zip(uvs) {
        mesh.vertices.push(egui::epaint::Vertex {
            pos,
            uv,
            color: Color32::WHITE,
        });
    }
    mesh.indices.extend_from_slice(&[0, 1, 2, 0, 2, 3]);
    painter.add(Shape::mesh(mesh));
}

#[instrument(skip_all)]
fn paint_polygons(
    to_screen: &RectTransform,
    painter: &Painter,
    cut_shapes: &[LineString<f32>],
    stroke: Stroke,
) {
    let mut shapes = Vec::with_capacity(cut_shapes.len());
    for line_string in cut_shapes {
        if line_string.0.len() < 2 {
            continue;
        }
        // A contour is one meshable line shape. Creating a separate two-point
        // allocation for every segment made dense generated paths expensive to
        // paint and multiplied egui's shape-processing overhead every frame.
        let points = line_string
            .0
            .iter()
            .map(|point| to_screen.transform_pos(Pos2::new(point.x, point.y)))
            .collect();
        shapes.push(Shape::line(points, stroke));
    }
    // Extending once avoids locking the painter for every perforation dash.
    painter.extend(shapes);
}

fn preview_cut_phases(
    paths: &[LineString<f32>],
    modes: &[CutMode],
    all_perforation: bool,
    dash: f32,
    gap: f32,
    overcut: crate::cut::OvercutSettings,
    peel_tabs: &[(usize, PeelTab)],
) -> Vec<CutPhase> {
    let modes = effective_cut_modes(paths.len(), modes, all_perforation);
    let prepared = paths
        .iter()
        .zip(&modes)
        .map(|(path, mode)| {
            if *mode == CutMode::Kiss && overcut.enabled && path.0.first() == path.0.last() {
                apply_overcut(path, overcut)
            } else {
                path.clone()
            }
        })
        .collect::<Vec<_>>();
    let mut phases = plan_cut_phases(&prepared, &modes, 1, 1, dash, gap);
    if !peel_tabs.is_empty() {
        let tabs = peel_tabs
            .iter()
            .map(|(_, tab)| tab.path.clone())
            .collect::<Vec<_>>();
        if !tabs.is_empty() {
            if let Some(kiss) = phases.iter_mut().find(|phase| phase.mode == CutMode::Kiss) {
                kiss.paths.extend(tabs);
            } else {
                phases.insert(
                    0,
                    CutPhase {
                        mode: CutMode::Kiss,
                        pressure: 1,
                        paths: tabs,
                    },
                );
            }
        }
    }
    phases
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn physical_grid_spacing_uses_selected_device_dpi() {
        let dpi = 254.0;
        let step_mm = adaptive_grid_step(10.0, dpi / 25.4, 1.0, 1_000.0);
        assert_eq!(step_mm, 10.0);
        assert!((step_mm * dpi / 25.4 - 100.0).abs() < 0.001);
    }

    #[test]
    fn changing_units_preserves_the_physical_grid_spacing() {
        let dpi = 300.0;
        let expected_pixels = 10.0 * dpi / 25.4;
        for unit in CanvasUnit::ALL {
            let pixels_per_unit = unit.pixels_per_unit(dpi);
            let step = adaptive_grid_step(unit.from_mm(10.0, dpi), pixels_per_unit, 1.0, 1_000.0);
            assert!((step * pixels_per_unit - expected_pixels).abs() < 0.001);
        }
    }

    #[test]
    fn adaptive_grid_uses_nice_steps_and_caps_line_count() {
        let pixels_per_mm = 10.0;
        let step = adaptive_grid_step(0.5, pixels_per_mm, 0.1, 1_000_000.0);
        assert_eq!(step, 250.0);
        let ticks = tick_positions(1_000_000.0, step * pixels_per_mm);
        assert!(ticks.len() <= MAX_GRID_LINES_PER_AXIS);
        assert!(ticks.windows(2).all(|pair| pair[1] > pair[0]));
    }

    #[test]
    fn grid_preserves_requested_spacing_until_adaptation_is_needed() {
        for requested in [6.0, 25.0, 75.0] {
            assert_eq!(adaptive_grid_step(requested, 10.0, 1.0, 1_000.0), requested);
        }
        assert_eq!(adaptive_grid_step(6.0, 10.0, 0.01, 1_000.0), 300.0);
    }

    #[test]
    fn invalid_tick_inputs_do_not_iterate() {
        assert!(tick_positions(100.0, 0.0).is_empty());
        assert!(tick_positions(f32::NAN, 10.0).is_empty());
        assert_eq!(nice_step_ceiling(f32::NAN), 1.0);
    }

    #[test]
    fn ruler_bands_stay_outside_the_document_at_every_zoom() {
        let canvas = Rect::from_min_size(Pos2::new(100.0, 80.0), Vec2::new(400.0, 700.0));
        let document_edge = canvas.expand(CANVAS_BORDER_WIDTH);

        for scene_scale in [0.1, 0.75, 1.0, 3.0] {
            let layout = RulerLayout::outside(canvas, scene_scale);

            assert_eq!(layout.canvas, canvas);
            assert_eq!(layout.top.bottom(), document_edge.top());
            assert_eq!(layout.left.right(), document_edge.left());
            assert!(!layout.top.intersect(document_edge).is_positive());
            assert!(!layout.left.intersect(document_edge).is_positive());
            assert!(!layout.corner.intersect(document_edge).is_positive());
            assert!((layout.top.height() * scene_scale - RULER_SIZE).abs() < 0.001);
            assert!((layout.left.width() * scene_scale - RULER_SIZE).abs() < 0.001);
            assert!(layout.bounds().contains_rect(layout.top));
            assert!(layout.bounds().contains_rect(layout.left));
            assert!(layout.bounds().contains_rect(layout.corner));
        }
    }

    #[test]
    fn fit_reserves_the_full_screen_space_ruler_gutter_at_the_final_scale() {
        let canvas = Rect::from_min_size(Pos2::new(100.0, 80.0), Vec2::new(400.0, 700.0));

        for viewport in [Vec2::new(700.0, 500.0), Vec2::new(1280.0, 720.0)] {
            let fitted = fitted_scene_rect(canvas, viewport, true, 2.0);
            let final_scale = (viewport / fitted.size()).min_elem();
            let layout = RulerLayout::outside(canvas, final_scale);

            assert!(fitted.contains_rect(layout.bounds()));
            assert!((layout.top.height() * final_scale - RULER_SIZE).abs() < 0.001);
            assert!((layout.left.width() * final_scale - RULER_SIZE).abs() < 0.001);
        }
    }

    fn assert_pos_close(actual: Pos2, expected: Pos2) {
        assert!(
            actual.distance(expected) < 0.001,
            "expected {expected:?}, got {actual:?}"
        );
    }

    #[test]
    fn oriented_corners_rotate_around_image_center() {
        let corners = oriented_corners(Pos2::new(10.0, 20.0), Vec2::new(40.0, 20.0), 90.0);
        assert_pos_close(corners[0], Pos2::new(40.0, 10.0));
        assert_pos_close(corners[1], Pos2::new(40.0, 50.0));
        assert_pos_close(corners[2], Pos2::new(20.0, 50.0));
        assert_pos_close(corners[3], Pos2::new(20.0, 10.0));
    }

    #[test]
    fn resize_preserves_opposite_world_corner_at_multiple_angles() {
        for rotation in [0.0, 37.0, 90.0, -135.0] {
            let offset = Pos2::new(100.0, 80.0);
            let size = Vec2::new(120.0, 60.0);
            let opposite = oriented_corners(offset, size, rotation)[2];
            let desired_size = Vec2::new(180.0, 90.0);
            let pointer =
                opposite + rotate_vector(Vec2::new(-desired_size.x, -desired_size.y), rotation);
            let (new_offset, new_scale) = resize_from_handle(
                offset,
                size,
                Vec2::new(1.2, 0.6),
                Vec2::new(100.0, 100.0),
                rotation,
                ResizeHandle::NorthWest,
                pointer,
                false,
                false,
            )
            .unwrap();
            assert!((new_scale.x - 1.8).abs() < 0.001);
            assert!((new_scale.y - 0.9).abs() < 0.001);
            assert_pos_close(
                oriented_corners(new_offset, desired_size, rotation)[2],
                opposite,
            );
        }
    }

    #[test]
    fn every_resize_handle_preserves_its_opposite_anchor_when_rotated() {
        let offset = Pos2::new(100.0, 80.0);
        let size = Vec2::new(120.0, 60.0);
        let natural_size = Vec2::new(100.0, 100.0);
        let scale = Vec2::new(1.2, 0.6);
        for rotation in [0.0, 37.0, 90.0, -135.0] {
            for handle in ResizeHandle::ALL {
                let signs = handle.signs();
                let desired = Vec2::new(
                    if signs.x == 0.0 { size.x } else { 180.0 },
                    if signs.y == 0.0 { size.y } else { 90.0 },
                );
                let anchor = resize_handle_position(offset, size, rotation, handle.opposite());
                let pointer = anchor
                    + rotate_vector(
                        Vec2::new(signs.x * desired.x, signs.y * desired.y),
                        rotation,
                    );
                let (new_offset, new_scale) = resize_from_handle(
                    offset,
                    size,
                    scale,
                    natural_size,
                    rotation,
                    handle,
                    pointer,
                    false,
                    false,
                )
                .unwrap();
                let new_size = natural_size * new_scale;
                assert!((new_size.x - desired.x).abs() < 0.001);
                assert!((new_size.y - desired.y).abs() < 0.001);
                assert_pos_close(
                    resize_handle_position(new_offset, new_size, rotation, handle.opposite()),
                    anchor,
                );
            }
        }
    }

    #[test]
    fn alt_option_resize_keeps_center_fixed() {
        let offset = Pos2::new(100.0, 80.0);
        let size = Vec2::new(120.0, 60.0);
        let center = offset + size / 2.0;
        let rotation = 37.0;
        let pointer = center + rotate_vector(Vec2::new(90.0, 0.0), rotation);
        let (new_offset, new_scale) = resize_from_handle(
            offset,
            size,
            Vec2::new(1.2, 0.6),
            Vec2::new(100.0, 100.0),
            rotation,
            ResizeHandle::East,
            pointer,
            false,
            true,
        )
        .unwrap();
        assert_pos_close(new_offset + Vec2::new(180.0, 60.0) / 2.0, center);
        assert!((new_scale.x - 1.8).abs() < 0.001);
        assert!((new_scale.y - 0.6).abs() < 0.001);
    }

    #[test]
    fn resize_cursors_follow_object_rotation() {
        assert_eq!(
            resize_cursor(ResizeHandle::East, 0.0),
            CursorIcon::ResizeHorizontal
        );
        assert_eq!(
            resize_cursor(ResizeHandle::East, 90.0),
            CursorIcon::ResizeVertical
        );
        assert_eq!(
            resize_cursor(ResizeHandle::NorthWest, 0.0),
            CursorIcon::ResizeNwSe
        );
        assert_eq!(
            resize_cursor(ResizeHandle::NorthWest, 90.0),
            CursorIcon::ResizeNeSw
        );
    }

    #[test]
    fn aspect_locked_resize_keeps_ratio_and_clamps_positive() {
        let offset = Pos2::new(0.0, 0.0);
        let size = Vec2::new(100.0, 50.0);
        let (new_offset, new_scale) = resize_from_handle(
            offset,
            size,
            Vec2::splat(1.0),
            size,
            0.0,
            ResizeHandle::NorthWest,
            Pos2::new(-100.0, -25.0),
            true,
            false,
        )
        .unwrap();
        let new_size = size * new_scale;
        assert!((new_size.x / new_size.y - 2.0).abs() < 0.001);
        assert_pos_close(
            oriented_corners(new_offset, new_size, 0.0)[2],
            Pos2::new(100.0, 50.0),
        );

        let (_, clamped_scale) = resize_from_handle(
            offset,
            size,
            Vec2::splat(1.0),
            size,
            0.0,
            ResizeHandle::NorthWest,
            Pos2::new(200.0, 100.0),
            true,
            false,
        )
        .unwrap();
        assert!(clamped_scale.x.is_finite());
        assert!(clamped_scale.y.is_finite());
        assert!(clamped_scale.x > 0.0 && clamped_scale.y > 0.0);
    }

    #[test]
    fn aspect_locked_resize_handles_extreme_persisted_scales_without_panicking() {
        let scale = Vec2::new(2_000.0, 0.0001);
        let natural = Vec2::splat(100.0);
        let size = natural * scale;
        let result = resize_from_handle(
            Pos2::ZERO,
            size,
            scale,
            natural,
            23.0,
            ResizeHandle::NorthWest,
            Pos2::new(-20.0, -20.0),
            true,
            false,
        )
        .unwrap();
        assert!(result.0.x.is_finite() && result.0.y.is_finite());
        assert!(result.1.x.is_finite() && result.1.y.is_finite());
        assert!(result.1.x > 0.0 && result.1.y > 0.0);
    }

    #[test]
    fn rotation_drag_preserves_grab_offset_and_shift_snaps_to_fifteen_degrees() {
        let center = Pos2::new(50.0, 50.0);
        let pickup = Pos2::new(45.0, 0.0);
        let start = pointer_rotation_degrees(center, pickup);
        assert_eq!(
            rotation_from_gesture(30.0, start, center, pickup, false),
            30.0
        );
        assert!(
            (rotation_from_gesture(30.0, start, center, Pos2::new(50.0, -10.0), false) - 35.7)
                .abs()
                < 0.1
        );
        assert_eq!(
            rotation_from_gesture(30.0, start, center, Pos2::new(100.0, 40.0), true),
            120.0
        );
        for degrees in [-1080.0, -180.0, 180.0, 1080.0] {
            let normalized = normalize_degrees(degrees);
            assert!((-180.0..180.0).contains(&normalized));
        }
    }

    #[test]
    fn escape_cancel_snapshot_restores_resize_and_rotation_starts() {
        let resize = TransformGesture::Resize {
            image_id: "artwork".into(),
            handle: ResizeHandle::SouthEast,
            offset: Pos2::new(10.0, 20.0),
            size: Vec2::new(100.0, 50.0),
            scale: Vec2::new(1.0, 0.5),
            natural_size: Vec2::splat(100.0),
            rotation_degrees: 25.0,
            aspect_locked: false,
        };
        assert_eq!(
            canceled_transform(&resize, Pos2::new(40.0, 50.0), Vec2::splat(2.0)),
            (Pos2::new(10.0, 20.0), Vec2::new(1.0, 0.5), 25.0)
        );

        let rotate = TransformGesture::Rotate {
            image_id: "artwork".into(),
            center: Pos2::new(60.0, 45.0),
            start_pointer_degrees: 0.0,
            initial_rotation_degrees: -35.0,
        };
        assert_eq!(
            canceled_transform(&rotate, Pos2::new(10.0, 20.0), Vec2::new(1.0, 0.5)),
            (Pos2::new(10.0, 20.0), Vec2::new(1.0, 0.5), -35.0)
        );
    }

    #[test]
    fn transform_handle_registered_after_artwork_wins_pointer_capture() {
        fn run_frame(ctx: &egui::Context, events: Vec<egui::Event>) -> (bool, bool) {
            let mut body_dragged = false;
            let mut handle_dragged = false;
            let raw = egui::RawInput {
                events,
                ..Default::default()
            };
            let _ = ctx.run(raw, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    let body = ui.interact(
                        Rect::from_min_max(Pos2::new(10.0, 10.0), Pos2::new(110.0, 110.0)),
                        Id::new("test-artwork"),
                        Sense::drag(),
                    );
                    let handle = ui.interact(
                        Rect::from_center_size(Pos2::new(50.0, 50.0), Vec2::splat(18.0)),
                        Id::new("test-handle"),
                        Sense::drag(),
                    );
                    body_dragged = body.dragged();
                    handle_dragged = handle.dragged();
                });
            });
            (body_dragged, handle_dragged)
        }

        let context = egui::Context::default();
        run_frame(
            &context,
            vec![egui::Event::PointerMoved(Pos2::new(50.0, 50.0))],
        );
        run_frame(
            &context,
            vec![egui::Event::PointerButton {
                pos: Pos2::new(50.0, 50.0),
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::NONE,
            }],
        );
        let (body_dragged, handle_dragged) = run_frame(
            &context,
            vec![egui::Event::PointerMoved(Pos2::new(75.0, 75.0))],
        );
        assert!(!body_dragged);
        assert!(handle_dragged);
    }

    #[test]
    fn peel_tab_drag_interaction_projects_pointer_onto_the_perimeter() {
        fn run_frame(ctx: &egui::Context, events: Vec<egui::Event>) -> Option<f32> {
            let path = LineString::from(vec![
                (0.0, 0.0),
                (100.0, 0.0),
                (100.0, 100.0),
                (0.0, 100.0),
                (0.0, 0.0),
            ]);
            let mut dragged_position = None;
            let raw = egui::RawInput {
                events,
                ..Default::default()
            };
            let _ = ctx.run(raw, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    let to_screen = RectTransform::identity(Rect::from_min_max(
                        Pos2::ZERO,
                        Pos2::new(100.0, 100.0),
                    ));
                    let (_, position) = interact_peel_tab_handle(
                        ui,
                        Id::new("peel-tab-drag-test"),
                        Pos2::new(50.0, 50.0),
                        24.0,
                        &path,
                        &to_screen,
                        true,
                    );
                    dragged_position = position;
                });
            });
            dragged_position
        }

        let context = egui::Context::default();
        run_frame(
            &context,
            vec![egui::Event::PointerMoved(Pos2::new(50.0, 50.0))],
        );
        run_frame(
            &context,
            vec![egui::Event::PointerButton {
                pos: Pos2::new(50.0, 50.0),
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::NONE,
            }],
        );
        let position = run_frame(
            &context,
            vec![egui::Event::PointerMoved(Pos2::new(100.0, 50.0))],
        )
        .expect("dragging the handle should produce a perimeter position");
        assert!((position - 0.375).abs() < 0.001);
    }

    #[test]
    fn preview_omits_disabled_geometry_and_preserves_modes() {
        let line = |y| LineString::from(vec![(0.0, y), (20.0, y)]);
        let phases = preview_cut_phases(
            &[line(0.0), line(1.0), line(2.0)],
            &[CutMode::Kiss, CutMode::Disabled, CutMode::Perforation],
            false,
            5.0,
            2.0,
            crate::cut::OvercutSettings::default(),
            &[],
        );
        assert_eq!(phases.len(), 2);
        assert_eq!(phases[0].mode, CutMode::Kiss);
        assert_eq!(phases[0].paths, [line(0.0)]);
        assert_eq!(phases[1].mode, CutMode::Perforation);
        assert!(
            phases
                .iter()
                .flat_map(|phase| phase.paths.iter())
                .flat_map(|path| path.0.iter())
                .all(|point| point.y != 1.0)
        );
    }
}
