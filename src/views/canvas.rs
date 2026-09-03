use egui::{
    Align2, Color32, CursorIcon, FontId, Frame, Id, Key, KeyboardShortcut, Modifiers, Painter,
    Pos2, Rect, Scene, Sense, Shape, Stroke, Ui, Vec2,
    emath::{self, RectTransform},
};
use geo::LineString;
use tracing::instrument;

use crate::{
    SapodillaApp,
    app::peel_tab_unmirrored,
    cut::apply_overcut,
    protocol::DEVICES,
    toolpath::{CutMode, CutPhase, effective_cut_modes, plan_cut_phases},
};

const CUT_LINE_WIDTH: f32 = 3.0;
const MIN_GRID_SCREEN_SPACING: f32 = 24.0;
const MAX_GRID_LINES_PER_AXIS: usize = 512;
const RULER_SIZE: f32 = 24.0;
const TRANSFORM_HANDLE_SIZE: f32 = 9.0;
const TRANSFORM_HIT_SIZE: f32 = 18.0;
const ROTATION_HANDLE_OFFSET: f32 = 32.0;
const MIN_IMAGE_SIZE: f32 = 1.0;
const MAX_IMAGE_SCALE: f32 = 1_000.0;

const DELETE_SHORTCUT: KeyboardShortcut = KeyboardShortcut::new(Modifiers::NONE, Key::Delete);
const BACKSPACE_SHORTCUT: KeyboardShortcut = KeyboardShortcut::new(Modifiers::NONE, Key::Backspace);

const NORMAL_UV: Rect = Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(1.0, 1.0));

#[derive(Clone, Debug)]
pub(crate) enum TransformGesture {
    Resize {
        image_id: String,
        corner: usize,
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
    },
}

pub fn canvas_editor(ui: &mut Ui, state: &mut SapodillaApp) {
    let scene = Scene::new().zoom_range(0.1..=3.0);

    let mut inner_rect = Rect::NAN;
    let mut canvas_rect = state.canvas_rect;

    let response = scene
        .show(ui, &mut canvas_rect, |ui| {
            Frame::canvas(ui.style())
                .fill(state.background_color32())
                .inner_margin(0.0)
                .stroke(Stroke::new(4.0_f32, Color32::BLACK))
                .show(ui, |ui| {
                    frame(ui, state);
                });
            inner_rect = ui.min_rect();
        })
        .response;

    state.canvas_rect = canvas_rect;

    if response.double_clicked() || state.previous_canvas_size != state.get_canvas().size {
        state.canvas_rect = inner_rect.shrink(ui.style().spacing.menu_spacing);
        state.previous_canvas_size = state.get_canvas().size;
    }
}

