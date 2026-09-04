# Feature reachability audit

This audit maps the checked README features to the controls a user can
actually reach. “Implemented” means the UI reaches the production code; it
does not mean that every printer transport has been exercised on hardware.

## Terms used in this audit

- **In-process** means the operation runs inside Sapodilla without uploading
  the artwork. It does not mean that every supporting resource is bundled; the
  native neural remover downloads its model on first use.
- **Native desktop** means the compiled Windows/macOS application rather than
  the browser/WebAssembly build.
- **In-session** means state is held only by the running application and is not
  restored after restart unless it is explicitly saved in a document.

## Contextual controls

- Select artwork on the canvas or in **Layers** to reveal adjustments,
  background removal, alignment, locking, visibility, replacement, and
  template-fit controls.
- Select **Print & Cut** under **Inspector → Settings → Mode** to reveal cutline
  generation, path editing, perforation, peel tabs, overcut, SVG import, and
  the procedural shape designer. Print-only mode now links directly to it.
- The **Library** and **Inspector** toolbar buttons show or hide their panels.
- Folder watching and neural background removal are native-desktop features.

## Audit matrix

| README capability | UI path | Audit result |
| --- | --- | --- |
| Connect to device | **Connection → Transport**, or **Inspector → Connection → Add printer → Transport**; refresh/select/connect | Implemented. Native USB, BLE, serial, and browser Web Serial remain platform-specific. |
| Device status | **Inspector → Connection** | Implemented as the latest device/job status. |
| Image upload | **+ Add artwork**, **Canvas → Add Image**, drag/drop, or `Ctrl/Cmd+Shift+U` | Implemented. Picker now accepts `.jpg`, `.jpeg`, and `.png`. |
| Placement, scale, rotation | Select artwork and use the canvas body/handles; numeric rotation is under **Inspector → Selection** | Implemented, including Shift snapping, Alt/Option center scaling, and Escape rollback. |
| Locking and visibility | **Inspector → Layers** or **Selection** | Implemented. |
| Rulers and grid | Toolbar/More toggles; spacing under **Inspector → Canvas** | Implemented and DPI-aware. |
| Layers and multi-selection | Drag rows under **Inspector → Layers**; Shift/Cmd selection on canvas | Implemented. |
| Sheet alignment | **Inspector → Selection → Align to sheet** | Implemented; rotated artwork aligns by its visible bounds. |
| Smart snapping | Toolbar/More and **Inspector → Canvas** | Implemented. |
| Image adjustments | Select one artwork → **Image adjustments** | Implemented. |
| Edge background removal | Select one artwork → **Background removal** | Runs in-process on the CPU without uploading artwork. |
| Neural background removal | Native desktop, select artwork → **AI background removal** | Runs inference on-device only with the native `background-ml` feature. First use downloads the model; later runs reuse the disk file and process session. |
| Auto-pack | Toolbar **Auto-pack**; gap/rotation settings under **Inspector → Canvas** | Implemented. |
| Cutline preview and generation | Select **Print & Cut** mode → toolbar preview / **Cut Preparation** | Implemented. Print mode displays a direct switch instead of silently hiding the workflow. |
| Cutline nodes and paths | **Edit nodes** plus **Inspector → Editable path** | Implemented for multiple paths. |
| Per-path operations | **Inspector → Editable path → Cut operation** | Implemented for kiss cut, perforation, and disabled. |
| Smoothing and union | **Inspector → Cut Preparation** | Implemented with closed/unlocked path guards. |
| SVG import | **Canvas → Import SVG Cutlines** or **Cut Preparation → Import SVG** | Implemented for cut geometry, not SVG artwork rendering. |
| Procedural shapes | **Print & Cut → Cut Preparation → Shape designer** | Implemented with 18 shapes. |
| Perforation, peel tabs, overcut, materials | **Inspector → Material / Cut Preparation** | Implemented. |
| Local asset library | **Library** panel | Implemented for imported assets in the current session. |
| Watched folder library | Native desktop **Library → Import folder / Watched folders** | Implemented with recursive startup and rescan refresh; unavailable in the browser. |
| Fill and shuffle fill | **Library → Fill sheet / Shuffle fill** | Implemented with deterministic cycle tracking. |
| Sapodilla document persistence | **File → Open / Save As** | Implemented using one version-1 `.sapodilla` JSON project format. The payload kind distinguishes sticker, sheet, and template projects. |
| Template slots | Select artwork → **Template slot fit** before saving a template; reopened templates expose slot assignment/replacement | Implemented with Contain, Cover, and Stretch plus stable IDs and locked template cutlines. |
| Exports | **File → Export** | Implemented for PNG, artwork PDF, cutline SVG, PLT, and debug toolpath SVG. |
| Multiple printers | Native **Inspector → Connection → Add printer** | Implemented for simultaneous connections. Connected printers currently advertise the same print/cut capabilities, so routing is automatic but not model-derived. |
| Production queue | **Inspector → Production queue (count)** | Implemented in-session. Dispatch selects the oldest currently routable job rather than enforcing strict head-of-line FIFO. |
| Queue lifecycle | Expand **Production queue** | Progress, failure, retry, offline transitions, and cancellation before device dispatch are implemented. Running device-job cancellation is not exposed. |
| Photo print and copies | Select **Print** mode; set **Copies**; connect and print | Implemented. |
| Print and cut | Select **Print & Cut**, generate valid cutlines, connect and print | Implemented as a combined print/cut job. |

## Automated coverage

The native test suite uses `egui_kittest` to query the same AccessKit semantics
that egui exposes to assistive technology. These tests cover wide and compact
entry points, contextual image tools, Print-to-Print-&-Cut discovery, template
fit selection, transport visibility, queue counts, and rotated alignment.

The existing Playwright harness remains useful for WebAssembly/WebGL startup,
browser file upload, resize behavior, and rendered canvas gestures. Browser
automation cannot query individual egui widgets through DOM selectors because
the application is rendered into one canvas, so semantic widget coverage
belongs in `egui_kittest` while custom-painted gestures retain direct
`egui::RawInput` tests.
