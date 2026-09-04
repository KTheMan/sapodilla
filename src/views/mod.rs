use std::{
    collections::{BTreeSet, VecDeque},
    io::Cursor,
    ops::RangeInclusive,
};

use egui::{Id, Modal, Pos2, ProgressBar, Ui, Vec2};
use egui_dnd::{DragDropItem, Handle, dnd};
use egui_extras::{
    Column, TableBuilder,
    syntax_highlighting::{CodeTheme, code_view_ui},
};
use tracing::debug;

use crate::{
    app::{Action, ContextSender, LoadedImage},
    cut::CutTuning,
    protocol::{self, AvocadoId, AvocadoPacket, AvocadoPacketReader, ModeType, ProtocolError},
    spawn,
};

pub use canvas::canvas_editor;
pub(crate) use canvas::synchronize_cut_preview;

mod canvas;
pub(crate) use canvas::TransformGesture;

pub fn pretty_hex(id: impl std::hash::Hash, ui: &mut Ui, data: &[u8]) {
    const SECTIONS_PER_LINE: usize = 4;
    const CHARS_PER_SECTION: usize = 4;

    let default_spacing = ui.ctx().style().spacing.item_spacing;

    egui::Grid::new(id)
        .spacing(Vec2 {
            x: default_spacing.x * 2.0,
            ..default_spacing
        })
        .show(ui, |ui| {
            for row in data.chunks(SECTIONS_PER_LINE * CHARS_PER_SECTION) {
                ui.horizontal(|ui| {
                    for chunk in row.chunks(CHARS_PER_SECTION) {
                        ui.monospace(hex::encode_upper(chunk));
                    }
                });

                // Only display directly visible characters, control characters
                // and newlines would be a problem.
                ui.monospace(
                    String::from_utf8_lossy(row).replace(|c| !(' '..='~').contains(&c), " "),
                );

                ui.end_row();
            }
        });
}

pub fn protocol_packets_table(
    ui: &mut Ui,
    packets: &VecDeque<protocol::AvocadoPacket>,
    viewing_packet: &mut Option<protocol::AvocadoPacket>,
) {
    TableBuilder::new(ui)
        .auto_shrink(false)
        .striped(true)
        .columns(Column::auto().resizable(true), 10)
        .column(Column::remainder().resizable(true))
        .header(20.0, |mut header| {
            const FIELDS: &[&str] = &[
                "Message ID",
                "Request ID",
                "Content Type",
                "Interaction Type",
                "Encoding Type",
                "Encryption Mode",
                "Terminal ID",
                "Message Number",
                "Message Total",
                "Subpackage",
                "Data",
            ];

            for field in FIELDS {
                header.col(|ui| {
                    ui.heading(*field);
                });
            }
        })
        .body(|body| {
            body.rows(20.0, packets.len(), |mut row| {
                let packet = &packets[row.index()];

                row.col(|ui| {
                    ui.label(packet.msg_number.to_string());
                });

                row.col(|ui| {
                    ui.label(
                        packet
                            .as_json::<AvocadoId>()
                            .map(|result| result.id.to_string())
                            .unwrap_or_default(),
                    );
                });

                row.col(|ui| {
                    ui.label(packet.content_type.to_string());
                });

                row.col(|ui| {
                    ui.label(packet.interaction_type.to_string());
                });

                row.col(|ui| {
                    ui.label(packet.encoding_type.to_string());
                });

                row.col(|ui| {
                    ui.label(packet.encryption_mode.to_string());
                });

                row.col(|ui| {
                    ui.label(packet.terminal_id.to_string());
                });

                row.col(|ui| {
                    ui.label(packet.msg_package_num.to_string());
                });

                row.col(|ui| {
                    ui.label(packet.msg_package_total.to_string());
                });

                row.col(|ui| {
                    ui.label(packet.is_subpackage.to_string());
                });

                row.col(|ui| {
                    ui.horizontal(|ui| {
                        ui.label(format!("{} bytes", packet.data.len()));
                        ui.add_space(8.0);
                        if ui.button("View").clicked() {
                            *viewing_packet = Some(packet.clone());
                        }
                    });
                });
            });
        });

    if let Some(packet) = viewing_packet {
        let modal = Modal::new(Id::new(packet.msg_number)).show(ui.ctx(), |ui| {
            ui.set_width(380.0);
            ui.heading("Viewing Packet Data");

            pretty_hex(format!("packet-{}", packet.msg_number), ui, &packet.data);

            ui.separator();

            if let Some(data) = packet.as_json::<serde_json::Value>() {
                let theme = CodeTheme::from_memory(ui.ctx(), ui.style());
                code_view_ui(
                    ui,
                    &theme,
                    &serde_json::to_string_pretty(&data).unwrap_or_default(),
                    "json",
                );
            };

            if ui.button("Close").clicked() {
                ui.close();
            }
        });

        if modal.should_close() {
            *viewing_packet = None;
        }
    }
}

