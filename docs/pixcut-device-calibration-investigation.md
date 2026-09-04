# PixCut device-side calibration investigation

## Status

Device-side calibration storage is **not yet identified**. Sapodilla currently
uses host-side coordinate conversion and must not guess at undocumented
calibration properties. `PrinterSubState::Calibrating` confirms that firmware
has a calibration operation, but it does not establish a readable or writable
property name, payload schema, unit, lifetime, or reset behavior.

This document is the gate for investigating device storage without exposing an
unsafe write path in the normal application.

## Public evidence checked

Liene's published workflow confirms that its app prints and cuts a calibration
sample, asks the operator to enter decimal coordinates for four points (A-D),
and then exposes a separate **Start Calibration** action:

- [How to Calibrate Cutting with PixCut S1?](https://www.liene-life.com/blogs/how-tos/how-to-calibrate-cutting-with-pixcut-s1)

That is useful evidence that a post-measurement operation exists, but the page
does not identify whether the result is stored on the printer or in the app. It
also provides no command name, payload schema, units, sign convention,
readback, reset value, or power-cycle behavior. The production client therefore
continues to use a fully reversible host-side profile while the capture gates
below remain unmet.

## Questions the capture must answer

1. Which request starts and completes official calibration?
2. Are calibration values sent directly, derived on the device, or embedded in
   a calibration job?
3. Which values can be read before and after calibration?
4. What are their coordinate system, axis order, units, sign, numeric range,
   and quantization?
5. Are values global, keyed by media type, or keyed by print-and-cut mode?
6. Do they survive reconnect, power cycle, and firmware restart?
7. Is there a documented or observed factory reset value?
8. Does firmware apply the stored correction before or after PLT coordinates?
9. Does applying a host profile as well create a double correction?

## Capture procedure

Use a sacrificial test printer and retain its original values. Capture the
official application's request and response frames without modifying them.
Perform one controlled change at a time:

1. Connect, request all properties used by the official calibration screen,
   and save the raw responses.
2. Open calibration but cancel without saving; record any traffic and state
   changes.
3. Run calibration with a neutral result and record the complete traffic from
   entry through return to idle.
4. Repeat with deliberately distinctive X-only and Y-only values whose signs
   and magnitudes cannot be confused, then with a small scale-like change if
   the official UI supports one.
5. Request the same properties after each change, reconnect, and power-cycle.
6. Restore the original values with the official application and prove the
   readback matches the initial capture.

Each capture bundle must record:

- printer model, serial number, hardware revision, and firmware revision;
- transport and framing mode;
- monotonic timestamp and direction for every frame;
- exact raw framed bytes plus decoded JSON where applicable;
- the official UI value entered before each frame sequence;
- printer state and sub-state transitions;
- whether paper was loaded and whether any motion occurred; and
- before, after, reconnect, reboot, and restored readbacks.

Redact account or network identifiers from shared reports, but retain stable
printer identity locally so captures cannot be mixed across units.

## Offline capture fixture format

Store sanitized captures under `tests/fixtures/calibration/device/` only after
their provenance is recorded. A fixture manifest should use this shape:

```json
{
  "schema_version": 1,
  "printer": {
    "model": "DHP700",
    "serial_number": "REDACTED-STABLE-A",
    "firmware_revision": "captured value"
  },
  "scenario": "x-only-positive",
  "entered_values": { "x": 3, "y": 0 },
  "frames": [
    {
      "elapsed_ms": 0,
      "direction": "host-to-printer",
      "raw_hex": "...",
      "decoded_json": {}
    }
  ],
  "readback": {
    "before": {},
    "after": {},
    "after_reconnect": {},
    "after_reboot": {},
    "restored": {}
  }
}
```

Captured fixtures are observations, not permission to synthesize unknown
commands. The decoder may accept unknown fields, while the serializer must
emit only a schema supported by an exact reviewed fixture.

## Implementation gates

### Gate 1: read-only identification

Requirements:

- At least two captures for each controlled change give the same semantic
  result.
- X-only and Y-only changes identify axis order and sign.
- At least three magnitudes identify unit and quantization.
- Readback is decoded with bounds and firmware-version checks.
- A read-only diagnostic can never construct or send a write request.

Passing this gate permits displaying a raw device calibration backup in Debug
Tools. It does not permit editing or activation.

### Gate 2: guarded write and restoration harness

Requirements:

- Exact write schema is supported by captured official traffic.
- The harness always performs `read -> durable backup -> write -> readback`.
- Mismatched readback stops before reboot or test motion.
- Reboot persistence is demonstrated.
- `restore -> readback` returns the exact original semantic values.
- An interrupted-write recovery procedure is demonstrated.
- Tests with a mock transport prove ordering and prove that backup failure,
  identity mismatch, stale firmware, or bounds failure prevents the write.

Passing this gate permits an engineering-only command that requires the user
to select a capture-backed firmware schema and explicitly confirm the printer
serial. It does not permit a normal settings control.

### Gate 3: coordinate interaction

Print and cut the same held-out target under four conditions:

1. device baseline plus stock host mapping;
2. device correction plus stock host mapping;
3. device baseline plus calibrated host mapping; and
4. device correction plus calibrated host mapping.

Use independently loaded sheets and the same validation policy as host-side
calibration. The results must establish whether the two transforms replace or
compose with one another. If both are applied by the firmware path, Sapodilla
must store the device readback hash with the host profile and refuse to apply a
profile whose expected device baseline differs.

Passing this gate permits implementing one of these explicit modes:

- **Host profile:** restore or require the captured device baseline, then apply
  the Sapodilla transform once while encoding PLT.
- **Device profile:** write and verify the device correction, then use the
  corresponding uncorrected host mapping.

There is no automatic hybrid mode.

### Gate 4: normal UI eligibility

Device-side storage can appear outside engineering tools only after:

- backup, bounded write, readback, reboot, restore, and reset all pass on every
  supported firmware family;
- identity and stale-firmware behavior are covered by automated tests;
- a recoverable error path exists for every step;
- host/device double-correction prevention is enforced rather than advisory;
- normal host-side calibration remains available without firmware writes; and
- a hardware report demonstrates no regression against the same held-out
  sheets.

## Required automated tests

- Decode captured property responses with kebab-case and observed aliases.
- Reject missing identity, unknown firmware schema, non-finite values, values
  outside captured bounds, and malformed response shapes.
- Produce byte-exact request JSON and framed bytes for each supported fixture.
- Prove read-only sessions contain no write-capable operation.
- Prove mock ordering for backup, write, readback, reboot check, and restore.
- Prove every failure before verified write leaves the transport without a
  write call.
- Prove a changed device readback invalidates a host profile that expects the
  prior device baseline.
- Prove restored device baseline plus host calibration applies compensation
  exactly once.

## Evidence still required

No property names, write method, payload values, reset command, or numeric
bounds should be added to production code until supported by sanitized capture
fixtures and a hardware report. The next action is therefore capture with the
official application, not speculative firmware commands.
