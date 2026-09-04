# Print-and-cut calibration implementation plan

Date: 2026-09-04

## Decision summary

Sapodilla will provide two calibration workflows behind one shared calibration
engine:

1. **Flatbed scanner** automatically measures high-contrast apertures left by
   removed, bridged through-cut targets.
2. **Manual** guides the operator through the seven-target measurement method
   publicly documented by East Bay Makers Club.

Both workflows generate their own deterministic print/cut jobs, create the same
kind of print-to-cut observations, fit the simplest correction supported by the
measurements, require a newly printed validation sheet, and save a profile for
the physical printer rather than the artwork document.

The first release imports an image created by the user's scanner software. It
does not integrate directly with TWAIN, WIA, Image Capture, or SANE. Direct
scanner control and device-side calibration writes are later extension points.

## Goals

- Correct systematic cut displacement, independent carriage/feed scale,
  rotation, and skew relative to the printed design.
- Separate measurable systematic error from sheet-loading variation and blade
  mechanics.
- Keep calibration profiles isolated by physical printer, firmware, and media;
  retain measurement and validation cut settings as provenance, and add a
  narrower material/settings scope only if hardware data proves it necessary.
- Never replace an active profile until a new profile passes validation.
- Preserve the current stock mapping as a resettable fallback.
- Produce enough run data to reproduce a result and compare it across releases.

## Non-goals for the first release

- Correcting the sheet that has already been printed and cut.
- Direct flatbed-scanner control.
- Phone-camera perspective and lens correction.
- Writing calibration into printer firmware before the official command,
  bounds, persistence, readback, and reset behavior have been captured.
- Automatically applying East Bay Makers Club's printer-specific scale or
  margin values to other printers.
- Fitting a nonlinear warp before repeated hardware results show a stable
  residual pattern that an affine model cannot explain.

## Attribution and clean-room boundary