pub fn packet_debug(
    ctx: &egui::Context,
    tx: &ContextSender<Action>,
    show: &mut bool,
    packets: &Option<Result<Vec<AvocadoPacket>, ProtocolError>>,
) {
    egui::Window::new("Saved Packet Debugger")
        .open(show)
        .default_width(480.0)
        .default_height(320.0)
        .resizable([true, true])
        .scroll(true)
        .show(ctx, |ui| {
            if ui.button("Select File").clicked() {
                let ctx = ctx.clone();
                let tx = tx.clone();

                spawn(async move {
                    let file = rfd::AsyncFileDialog::new().pick_file().await;
                    if let Some(file) = file {
                        let data = file.read().await;

                        let mut maybe_hex_data = data.clone();
                        maybe_hex_data.retain(|c| !c.is_ascii_whitespace());

                        let data = hex::decode(&maybe_hex_data).unwrap_or(data);
                        debug!("processed data: {}", hex::encode(&data));

                        let cursor = Cursor::new(data);
                        let avocado_packets: Result<Vec<_>, _> =
                            AvocadoPacketReader::new(cursor).collect();

                        let _ = tx.send(Action::LoadedAvocadoPackets(avocado_packets));
                        ctx.request_repaint();
                    }
                });
            }

            match packets {
                Some(Ok(packets)) => {
                    let has_exactly_one = packets.len() == 1;

                    for (index, packet) in packets.iter().enumerate() {
                        packet_details(ui, has_exactly_one, index, packet);
                    }
                }
                Some(Err(err)) => {
                    ui.label(format!("Error! {err}"));
                }
                None => {
                    ui.label("No packets loaded");
                }
            }
        });
}

fn packet_details(ui: &mut Ui, has_exactly_one: bool, index: usize, packet: &AvocadoPacket) {
    egui::CollapsingHeader::new(format!("Packet {}", index + 1))
        .default_open(has_exactly_one)
        .show(ui, |ui| {
            let theme = CodeTheme::from_memory(ui.ctx(), ui.style());
            ui.style_mut().spacing.item_spacing = Vec2::new(8.0, 16.0);

            code_view_ui(
                ui,
                &theme,
                &serde_json::to_string_pretty(packet).unwrap_or_default(),
                "json",
            );

            ui.heading("Packet Data (hex)");
            pretty_hex(format!("packet-{index}"), ui, &packet.data);

            if let Some(data) = packet.as_json::<serde_json::Value>() {
                ui.heading("Packet Data (json)");
                code_view_ui(
                    ui,
                    &theme,
                    &serde_json::to_string_pretty(&data).unwrap_or_default(),
                    "json",
                );
            }
        });
}

