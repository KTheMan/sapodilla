use egui::{
    Color32, Frame, Key, KeyboardShortcut, Modifiers, Painter, Pos2, Rect, Scene, Sense, Shape,
    Stroke, Ui, Vec2,
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

const DELETE_SHORTCUT: KeyboardShortcut = KeyboardShortcut::new(Modifiers::NONE, Key::Delete);
const BACKSPACE_SHORTCUT: KeyboardShortcut = KeyboardShortcut::new(Modifiers::NONE, Key::Backspace);

const NORMAL_UV: Rect = Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(1.0, 1.0));

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

    let mut hovers = Vec::new();
    let mut remove = None;

    for (idx, image) in state.loaded_images.iter_mut().enumerate() {
        if !image.visible {
            continue;
        }
        let pos_in_screen = to_screen.transform_pos(image.visual_offset());
        let image_rect = Rect::from_min_size(pos_in_screen, image.rotated_size());

        let rect_id = response.id.with(idx);
        let rect_response = ui.interact(image_rect, rect_id, Sense::drag());

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

        if !image.locked {
            image.offset += rect_response.drag_delta();
            if state.snap_to_guides && rect_response.dragged() {
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
                        if (anchor - target).abs() <= 6.0 {
                            image.offset.x += target - anchor;
                        }
                    }
                }
                for anchor in anchors_y {
                    for target in targets_y {
                        if (anchor - target).abs() <= 6.0 {
                            image.offset.y += target - anchor;
                        }
                    }
                }
            }
        }

        if rect_response.hovered() {
            hovers.push(image_rect);
        }
        if state.selected_images.contains(&idx) && !hovers.contains(&image_rect) {
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

    if state.show_cutlines {
        let dpi = DEVICES[state.selected_device].dpi;
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