fn frame(ui: &mut Ui, state: &mut SapodillaApp) {
    let size = state.get_canvas().size;

    ui.set_min_size(size);
    ui.set_max_size(size);

    let (response, mut painter) = ui.allocate_painter(size, Sense::empty());

    let to_screen = emath::RectTransform::from_to(
        Rect::from_min_size(Pos2::ZERO, response.rect.size()),
        response.rect,
    );

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
            scene_scale,
        );
    }

    let mut artwork_transform_changed =
        interact_transform_handles(ui, state, response.id, &to_screen, scene_scale);
    let transform_active_image_id = state
        .canvas_transform_gesture
        .as_ref()
        .map(transform_gesture_image_id)
        .map(str::to_owned);

    let mut hovers = Vec::new();
    let mut remove = None;

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
            Sense::drag()
        };
        let rect_response = ui.interact(image_rect, rect_id, sense);

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

        let handle_owns_gesture = transform_active_image_id.as_deref() == Some(&image.id);
        if !image.locked && !handle_owns_gesture && !state.edit_cutlines {
            if rect_response.drag_delta() != Vec2::ZERO {
                artwork_transform_changed = true;
            }
            image.offset += rect_response.drag_delta();
            if state.snap_to_guides && rect_response.dragged() {
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

        if rect_response.hovered() {
            hovers.push(image_rect);
        }
        if state.selected_images.contains(&idx)
            && state.selected_images.len() != 1
            && !hovers.contains(&image_rect)
        {
            hovers.push(image_rect);
        }

        if rect_response.hovered()
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

    if artwork_transform_changed {
        invalidate_auto_cutlines(state);
    }

    if state.show_cutlines {
        for phase in preview_cut_phases(
            &state.cut_shapes,
            &state.cut_modes,
            state.perf_cut,
            state.perf_dash_mm * dpi / 25.4,
            state.perf_gap_mm * dpi / 25.4,
            state.overcut,
            state.peel_tabs,
        ) {
            let stroke = match phase.mode {
                CutMode::Kiss => Stroke::new(CUT_LINE_WIDTH, Color32::from_rgb(67, 170, 139)),
                CutMode::Perforation => Stroke::new(CUT_LINE_WIDTH, Color32::from_rgb(249, 65, 68)),
                CutMode::Disabled => continue,
            };
            paint_polygons(&to_screen, &painter, &phase.paths, stroke);
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

    if state.show_rulers {
        paint_rulers(
            &painter,
            response.rect,
            ui.clip_rect(),
            size,
            dpi,
            state.grid_spacing_mm,
            scene_scale,
        );
    }

    if let Some(remove) = remove {
        state.loaded_images.remove(remove);
        state.selected_images.retain(|selected| *selected != remove);
        for selected in &mut state.selected_images {
            if *selected > remove {
                *selected -= 1;
            }
        }
    }
}

fn transform_gesture_image_id(gesture: &TransformGesture) -> &str {
    match gesture {
        TransformGesture::Resize { image_id, .. } | TransformGesture::Rotate { image_id, .. } => {
            image_id
        }
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

fn normalize_degrees(degrees: f32) -> f32 {
    (degrees + 180.0).rem_euclid(360.0) - 180.0
}

fn rotation_from_pointer(center: Pos2, pointer: Pos2, snap: bool) -> f32 {
    let delta = pointer - center;
    let degrees = normalize_degrees(delta.y.atan2(delta.x).to_degrees() + 90.0);
    if snap {
        normalize_degrees((degrees / 15.0).round() * 15.0)
    } else {
        degrees
    }
}

#[allow(clippy::too_many_arguments)]
fn resize_from_corner(
    offset: Pos2,
    size: Vec2,
    scale: Vec2,
    natural_size: Vec2,
    rotation_degrees: f32,
    corner: usize,
    pointer: Pos2,
    aspect_locked: bool,
) -> Option<(Pos2, Vec2)> {
    if corner >= 4
        || !size.x.is_finite()
        || !size.y.is_finite()
        || size.x <= 0.0
        || size.y <= 0.0
        || natural_size.x <= 0.0
        || natural_size.y <= 0.0
    {
        return None;
    }
    let signs = [
        Vec2::new(-1.0, -1.0),
        Vec2::new(1.0, -1.0),
        Vec2::new(1.0, 1.0),
        Vec2::new(-1.0, 1.0),
    ];
    let sign = signs[corner];
    let opposite = oriented_corners(offset, size, rotation_degrees)[(corner + 2) % 4];
    let local_delta = rotate_vector(pointer - opposite, -rotation_degrees);

    let (new_size, new_scale) = if aspect_locked {
        let diagonal = Vec2::new(sign.x * size.x, sign.y * size.y);
        let denominator = diagonal.length_sq();
        if denominator <= f32::EPSILON {
            return None;
        }
        let minimum = (MIN_IMAGE_SIZE / size.x)
            .max(MIN_IMAGE_SIZE / size.y)
            .max(f32::EPSILON);
        let maximum = (MAX_IMAGE_SCALE / scale.x.abs().max(f32::EPSILON))
            .min(MAX_IMAGE_SCALE / scale.y.abs().max(f32::EPSILON));
        let factor = (local_delta.dot(diagonal) / denominator).clamp(minimum, maximum);
        (size * factor, scale * factor)
    } else {
        let width =
            (sign.x * local_delta.x).clamp(MIN_IMAGE_SIZE, natural_size.x * MAX_IMAGE_SCALE);
        let height =
            (sign.y * local_delta.y).clamp(MIN_IMAGE_SIZE, natural_size.y * MAX_IMAGE_SCALE);
        (
            Vec2::new(width, height),
            Vec2::new(width / natural_size.x, height / natural_size.y),
        )
    };

    if !new_size.is_finite() || !new_scale.is_finite() {
        return None;
    }
    let center = opposite
        + rotate_vector(
            Vec2::new(sign.x * new_size.x / 2.0, sign.y * new_size.y / 2.0),
            rotation_degrees,
        );
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
    let corners = oriented_corners(offset, size, rotation_degrees);
    let hit_size = TRANSFORM_HIT_SIZE / scene_scale;
    let rotation_distance = ROTATION_HANDLE_OFFSET / scene_scale;
    let rotate_handle = rotation_handle_position(offset, size, rotation_degrees, rotation_distance);
    let mut gesture = state.canvas_transform_gesture.take();
    let mut changed = false;
    let mut pending_resize = None;
    let mut pending_rotation = None;

    for (corner, position) in corners.into_iter().enumerate() {
        let position = to_screen.transform_pos(position);
        let response = ui
            .interact(
                Rect::from_center_size(position, Vec2::splat(hit_size)),
                canvas_id.with(("resize", image_id.as_str(), corner)),
                Sense::drag(),
            )
            .on_hover_cursor(if corner % 2 == 0 {
                CursorIcon::ResizeNwSe
            } else {
                CursorIcon::ResizeNeSw
            });
        if response.drag_started() {
            gesture = Some(TransformGesture::Resize {
                image_id: image_id.clone(),
                corner,
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
                corner,
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
            let invert_lock = ui.input(|input| input.modifiers.shift);
            pending_resize = resize_from_corner(
                *offset,
                *size,
                *scale,
                *natural_size,
                *rotation_degrees,
                *corner,
                pointer,
                *aspect_locked ^ invert_lock,
            );
        }
    }

    let rotate_response = ui
        .interact(
            Rect::from_center_size(
                to_screen.transform_pos(rotate_handle),
                Vec2::splat(hit_size),
            ),
            canvas_id.with(("rotate", image_id.as_str())),
            Sense::drag(),
        )
        .on_hover_cursor(CursorIcon::Crosshair);
    if rotate_response.drag_started() {
        gesture = Some(TransformGesture::Rotate {
            image_id: image_id.clone(),
            center: offset + size / 2.0,
        });
    }
    if rotate_response.dragged()
        && let Some(pointer) = rotate_response.interact_pointer_pos()
        && let Some(TransformGesture::Rotate {
            image_id: active_id,
            center,
        }) = gesture.as_ref()
        && active_id == &image_id
    {
        let pointer = to_screen.inverse().transform_pos(pointer);
        pending_rotation = Some(rotation_from_pointer(
            *center,
            pointer,
            ui.input(|input| input.modifiers.shift),
        ));
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
    painter.add(Shape::line(outline, Stroke::new(1.5 / scene_scale, color)));
    if image.locked || state.edit_cutlines {
        return;
    }

    let visual_size = TRANSFORM_HANDLE_SIZE / scene_scale;
    for position in corners {
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
}

fn invalidate_auto_cutlines(state: &mut SapodillaApp) {
    let count = state.auto_cut_count.min(state.cut_shapes.len());
    if count == 0 {
        return;
    }
    state.cut_shapes.drain(..count);
    state.cut_modes.drain(..count.min(state.cut_modes.len()));
    state
        .cutline_owners
        .drain(..count.min(state.cutline_owners.len()));
    state
        .cutline_locked
        .drain(..count.min(state.cutline_locked.len()));
    state.auto_cut_count = 0;
    state.selected_cut_path = None;
    state.selected_cut_node = None;
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

fn adaptive_grid_step_mm(
    requested_mm: f32,
    pixels_per_mm: f32,
    scene_scale: f32,
    extent_pixels: f32,
) -> f32 {
    let base = requested_mm.clamp(0.5, 100.0);
    let screen_requirement = MIN_GRID_SCREEN_SPACING / (pixels_per_mm * scene_scale).max(0.001);
    let count_requirement = extent_pixels / pixels_per_mm / MAX_GRID_LINES_PER_AXIS as f32;
    nice_step_ceiling(base.max(screen_requirement).max(count_requirement))
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
    (0..=count.min(MAX_GRID_LINES_PER_AXIS))
        .map(|index| index as f32 * step_pixels)
        .collect()
}

fn paint_grid(
    painter: &Painter,
    canvas_rect: Rect,
    canvas_size: Vec2,
    dpi: f32,
    requested_mm: f32,
    scene_scale: f32,
) {
    let pixels_per_mm = dpi / 25.4;
    let step_mm = adaptive_grid_step_mm(
        requested_mm,
        pixels_per_mm,
        scene_scale,
        canvas_size.x.max(canvas_size.y),
    );
    let step_pixels = step_mm * pixels_per_mm;
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

fn paint_rulers(
    painter: &Painter,
    canvas_rect: Rect,
    viewport_rect: Rect,
    canvas_size: Vec2,
    dpi: f32,
    requested_mm: f32,
    scene_scale: f32,
) {
    let visible = canvas_rect.intersect(viewport_rect);
    if !visible.is_positive() {
        return;
    }

    let pixels_per_mm = dpi / 25.4;
    let step_mm = adaptive_grid_step_mm(
        requested_mm,
        pixels_per_mm,
        scene_scale,
        canvas_size.x.max(canvas_size.y),
    );
    let step_pixels = step_mm * pixels_per_mm;
    let band = RULER_SIZE / scene_scale;
    let stroke_width = 1.0 / scene_scale;
    let top_band = Rect::from_min_max(
        visible.min,
        Pos2::new(
            visible.right(),
            (visible.top() + band).min(visible.bottom()),
        ),
    );
    let left_band = Rect::from_min_max(
        visible.min,
        Pos2::new(
            (visible.left() + band).min(visible.right()),
            visible.bottom(),
        ),
    );
    let overlay = painter.with_clip_rect(visible);
    let fill = Color32::from_rgba_unmultiplied(245, 248, 250, 232);
    overlay.rect_filled(top_band, 0.0, fill);
    overlay.rect_filled(left_band, 0.0, fill);
    overlay.line_segment(
        [top_band.left_bottom(), top_band.right_bottom()],
        Stroke::new(stroke_width, Color32::from_gray(105)),
    );
    overlay.line_segment(
        [left_band.right_top(), left_band.right_bottom()],
        Stroke::new(stroke_width, Color32::from_gray(105)),
    );

    let font = FontId::monospace(9.0 / scene_scale);
    let tick_color = Color32::from_gray(75);
    let first_x = (((visible.left() - canvas_rect.left()) / step_pixels).floor() as isize).max(0);
    let last_x = (((visible.right() - canvas_rect.left()) / step_pixels).ceil() as usize)
        .min(MAX_GRID_LINES_PER_AXIS);
    for index in first_x as usize..=last_x {
        let x = canvas_rect.left() + index as f32 * step_pixels;
        if x > canvas_rect.right() {
            break;
        }
        let major = index % 5 == 0;
        let tick = if major { band * 0.5 } else { band * 0.25 };
        overlay.line_segment(
            [
                Pos2::new(x, top_band.bottom()),
                Pos2::new(x, top_band.bottom() - tick),
            ],
            Stroke::new(stroke_width, tick_color),
        );
        if major && x >= left_band.right() {
            overlay.text(
                Pos2::new(x + 2.0 / scene_scale, top_band.top() + 2.0 / scene_scale),
                Align2::LEFT_TOP,
                format_tick(index as f32 * step_mm),
                font.clone(),
                tick_color,
            );
        }
    }

    let first_y = (((visible.top() - canvas_rect.top()) / step_pixels).floor() as isize).max(0);
    let last_y = (((visible.bottom() - canvas_rect.top()) / step_pixels).ceil() as usize)
        .min(MAX_GRID_LINES_PER_AXIS);
    for index in first_y as usize..=last_y {
        let y = canvas_rect.top() + index as f32 * step_pixels;
        if y > canvas_rect.bottom() {
            break;
        }
        let major = index % 5 == 0;
        let tick = if major { band * 0.5 } else { band * 0.25 };
        overlay.line_segment(
            [
                Pos2::new(left_band.right(), y),
                Pos2::new(left_band.right() - tick, y),
            ],
            Stroke::new(stroke_width, tick_color),
        );
        if major && y >= top_band.bottom() {
            overlay.text(
                Pos2::new(left_band.left() + 2.0 / scene_scale, y + 2.0 / scene_scale),
                Align2::LEFT_TOP,
                format_tick(index as f32 * step_mm),
                font.clone(),
                tick_color,
            );
        }
    }
    overlay.rect_filled(top_band.intersect(left_band), 0.0, Color32::from_gray(225));
    overlay.text(
        top_band.intersect(left_band).center(),
        Align2::CENTER_CENTER,
        "mm",
        font,
        tick_color,
    );
}

fn format_tick(value_mm: f32) -> String {
    if value_mm.fract().abs() < 0.001 {
        format!("{value_mm:.0}")
    } else {
        format!("{value_mm:.1}")
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
    for line_string in cut_shapes {
        // Create a line shape for each line from all our polygons.
        let shapes = line_string.lines().map(|line| {
            let start = to_screen.transform_pos(Pos2::new(line.start.x, line.start.y));
            let end = to_screen.transform_pos(Pos2::new(line.end.x, line.end.y));

            Shape::line(vec![start, end], stroke)
        });

        painter.extend(shapes);
    }
}

fn preview_cut_phases(
    paths: &[LineString<f32>],
    modes: &[CutMode],
    all_perforation: bool,
    dash: f32,
    gap: f32,
    overcut: crate::cut::OvercutSettings,
    peel_tabs: bool,
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
    if peel_tabs {
        let tabs = paths
            .iter()
            .zip(&modes)
            .filter(|(_, mode)| **mode != CutMode::Disabled)
            .filter_map(|(path, _)| peel_tab_unmirrored(path))
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
        let step_mm = adaptive_grid_step_mm(10.0, dpi / 25.4, 1.0, 1_000.0);
        assert_eq!(step_mm, 10.0);
        assert!((step_mm * dpi / 25.4 - 100.0).abs() < 0.001);
    }

    #[test]
    fn adaptive_grid_uses_nice_steps_and_caps_line_count() {
        let pixels_per_mm = 10.0;
        let step = adaptive_grid_step_mm(0.5, pixels_per_mm, 0.1, 1_000_000.0);
        assert!(matches!(step, 200.0 | 500.0 | 1_000.0));
        let ticks = tick_positions(1_000_000.0, step * pixels_per_mm);
        assert!(ticks.len() <= MAX_GRID_LINES_PER_AXIS + 1);
        assert!(ticks.windows(2).all(|pair| pair[1] > pair[0]));
    }

    #[test]
    fn invalid_tick_inputs_do_not_iterate() {
        assert!(tick_positions(100.0, 0.0).is_empty());
        assert!(tick_positions(f32::NAN, 10.0).is_empty());
        assert_eq!(nice_step_ceiling(f32::NAN), 1.0);
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
            let (new_offset, new_scale) = resize_from_corner(
                offset,
                size,
                Vec2::new(1.2, 0.6),
                Vec2::new(100.0, 100.0),
                rotation,
                0,
                pointer,
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
    fn aspect_locked_resize_keeps_ratio_and_clamps_positive() {
        let offset = Pos2::new(0.0, 0.0);
        let size = Vec2::new(100.0, 50.0);
        let (new_offset, new_scale) = resize_from_corner(
            offset,
            size,
            Vec2::splat(1.0),
            size,
            0.0,
            0,
            Pos2::new(-100.0, -25.0),
            true,
        )
        .unwrap();
        let new_size = size * new_scale;
        assert!((new_size.x / new_size.y - 2.0).abs() < 0.001);
        assert_pos_close(
            oriented_corners(new_offset, new_size, 0.0)[2],
            Pos2::new(100.0, 50.0),
        );

        let (_, clamped_scale) = resize_from_corner(
            offset,
            size,
            Vec2::splat(1.0),
            size,
            0.0,
            0,
            Pos2::new(200.0, 100.0),
            true,
        )
        .unwrap();
        assert!(clamped_scale.x.is_finite());
        assert!(clamped_scale.y.is_finite());
        assert!(clamped_scale.x > 0.0 && clamped_scale.y > 0.0);
    }

    #[test]
    fn rotation_is_normalized_and_shift_snaps_to_fifteen_degrees() {
        let center = Pos2::new(50.0, 50.0);
        assert!((rotation_from_pointer(center, Pos2::new(50.0, 0.0), false)).abs() < 0.001);
        assert!(
            (rotation_from_pointer(center, Pos2::new(100.0, 50.0), false) - 90.0).abs() < 0.001
        );
        assert!(
            (rotation_from_pointer(center, Pos2::new(50.0, 100.0), false) + 180.0).abs() < 0.001
        );
        assert_eq!(
            rotation_from_pointer(center, Pos2::new(100.0, 40.0), true),
            75.0
        );
        for degrees in [-1080.0, -180.0, 180.0, 1080.0] {
            let normalized = normalize_degrees(degrees);
            assert!((-180.0..180.0).contains(&normalized));
        }
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
            false,
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