struct EnumeratedItem<T> {
    item: T,
    index: usize,
    id: Id,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ArtworkMenuCommand {
    Duplicate,
    BringToFront,
    BringForward,
    SendBackward,
    SendToBack,
    RotateClockwise,
    RotateCounterclockwise,
    FlipHorizontal,
    FlipVertical,
    SetVisible(bool),
    SetLocked(bool),
    SetCutting(bool),
    Remove,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ArtworkMenuAction {
    pub image_ids: Vec<String>,
    pub command: ArtworkMenuCommand,
}

#[derive(Clone, Debug)]
pub(crate) struct ArtworkMenuContext {
    image_ids: Vec<String>,
    count: usize,
    any_locked: bool,
    all_locked: bool,
    all_visible: bool,
    all_cutting: bool,
    can_move_forward: bool,
    can_move_backward: bool,
    can_cut: bool,
}

impl ArtworkMenuContext {
    pub(crate) fn new(
        images: &[LoadedImage],
        selected_images: &[usize],
        mode_type: ModeType,
    ) -> Option<Self> {
        let selected = selected_images
            .iter()
            .copied()
            .filter(|index| *index < images.len())
            .collect::<BTreeSet<_>>();
        if selected.is_empty() {
            return None;
        }
        let chosen = selected
            .iter()
            .map(|index| &images[*index])
            .collect::<Vec<_>>();
        let first = *selected.first().expect("selection is non-empty");
        let last = *selected.last().expect("selection is non-empty");

        Some(Self {
            image_ids: chosen.iter().map(|image| image.id.clone()).collect(),
            count: chosen.len(),
            any_locked: chosen.iter().any(|image| image.locked),
            all_locked: chosen.iter().all(|image| image.locked),
            all_visible: chosen.iter().all(|image| image.visible),
            all_cutting: chosen.iter().all(|image| image.enable_cutting),
            can_move_forward: images
                .iter()
                .enumerate()
                .skip(last + 1)
                .any(|(index, _)| !selected.contains(&index)),
            can_move_backward: images
                .iter()
                .enumerate()
                .take(first)
                .any(|(index, _)| !selected.contains(&index)),
            can_cut: mode_type.has_cutting(),
        })
    }

    fn single(image: &LoadedImage, index: usize, image_count: usize, mode_type: ModeType) -> Self {
        Self {
            image_ids: vec![image.id.clone()],
            count: 1,
            any_locked: image.locked,
            all_locked: image.locked,
            all_visible: image.visible,
            all_cutting: image.enable_cutting,
            can_move_forward: index + 1 < image_count,
            can_move_backward: index > 0,
            can_cut: mode_type.has_cutting(),
        }
    }

    fn action(&self, command: ArtworkMenuCommand) -> ArtworkMenuAction {
        ArtworkMenuAction {
            image_ids: self.image_ids.clone(),
            command,
        }
    }
}

pub(crate) fn artwork_context_menu(
    ui: &mut Ui,
    context: &ArtworkMenuContext,
) -> Option<ArtworkMenuAction> {
    let mut action = None;
    let multiple = context.count > 1;
    let duplicate_label = if multiple {
        format!("Duplicate {} selected", context.count)
    } else {
        "Duplicate".to_owned()
    };
    if ui.button(duplicate_label).clicked() {
        action = Some(context.action(ArtworkMenuCommand::Duplicate));
    }

    ui.separator();
    ui.add_enabled_ui(!context.any_locked, |ui| {
        ui.menu_button("Arrange", |ui| {
            if ui
                .add_enabled(
                    context.can_move_forward,
                    egui::Button::new("Bring to front"),
                )
                .clicked()
            {
                action = Some(context.action(ArtworkMenuCommand::BringToFront));
            }
            if ui
                .add_enabled(context.can_move_forward, egui::Button::new("Bring forward"))
                .clicked()
            {
                action = Some(context.action(ArtworkMenuCommand::BringForward));
            }
            if ui
                .add_enabled(
                    context.can_move_backward,
                    egui::Button::new("Send backward"),
                )
                .clicked()
            {
                action = Some(context.action(ArtworkMenuCommand::SendBackward));
            }
            if ui
                .add_enabled(context.can_move_backward, egui::Button::new("Send to back"))
                .clicked()
            {
                action = Some(context.action(ArtworkMenuCommand::SendToBack));
            }
        });
        ui.menu_button("Transform", |ui| {
            if ui.button("Rotate 90° clockwise").clicked() {
                action = Some(context.action(ArtworkMenuCommand::RotateClockwise));
            }
            if ui.button("Rotate 90° counterclockwise").clicked() {
                action = Some(context.action(ArtworkMenuCommand::RotateCounterclockwise));
            }
            ui.separator();
            if ui.button("Flip horizontal").clicked() {
                action = Some(context.action(ArtworkMenuCommand::FlipHorizontal));
            }
            if ui.button("Flip vertical").clicked() {
                action = Some(context.action(ArtworkMenuCommand::FlipVertical));
            }
        });
    });

    ui.separator();
    let visibility_label = match (multiple, context.all_visible) {
        (true, true) => "Hide selected",
        (true, false) => "Show selected",
        (false, true) => "Hide artwork",
        (false, false) => "Show artwork",
    };
    if ui.button(visibility_label).clicked() {
        action = Some(context.action(ArtworkMenuCommand::SetVisible(!context.all_visible)));
    }
    let lock_label = match (multiple, context.all_locked) {
        (true, true) => "Unlock selected",
        (true, false) => "Lock selected",
        (false, true) => "Unlock artwork",
        (false, false) => "Lock artwork",
    };
    if ui.button(lock_label).clicked() {
        action = Some(context.action(ArtworkMenuCommand::SetLocked(!context.all_locked)));
    }
    if context.can_cut {
        let cut_label = match (multiple, context.all_cutting) {
            (true, true) => "Exclude selected from cutlines",
            (true, false) => "Include selected in cutlines",
            (false, true) => "Exclude from cutlines",
            (false, false) => "Include in cutlines",
        };
        if ui.button(cut_label).clicked() {
            action = Some(context.action(ArtworkMenuCommand::SetCutting(!context.all_cutting)));
        }
    }

    ui.separator();
    let remove_label = if multiple {
        format!("Remove {} from sheet", context.count)
    } else {
        "Remove from sheet".to_owned()
    };
    let remove = ui.add_enabled(
        !context.any_locked,
        egui::Button::new(egui::RichText::new(remove_label).color(ui.visuals().error_fg_color)),
    );
    if remove.clicked() {
        action = Some(context.action(ArtworkMenuCommand::Remove));
    }
    remove.on_disabled_hover_text("Unlock selected artwork to remove it");

    if action.is_some() {
        ui.close();
    }
    action
}

impl<T> DragDropItem for EnumeratedItem<T> {
    fn id(&self) -> Id {
        self.id
    }
}

#[allow(
    clippy::ptr_arg,
    reason = "egui_dnd reordering updates the backing Vec"
)]
pub fn loaded_images(
    ui: &mut Ui,
    loaded_images: &mut Vec<LoadedImage>,
    selected_images: &mut Vec<usize>,
    mode_type: ModeType,
) -> (bool, Option<ArtworkMenuAction>) {
    ui.heading("Layers");

    let mut changed = false;
    let mut select = None;
    let mut menu_action = None;
    let selected_ids = selected_images
        .iter()
        .filter_map(|index| loaded_images.get(*index))
        .map(|image| image.id.clone())
        .collect::<BTreeSet<_>>();
    let menu_context = ArtworkMenuContext::new(loaded_images, selected_images, mode_type);
    let image_count = loaded_images.len();

    ui.spacing_mut().scroll.floating = false;

    egui::ScrollArea::vertical()
        .auto_shrink([false, true])
        .show(ui, |ui| {
            let response = dnd(ui, "Images").with_animation_time(0.0).show(
                loaded_images
                    .iter_mut()
                    .enumerate()
                    .map(|(index, item)| EnumeratedItem {
                        id: Id::new(("layer", item.id.clone())),
                        item,
                        index,
                    }),
                |ui, EnumeratedItem { item, index, .. }, handle, _dragging| {
                    let interaction = image_controls(
                        ui,
                        item,
                        handle,
                        mode_type,
                        selected_ids.contains(&item.id),
                        menu_context.as_ref(),
                        index,
                        image_count,
                    );
                    if interaction.select {
                        select = Some(index);
                    }
                    if interaction.menu_action.is_some() {
                        menu_action = interaction.menu_action;
                    }
                    changed |= interaction.changed;
                    ui.add_space(6.0);
                },
            );

            if response.is_drag_finished() {
                response.update_vec(loaded_images);
                *selected_images = loaded_images
                    .iter()
                    .enumerate()
                    .filter_map(|(index, image)| selected_ids.contains(&image.id).then_some(index))
                    .collect();
                changed = true;
            }
        });

    if let Some(index) = select {
        *selected_images = vec![index];
    }
    (changed, menu_action)
}

