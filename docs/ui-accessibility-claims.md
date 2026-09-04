# UI accessibility and workspace claims

These claims are executable rather than screenshot-only. The egui accessibility
tree is exercised with `egui_kittest`, so tests locate controls by role and
accessible name and invoke actions through AccessKit where relevant.

| Product claim | Automated evidence |
| --- | --- |
| The workspace exposes a named `Pane` and placed artwork exposes a named `Image`. | `app::ui_tests::imported_artwork_has_named_elements_and_a_usable_native_viewport` |
| Artwork can be selected through its accessibility action, not only by a coordinate-based pointer click. | `app::ui_tests::artwork_can_be_selected_through_accesskit` |
| A library thumbnail has an action-specific button name and can add its asset through AccessKit. | `app::ui_tests::library_thumbnail_button_has_an_action_specific_accessible_name` |
| Primary workspace controls are named, enabled, focusable, clickable through AccessKit, and at least 32×32 points. | `app::ui_tests::primary_workspace_controls_are_named_actionable_and_easy_to_target` |
| The selected-artwork name field and contextual image tools have associated roles and names. | `app::ui_tests::selected_artwork_exposes_image_tools_and_template_fit` |
| Layer name, X, Y, width, height, and rotation inputs are associated with accessible names. | `app::ui_tests::layer_transform_fields_have_associated_accessible_names` |
| At the 1280×800 native window size, the artwork canvas is at least 700×500 points and normalized sample artwork is at least 140×80 points on screen. | `app::ui_tests::imported_artwork_has_named_elements_and_a_usable_native_viewport` |
| At 700×720, the canvas remains at least 600×450 points, sample artwork remains at least 120×70 points, and Fit sheet is reachable. | `app::ui_tests::compact_import_keeps_canvas_artwork_and_fit_control_reachable` |
| Small and oversized raster sources are normalized into a centered, finite, aspect-preserving initial placement bounded by the sheet's usable-area policy. | `app::tests::new_artwork_normalization_handles_small_and_oversized_sources` |

Run the semantic UI suite with:

```powershell
cargo test --no-default-features app::ui_tests
```

These checks complement manual visual review. They intentionally assert stable
product outcomes (roles, names, actions, and useful bounds) rather than colors
or exact pixel snapshots.