The Manual workflow is based on the methodology documented in the
[East Bay Makers Club `pixcut-s1` calibration workflow](https://github.com/eastbaymakersclub/pixcut-s1#calibration-workflow).
The source inspected for this plan was repository commit
`a73cd65b7374e5b9e1eb7a0c54594f3276c14f8f`; the referenced generator was
inspected at commit
[`f71a8f882b2462990e95554a0927a6e1d9860993`](https://github.com/eastbaymakersclub/pixcut-s1/blob/f71a8f882b2462990e95554a0927a6e1d9860993/generate_calibration_sheet.py).

The upstream repository does not currently contain a `LICENSE`, `COPYING`, or
notice file. Sapodilla must therefore independently implement the functional
method in Rust with original artwork and UI copy. Do not copy or translate the
Python source, its generated artwork, or its prose without receiving an
explicit license.

Required visible copy on the Manual method card:

> Guided measurements based on the seven-target method publicly documented by
> East Bay Makers Club.

Required linked footer on the Manual results screen:

> Method credit: East Bay Makers Club · pixcut-s1

When the workflow ships, add this factual entry to `THIRD_PARTY_NOTICES.md`:

> Sapodilla's Manual calibration workflow is an independent implementation
> informed by the seven-target calibration method documented by East Bay
> Makers Club in `eastbaymakersclub/pixcut-s1`, inspected at commit
> `a73cd65b7374e5b9e1eb7a0c54594f3276c14f8f`. No upstream source code or
> graphical assets are included.

Do not append a license text unless upstream declares one or separately grants
one.

## Existing integration constraints

- `src/protocol.rs` stores one model-wide `CutterCalibration` containing only a
  uniform scale and translation.
- `src/app.rs::encode_plt` applies that mapping after the Y mirror and swaps the
  axes while formatting PLT coordinates. Calibration work must replace this
  implicit sequence with one documented canvas-to-plotter transform and one
  final integer quantization.
- `PendingPrintJob` currently contains an already encoded PLT before the queue
  selects a physical printer. Production PLT encoding must move to dispatch, or
  a job must be restricted to one printer, before per-printer profiles can be
  correct.
- `JobSpec::restricted_to` already supports targeting calibration and
  validation jobs at the printer selected in the wizard.
- Existing eframe persistence is suitable for versioned profiles and resumable
  wizard drafts. Profiles do not belong in `.sapodilla` artwork documents.
- Raster processing must run through `spawn_blocking`; a 600 DPI 4x7 scan is
  roughly 2400x4200 pixels.
- The current image dependencies decode PNG and JPEG but not TIFF. The first
  release should request lossless PNG and optionally accept JPEG. TIFF support
  can be added deliberately rather than promised by the wizard.
- `MaterialProfile::speed` is currently stored and displayed but is not emitted
  by PLT/job serialization. Treat it as uncontrolled metadata: do not claim
  that calibration and production speed are matched, and do not use speed in
  profile selection or acceptance until a protocol command is implemented and
  verified on hardware.

## Proposed module layout

```text
src/
  calibration/
    mod.rs            public domain types and orchestration
    profile.rs        versioned persistence and identity matching
    transform.rs      affine models, composition, inversion, and units
    solver.rs         robust fitting, model selection, and metrics
    targets.rs        original Manual and Flatbed target generators
    scan.rs           scan normalization, detection, and confidence
    report.rs         run manifests and human-readable diagnostics
  app/
    calibration_ui.rs walkthrough modal and action handling
tests/
  fixtures/
    calibration/      synthetic scan fixtures and expected detections
```

Keep target generation and solving independent of egui so they can be unit
tested and later reused by a CLI or hardware harness.

## Core data model

The exact Rust representation may vary, but it must preserve these concepts:

```rust
enum CalibrationMethod {
    FlatbedScanner,
    ManualEastBay,
}

struct PrinterCalibrationKey {
    serial_number: String,
    model: String,
    firmware_revision: String,
    media_size: u16,
    media_type: u16,
}

struct CalibrationObservation {
    target_id: String,
    nominal_print_mm: [f64; 2],
    observed_cut_mm: [f64; 2],
    uncertainty_mm: [f64; 2],
    confidence: f64,
    included: bool,
}

struct CanvasToPlotter {
    matrix: [[f64; 2]; 2],
    translation: [f64; 2],
}

struct CalibrationProfile {
    version: u8,
    key: PrinterCalibrationKey,
    method: CalibrationMethod,
    canvas_to_plotter: CanvasToPlotter,
    baseline_mapping_id: String,
    created_at: u64,
    validation: ValidationMetrics,
    previous_profile_id: Option<String>,
}
```

Record measurement and validation pressure, cut direction/path order, and cut
mode as profile provenance. A configured speed value may be recorded as
informational only while the protocol does not apply it. Do not key a
scanner-created profile to the temporary
through-cut mode: it is a print-to-cutter registration profile whose production
applicability is established by the required kiss-cut validation. If hardware
tests demonstrate a repeatable material- or pressure-dependent displacement,
add an optional material/cut-settings scope without duplicating the underlying
printer identity.

Also persist a resumable `CalibrationRun` containing the selected printer,
baseline profile, generated target manifest, queue/device job identifiers,
measurements or scan detections, exclusions, fit candidates, and validation
state. Cap and sanitize persisted runs and profiles; reject non-finite,
singular, implausibly scaled, or unknown-version transforms.

Query `serial-number`, `model`, and `firmware-revision` after connection. If a
transport cannot provide a serial, require the user to select a named fallback
profile rather than silently treating a transient USB bus address as permanent.
Mark a profile stale after a firmware change and offer validation before reuse.

## Transform and solver contract

Use unmirrored canvas coordinates as the only public input to calibration. The
stock mapping becomes an explicit `CanvasToPlotter` baseline rather than a
scale and offset applied around hidden mirror/swap operations.

For a calibration job, let `p` be the desired printed center and let the
current baseline mapping command a cut. Measurements produce an observed cut
center `q`. Fit the physical response:

```text
q = A p + b
```

Future cut commands must pre-compensate in canvas space:

```text
p_command = inverse(A) (p_desired - b)
plotter_command = baseline_mapping(p_command)
```

Compose those operations and persist the resulting direct canvas-to-plotter
mapping. Do not apply a fitted forward error transform, and do not apply the
old calibration again after the composed mapping.

Fit candidates in this order:

1. Translation only.
2. Independent X/Y scale plus translation.
3. Six-parameter affine mapping when point count, spatial coverage, and
   measurement precision support it.

Use robust weighted least squares with an outlier-resistant loss. Select the
simplest model whose held-out or leave-one-out error is materially better than
the simpler candidate. Report RMS, p95, maximum, mean X/Y, per-target residual,
condition, and measurement uncertainty. A nonlinear feed correction remains
disabled until the same residual structure repeats over multiple sheet loads.

### Model eligibility and coverage

Keep these rules in a tested calibration policy rather than deriving them in
the UI:

- Translation requires at least four accepted targets spanning both the left
  and right portions of the sheet and at least two separated Y rows.
- Independent X/Y scale plus translation requires at least six accepted
  targets, both outer X columns, the top and bottom rows, and a sufficiently
  conditioned normalized design matrix.
- Full affine requires at least eight accepted observations across at least six
  distinct target positions, with all four quadrants represented. A single
  seven-target Manual sheet is therefore not eligible; Manual can unlock
  affine only after a second independently loaded sheet supplies at least
  twelve accepted observations in total, with at least six accepted from each
  sheet.
- Promote to a more complex candidate only when leave-one-out or held-out p95
  improves by both at least 10 percent and 0.05 mm, subject to hardware tuning.
  Otherwise retain the simpler model.
- Flatbed analysis still requires at least eight automatic detections, all four
  quadrants, and its detector/conditioning gates even when a simpler model
  would mathematically fit fewer points.

The five-target Manual validation sheet requires at least four valid targets
covering both outer X regions and both outer Y regions. The Flatbed validation
layout requires at least six valid apertures covering all four quadrants. Any
validation target above the policy's maximum-error limit fails activation even
when aggregate RMS improves.

## Shared target-generation requirements

- Generate print raster and cut paths from one manifest expressed in physical
  millimeters and converted to the 1200x2100, 300 DPI canvas exactly once.
- Generate cut paths directly as vectors; never involve automatic artwork
  contour extraction.
- Include target IDs, orientation marks, mapping/profile version, and a run ID.
  Encode the run, target revision, method, and calibration-versus-validation
  purpose into a scanner-readable high-contrast token; reject an imported scan
  when the decoded token does not match the current wizard step. Keep the full
  token and its quiet surround below the device's 2 mm top imaging guard.
  Validation tokens also include a candidate generation that advances whenever
  training input changes; invalidate the old validation job, measurements,
  scan, kiss-cut confirmation, payload hashes, and dispatched-command evidence
  at the same boundary. Carry that generation through asynchronous target-job
  preparation, scan analysis, and solving, and discard completions from an
  older generation.
- Preserve a preview overlay for troubleshooting without using it as measured
  input.
- Record the exact JPEG hash, PLT hash, ordered integer move/draw coordinates
  parsed from the final dispatched PLT, material settings, target manifest,
  profile baseline, and physical printer identity in the run report.
- Restrict every calibration and validation queue job to the selected printer.
- Do not let ordinary artwork safe-area checks silently move or resize a
  calibration target. Validate the special calibration layout against its own
  hardware-tested bounds.

## Flatbed Scanner target

Use 12–16 stations distributed through the usable sheet area. Each station
contains:

- An original 18–24 mm dark, saturated printed patch.
- A printed outer ring and four or eight ticks defining the local printed
  center.
- An 8–12 mm circular through-cut aperture.
- Three or four symmetric micro-bridges at recorded angles so the slug remains
  in the sheet until unloading.
- Enough margin that a coarse offset still leaves the aperture inside the
  printed patch.

The target generator should emit clean circle arcs separated by explicit
blade-up bridge gaps rather than applying the normal repeating perforation
dash pattern. Use the lowest confirmed full-through pressure, keep the sheet
structurally connected, and cut the aperture phase last. Circle centers drive
registration; radius, ellipticity, seam behavior, and bridge tears are
diagnostics.

### Scan analysis pipeline

1. Decode the imported scan off the UI thread and validate dimensions.
2. Locate asymmetric printed page fiducials and determine orientation.
3. Fit the scanner-to-print affine mapping from printed features only.
4. Decode and verify the physical sheet's run/purpose token before accepting
   any measurements.
5. Predict each target region from the run manifest.
6. Sample the actual backing color from a control area.
7. Segment each open aperture by local color distance rather than a fixed
   white threshold.
8. Extract the aperture boundary and robustly fit a circle or ellipse while
   excluding known bridge sectors, tears, and the cut seam.
9. Compare the fitted aperture center with the local printed ring center.
10. Return per-target confidence and covariance; do not silently use failed
   detections.

The initial capture instructions require the printed face against clean glass,
a fresh opaque matte-white sheet directly behind the calibration sheet, light
pressure from the lid, 600 DPI color, lossless PNG, and scanner cleanup,
auto-crop, auto-deskew, and sharpening disabled.

Minimum automatic acceptance must require both enough targets and spatial
coverage. The provisional rule is at least eight accepted targets, at least
one in every sheet quadrant, finite fit covariance, and no unresolved sheet
orientation. Low contrast, retained liner, missing slugs, shadows, poor
rectification, and torn arcs must produce reviewable failures rather than bad
measurements.

## Manual target: East Bay seven-target method

Recreate the functional geometry with original Sapodilla rendering:

- Canvas: 1200x2100 pixels at 300 DPI, or 101.6x177.8 mm.
- Seven 14x14 mm kiss-cut rectangles:

| Target | Top-left mm | Center mm |
| --- | ---: | ---: |
| C1 | 8, 14 | 15, 21 |
| C2 | 78, 14 | 85, 21 |
| C3 | 8, 78 | 15, 85 |
| C4 | 43, 78 | 50, 85 |
| C5 | 78, 78 | 85, 85 |
| C6 | 8, 146 | 15, 153 |
| C7 | 78, 146 | 85, 153 |

- A 5 mm grid with 10 mm major lines.
- Printability rectangles at 0, 1, 2, 3, 5, and 10 mm from the sheet edge.
- An H80 print scale bar from `(10,62)` to `(90,62)` mm.
- A V150 print scale bar from `(62,10)` to `(62,160)` mm.
- An original printed intended-cut box, 1 mm inset box, center cross, target ID,
  and dimensions at each station.

Use normal production kiss-cut pressure and the same path conventions as
ordinary sticker jobs. The configured material speed is not currently sent to
the printer and must not be described as matched. The upstream local scale
corrections and safe margins are observations from one printer and must not be
defaults. The upstream fitted PLT transform may be retained only as a
conformance comparison, not as a universal calibration.

C2, C5, and C7 end at 92 mm, while the current declared safe width is about
91.95 mm. Preserve the documented layout in the special calibration generator
for initial hardware comparison; do not let the generic off-canvas guard move
it. Move the targets inward only if Sapodilla hardware validation shows that
the 0.05 mm overlap is material.

For measured distances `L`, `R`, `T`, and `B` from the printed center cross to
the physical cut edges:

```text
dx = (R - L) / 2       // positive is right
dy = (B - T) / 2       // positive is down
cut_width = L + R
cut_height = T + B
observed_cut_center = nominal_print_center + [dx, dy]
```

All expected edge distances are 7 mm. The inferred width and height are useful
diagnostics but must not be double-weighted as independent observations.

Record the printability insets and H80/V150 measurements. Report actual print
scale as `measured / expected`. When scale measurements are present, normalize
the physical L/R/T/B displacement into printed logical coordinates with the
inverse measured X/Y print scale before fitting the cut response. If they are
skipped, assume unit print scale and mark the fit as lower-confidence. A future
optional print-dimension compensation would update an existing raster scale
using `expected / measured`, but the first release must not resize normal
artwork automatically: using print scale to interpret calibration measurements
is different from changing the user's printed dimensions.

## Wizard entry and shared shell

Add **Settings -> Calibration...** and a **Calibration profile** action on each
connected printer row. Use a responsive egui modal consistent with the current
Appearance modal:

- Wide layout: step rail plus content.
- Compact layout: `Step n of m` above a single scrollable column.
- Persistent footer: Back, primary action, Save and exit/Cancel.
- Draft autosave after every measurement and completed transition.
- Escape opens Save and exit, Discard, and Keep calibrating choices.
- Existing artwork remains untouched; the wizard submits a generated special
  job.

The method chooser shows the selected printer and media, then two keyboard-
focusable cards:

- **Flatbed scanner - Recommended:** automatically measures removable
  calibration apertures from a 600 DPI scan. Requirements: scanner, clean
  white backing sheet, calibration and validation sticker sheets.
- **Manual:** guided H/V, printability, and seven-target measurements with
  metric calipers or a ruler. The East Bay Makers Club credit and source link
  appear directly on this card.

Never modify the active profile during calibration. Preserve the previous
profile for one-click rollback after activation.

## Flatbed Scanner walkthrough

1. **Prepare**
   - Explain that the job intentionally through-cuts bridged centers.
   - Confirm the selected printer, 4x7 media, current full-through material
     settings, flatbed availability, and clean matte-white backing paper.
2. **Print calibration sheet**
   - Show the target preview and submit a queue job restricted to the selected
     printer.
   - Reuse queue progress and device status; ambiguous failures after printer
     motion offer status checking rather than blindly duplicating the job.
3. **Remove centers**
   - Show a diagram: remove every center including liner, leave surrounding
     printed material in place, and keep the sheet flat.
4. **Scan and import**
   - Instruct printed-side-down placement, backing paper, 600 DPI color PNG,
     and disabled scanner cleanup.
   - Offer `Import scan...`; direct `Scan now...` is not shown until a scanner
     backend exists.
5. **Analyze and review**
   - Display analysis phases and then an overlay plus text table of Accepted,
     Review, and Missing targets.
   - Allow re-import, 90-degree rotation, exclusion of damaged targets, and an
     Advanced manually adjusted center. Manual edits remain visibly marked.
6. **Candidate result**
   - Show measured current metrics, cross-validated estimate, residual vectors,
     excluded points, selected model, and a plain-language diagnosis.
7. **Validate**
   - Print a new corrected sheet with held-out through-cut apertures and several
     representative normal-pressure kiss-cut contours.
   - Re-scan the apertures to obtain actual corrected metrics.
   - Ask the user to confirm that the representative kiss cuts follow their
     printed outlines; through-cut force is not identical to production cutting.
8. **Finish**
   - Activate only after the validation policy passes. Show printer, media,
     method, date, before/after metrics, kiss-cut check, View details, Print a
     test design, and Revert to previous profile.

## Manual walkthrough

1. **Prepare**
   - Confirm selected printer, media, normal production material settings, and
     metric measuring tool. Explain that Sapodilla performs the calculations.
   - Do not present configured cut speed as a controlled calibration variable
     until job serialization actually applies it.
   - Show the East Bay Makers Club methodology credit and source link.
2. **Print measurement sheet**
   - Preview and submit the original Sapodilla rendering of the documented
     seven-target layout, restricted to the selected printer.
3. **Print area**
   - Optionally enter the first fully visible inset at Top, Right, Bottom, and
     Left. Explain that this diagnoses printability, not cut alignment.
4. **Print scale**
   - Enter H80 and V150 endpoint-to-endpoint measurements. Display expected
     values and derived ratios without asking the user to calculate them.
5. **Registration measurements**
   - Present one C1-C7 target at a time with spatially arranged Left, Right,
     Top, and Bottom fields, each expected at 7.00 mm.
   - Show the target location on a mini sheet map and the live derived signed
     displacement. Allow damaged targets to be skipped without discarding data.
6. **Optional cut-size cross-check**
   - Provide collapsed advanced width/height fields without double-weighting
     them in the fit.
7. **Repeat measurement sheet when useful**
   - After the first sheet, offer `Measure another sheet` and recommend it when
     the simpler candidates leave a repeatable position-dependent or skew-like
     residual pattern large enough that an affine candidate could materially
     improve p95 under the model-promotion policy. Do not recommend it merely
     because affine is available.
   - Generate the same documented seven-target geometry with a new run ID. The
     user must unload and independently load fresh media, print/cut it, and
     repeat the print-scale and C1-C7 measurement steps; a duplicate reading of
     the first physical sheet does not qualify.
   - Merge only runs for the same stable printer identity, firmware range,
     media profile, target revision, pressure/path conventions, and measurement
     units. Normalize each sheet with its own H80/V150 measurements, retain each
     accepted observation and uncertainty independently rather than averaging
     matching C IDs, and show both run IDs in the review. Require at least six
     accepted targets from each sheet and twelve total before evaluating the
     affine candidate.
   - `Continue with one sheet` remains available and evaluates only translation
     and independent X/Y scale plus translation. The later validation sheet is
     never merged into training data.
8. **Review and candidate result**
   - Show all measurements, precision, outliers, residual map, competing model
     metrics, source sheet/run for every observation, and links back to any
     target.
9. **Validate and finish**
   - Print a new corrected five-target sheet, repeat the guided L/R/T/B entry,
     compare actual before/after metrics, and activate or keep the current
     profile. Repeat the linked method credit in an `About this method` detail.

## Validation and activation policy

Keep thresholds in a calibration policy type rather than UI literals. Initial
values must be tuned from hardware runs. A candidate is structurally eligible
only when:

- The transform is finite, invertible, and within bounded scale/skew limits.
- The method-specific spatial coverage rule passes.
- Validation uses a newly printed/cut sheet and is not a refit of the training
  observations.
- Validation RMS improves provisionally by at least 25 percent.
- Validation maximum error does not materially worsen.
- The result meets a provisional p95 goal of 0.30-0.50 mm, or is explicitly
  labeled `Improved, outside goal` rather than `Calibrated`.
- The Flatbed workflow's normal kiss-cut inspection passes.

Require repeated sheet loads during hardware characterization. Set the final
goal relative to measured repeatability; a static transform cannot remove
random load/slip error. Invalid or singular fits cannot be activated through an
override. A valid but improved-yet-outside-goal result may have a clearly
separated Advanced override while retaining the previous profile.

## Implementation slices

### 1. Canonical transform and persistence foundation

- Introduce the calibration modules, profile/run schemas, identity lookup, and
  storage sanitization.
- Replace implicit mirror/swap/scale code with a direct canvas-to-plotter
  transform while proving existing PLT output remains byte-for-byte stable.
- Move production PLT calibration to the point where the routed printer is
  known, or retain uncalibrated cut geometry until that point.
- Add profile reset, rollback, stale-firmware behavior, import/export, and a
  diagnostic profile view.

### 2. Deterministic target jobs and shared wizard shell

- Add the target manifest and original raster/vector generator.
- Add printer-restricted calibration/validation jobs and queue recovery.
- Add the responsive, resumable modal, method chooser, target previews, run
  report, and East Bay Makers Club credit surfaces.

### 3. Manual workflow end to end

- Implement the seven-target sheet, H80/V150 and printability diagnostics.
- Add guided measurement entry, sign-safe formulas, uncertainty, robust model
  fitting, result review, second-sheet validation, and activation.
- Hardware-compare Sapodilla's generated target layout with the documented
  method without importing the upstream local correction values.

### 4. Flatbed Scanner workflow end to end

- Implement bridged circular through-cut targets and backing control area.
- Add PNG import, scan normalization, printed fiducial detection, aperture
  segmentation, robust circle/ellipse fitting, confidence, overlays, and
  manual review.
- Add through-cut validation plus the required normal kiss-cut confirmation.

### 5. Hardening and later extensions

- Gather multi-load hardware datasets and finalize thresholds.
- Add residual/feed-axis correction only if repeated evidence supports it.
- Consider TIFF, direct scanner APIs, phone capture, and waste-margin learning.
- Once captured safely, add optional printer-side calibration read/backup,
  write, reboot/readback, reset, and host/device double-correction prevention.

## Verification plan

### Unit and property tests

- Current stock transform corner/axis and byte-for-byte PLT goldens.
- Transform composition, inversion, singular rejection, unit conversion, and
  one-time quantization.
- Synthetic translation, independent scale, rotation/skew, noise, uncertainty,
  outlier, and leave-one-out solver cases.
- Manual sign formulas: `(R-L)/2`, `(B-T)/2`, width, and height.
- Profile/run serialization, migration, bounds, stale firmware, and rollback.
- Target geometry, station coordinates, safe bounds, bridge angles, cut order,
  raster/vector manifest agreement, and deterministic hashes.

### Scanner fixtures

Generate original synthetic fixtures for rotation, scale, scanner skew, color
cast, blur, dirty/discolored backing, translucent liner text, missing slugs,
shadows, torn bridges, incomplete arcs, and outlier targets. Verify orientation,
accepted/rejected IDs, center-localization error, confidence ordering, and
failure messages. Do not import East Bay generated artwork as fixtures.

### UI and queue tests

- `egui_kittest` coverage for both method paths, keyboard focus, compact layout,
  validation errors, save/resume/discard, attribution link, activation, and
  rollback.
- Queue tests proving calibration jobs run only on the selected printer and
  production PLT uses the routed printer's profile.
- Failure tests for disconnect, retry before dispatch, ambiguous post-motion
  failure, wrong scan/run/purpose token, insufficient spatial coverage, worse
  validation, and singular transforms.

### Hardware acceptance harness

Persist one JSON report per run with printer serial/firmware, media/material,
exact payload hashes, observations, exclusions, coefficients, and before/after
metrics. For both workflows:

1. Run the unchanged baseline over at least three independently loaded sheets.
2. Fit on one or more runs and validate on new sheets.
3. Compare current, candidate, and rollback profiles at held-out positions.
4. Repeat Flatbed targets with rotated seam/start angles to expose blade drag.
5. Confirm through-cut calibration with normal production kiss-cut shapes.

Initial engineering gates are at least 25 percent validation RMS improvement,
no material maximum-error regression, and p95 no worse than the larger of the
chosen product target or the measured repeatability floor. Scanner center
localization should contribute materially less error than one 300 DPI print
pixel (0.0847 mm) on clean 600 DPI fixtures.

## Open hardware decisions

- Minimum reliable full-through pressure and bridge size for Liene sticker
  stock without losing slugs inside the printer.
- Whether full-through center bias differs measurably from normal kiss cutting.
- Final scanner patch/backing colors and minimum contrast threshold.
- Minimum manual measurement precision at which affine beats independent-axis
  scale plus translation.
- Whether profiles need ribbon-lot identity in addition to serial, firmware,
  media, material, and mode.
- The measured multi-load repeatability floor and therefore the final p95
  product goal.