struct ImageControlsInteraction {
    select: bool,
    changed: bool,
    menu_action: Option<ArtworkMenuAction>,
}

#[allow(clippy::too_many_arguments)]
fn image_controls(
    ui: &mut Ui,
    image: &mut LoadedImage,
    handle: Handle<'_>,
    mode_type: ModeType,
    active: bool,
    menu_context: Option<&ArtworkMenuContext>,
    index: usize,
    image_count: usize,
) -> ImageControlsInteraction {
    let mut interaction = ImageControlsInteraction {
        select: false,
        changed: false,
        menu_action: None,
    };
    let own_context = ArtworkMenuContext::single(image, index, image_count, mode_type);
    let context = if active {
        menu_context.unwrap_or(&own_context)
    } else {
        &own_context
    };
    let selected_fill = ui.visuals().selection.bg_fill;
    let selected_stroke = ui.visuals().selection.stroke;
    egui::Frame::new()
        .fill(if active {
            selected_fill
        } else {
            egui::Color32::TRANSPARENT
        })
        .stroke(if active {
            selected_stroke
        } else {
            egui::Stroke::NONE
        })
        .corner_radius(egui::CornerRadius::same(8))
        .inner_margin(egui::Margin::symmetric(6, 5))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let preview = handle.sense(egui::Sense::click_and_drag()).ui(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(crate::icons::DOTS_SIX_VERTICAL).size(16.0));
                        let (response, painter) =
                            ui.allocate_painter(Vec2::splat(44.0), egui::Sense::empty());
                        response.widget_info(|| {
                            egui::WidgetInfo::labeled(
                                egui::WidgetType::Image,
                                true,
                                format!("Layer preview: {}", image.name),
                            )
                        });

                        painter.image(
                            image.sized_texture.id,
                            response.rect,
                            egui::Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(1.0, 1.0)),
                            egui::Color32::WHITE,
                        );
                    });
                });
                let preview = preview.on_hover_text(format!("Drag to reorder {}", image.name));
                preview.widget_info(|| {
                    egui::WidgetInfo::selected(
                        egui::WidgetType::Button,
                        true,
                        active,
                        format!("Select layer {} from thumbnail", image.name),
                    )
                });
                let preview_secondary_clicked = ui.input(|input| {
                    input.pointer.secondary_clicked()
                        && input
                            .pointer
                            .interact_pos()
                            .is_some_and(|pointer| preview.rect.contains(pointer))
                });
                if (preview.clicked() || preview_secondary_clicked) && !active {
                    interaction.select = true;
                }
                let mut popup = egui::Popup::context_menu(&preview);
                if preview_secondary_clicked {
                    popup = popup.open_memory(egui::SetOpenCommand::Bool(true));
                }
                popup.show(|ui| {
                    if let Some(action) = artwork_context_menu(ui, context) {
                        interaction.menu_action = Some(action);
                    }
                });
                let name_response = ui
                    .vertical(|ui| {
                        ui.set_min_width(92.0);
                        let name = ui.add(
                            egui::Label::new(egui::RichText::new(&image.name).strong())
                                .sense(egui::Sense::click()),
                        );
                        ui.small(format!("{:.0} × {:.0} px", image.size().x, image.size().y));
                        name
                    })
                    .inner;
                name_response.widget_info(|| {
                    egui::WidgetInfo::selected(
                        egui::WidgetType::Button,
                        true,
                        active,
                        format!("Select layer {}", image.name),
                    )
                });
                if name_response.clicked() && !active {
                    interaction.select = true;
                }

                let visibility_label = if image.visible {
                    format!("Hide {}", image.name)
                } else {
                    format!("Show {}", image.name)
                };
                if crate::theme::icon_toggle(
                    ui,
                    if image.visible {
                        crate::icons::EYE
                    } else {
                        crate::icons::EYE_SLASH
                    },
                    image.visible,
                    visibility_label.clone(),
                    visibility_label,
                )
                .clicked()
                {
                    image.visible = !image.visible;
                    interaction.changed = true;
                }

                let lock_label = if image.locked {
                    format!("Unlock {}", image.name)
                } else {
                    format!("Lock {}", image.name)
                };
                if crate::theme::icon_toggle(
                    ui,
                    if image.locked {
                        crate::icons::LOCK
                    } else {
                        crate::icons::LOCK_OPEN
                    },
                    image.locked,
                    lock_label.clone(),
                    lock_label,
                )
                .clicked()
                {
                    image.locked = !image.locked;
                    interaction.changed = true;
                }

                ui.spacing_mut().interact_size = Vec2::splat(32.0);
                let menu = ui.menu_button(
                    egui::RichText::new(crate::icons::DOTS_THREE).size(17.0),
                    |ui| {
                        if let Some(action) = artwork_context_menu(ui, context) {
                            interaction.menu_action = Some(action);
                        }
                    },
                );
                let actions_label = format!("Actions for layer {}", image.name);
                menu.response.widget_info(|| {
                    egui::WidgetInfo::labeled(egui::WidgetType::Button, true, actions_label.clone())
                });
                menu.response.on_hover_text(actions_label);
            });
        });
    interaction
}

