# Sapodilla

> [!WARNING]
> This project is a work in progress. Features may not work as expected and
> could potentially harm your device. Use with caution.

A local-first sticker studio and alternative interface for the PixCut S1.
Artwork, layouts, cutlines, and studio documents stay on the machine running
the app.

## Usage

Sapodilla runs as a native desktop application or in Chrome via WebAssembly.
The hosted browser build is available at [gh.knnygrdn.com/sapodilla](https://gh.knnygrdn.com/sapodilla/).

### Features

Feature controls are contextual: select artwork to reveal image tools, choose
**Print & Cut** mode to reveal cutline tools, and use the **Library** and
**Inspector** toolbar buttons to show their panels. See the
[feature reachability audit](docs/feature-reachability.md) for exact UI paths and
platform availability. The tested roles, actions, and minimum workspace bounds
are recorded in the [UI accessibility claims](docs/ui-accessibility-claims.md).

- [x] Connect to device
    - [x] Native vendor USB discovery and bulk transfer
    - [x] Native PixCut Bluetooth LE GATT discovery and transfer
    - [x] Native USB-serial and paired Bluetooth serial discovery
    - [x] Browser Web Serial
- [x] Get status updates from device
- [x] Canvas Editor
    - [x] Image upload
    - [x] Drag/drop placement, scaling, rotation, locking, and visibility
    - [x] DPI-aware rulers, configurable grid, and direct resize/rotation handles
    - [x] Reorderable layers, multi-selection, sheet alignment, and smart snapping
    - [x] Shared canvas and layer context actions for duplicate, arrange, transform, state, and removal
    - [x] Brightness, contrast, saturation, and hue controls
    - [x] One-click in-process edge-background removal (no upload)
    - [x] Native-only on-device neural background removal with first-use model download and caching
    - [x] MaxRects auto-pack with gaps and optional rotation
    - [x] Cutline preview
    - [x] Cutline generation
    - [x] Editable cutline nodes and multiple paths
    - [x] Per-path kiss cut, perforation, or disabled operations
    - [x] Path smoothing and boolean union
    - [x] SVG cutline import for paths, basic shapes, transforms, units, and viewBox
    - [x] 18-shape procedural designer
    - [x] Contour perforation, peel-tab paths, overcut ramps, and material profiles
- [x] Local Library
    - [x] Native desktop persistent recursive folder libraries with startup/rescan refresh
    - [x] Add, fill, and shuffled/cycled fill workflows
- [x] Share & Save
    - [x] Versioned `.sapodilla` project documents for stickers, sheets, and templates
    - [x] Template slots with fit modes, stable artwork identity, and locked cutlines
    - [x] PNG, artwork PDF, cut SVG, PLT, and debug toolpath SVG export
- [x] Production Queue
    - [x] Simultaneous native USB/BLE/serial connections with automatic routing
    - [x] In-session oldest-routable jobs with concurrent dispatch as printers become available
    - [x] Progress, queued-job cancellation, failure, retry, and offline transitions
- [x] Photo Printing
    - [x] Single print job
    - [x] Set number of copies
- [x] Sticker Cutting and Printing
    - [x] Print and cut job

The browser build uses Web Serial. Native builds provide both the PixCut vendor
bulk-USB, PixCut BLE GATT, and operating-system serial connections, including
paired Bluetooth SPP/RFCOMM ports. Multiple native printers can stay connected
at once, and a device is always selected explicitly before connecting.

## Protocol

Protocol documentation can be found [here](protocol.md).