pub fn px_slider<'a>(
    value: &'a mut f32,
    dpi: f32,
    range: RangeInclusive<f32>,
) -> egui::DragValue<'a> {
    egui::DragValue::new(value)
        .max_decimals(0)
        .suffix(" px")
        .range(range)
        .custom_parser(move |val| {
            let lower = val.trim().to_ascii_lowercase();

            if let Some(val) = lower.strip_suffix("in") {
                val.trim().parse().map(|val: f64| val * f64::from(dpi)).ok()
            } else {
                val.strip_suffix("px").unwrap_or(&lower).trim().parse().ok()
            }
        })
}

pub fn cut_controls(
    ui: &mut Ui,
    dpi: f32,
    cut_tuning: &mut CutTuning,
    progress: Option<(usize, usize)>,
    has_intersections: bool,
    off_canvas: bool,
) {
    ui.heading("Cut Preparation");

    let progress_pct = progress
        .map(|(completed, total)| completed as f32 / total as f32)
        .unwrap_or(0.0);

    ui.add_visible(
        progress.is_some(),
        ProgressBar::new(progress_pct)
            .animate(progress.is_some())
            .show_percentage(),
    );

    ui.checkbox(&mut cut_tuning.internal, "Allow Internal Cuts");
    ui.checkbox(&mut cut_tuning.white_transparent, "Make White Transparent");

    let mut buffer = cut_tuning.buffer / dpi * 25.4;
    ui.add(
        egui::Slider::new(&mut buffer, -1.0..=5.0)
            .suffix(" mm")
            .text("Padding Distance"),
    )
    .on_hover_text("Padding between the edges of the sticker and the cutline");
    cut_tuning.buffer = buffer * dpi / 25.4;

    let mut minimum_length = cut_tuning.minimum_length / dpi;
    ui.add(
        egui::Slider::new(&mut minimum_length, 0.05..=1.0)
            .suffix(" in")
            .text("Minimum Cut Length"),
    )
    .on_hover_text("Minimum length to cut, anything smaller will be ignored");
    cut_tuning.minimum_length = minimum_length * dpi;

    ui.collapsing("Advanced Settings", |ui| {
        ui.add(egui::Slider::new(&mut cut_tuning.simplify, 0.0..=5.0).text("Simplify Amount"))
            .on_hover_text("Simplification epsilon, decreases total number of line segments");

        ui.horizontal(|ui| {
            ui.add(
                egui::DragValue::new(&mut cut_tuning.smoothing)
                    .range(0..=10)
                    .speed(0.05),
            );
            ui.label("Smoothing Steps");
        })
        .response
        .on_hover_text("Increases number of smoothing iterations");
    });

    let error_messages: Vec<_> = [
        has_intersections.then_some("Cut Lines Overlap"),
        off_canvas.then_some("Cut Lines Out of Bounds"),
    ]
    .into_iter()
    .flatten()
    .collect();

    if error_messages.is_empty() {
        ui.add_visible(
            false,
            egui::Label::new(
                egui::RichText::new("Error Message")
                    .strong()
                    .color(egui::Color32::RED),
            ),
        );
    } else {
        ui.horizontal(|ui| {
            for message in error_messages {
                ui.add(egui::Label::new(
                    egui::RichText::new(message)
                        .strong()
                        .color(egui::Color32::RED),
                ));
            }
        });
    }
}
