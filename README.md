# Sapodilla

> [!WARNING]
> This project is a work in progress. Features may not work as expected and
> could potentially harm your device. Use with caution.

A local-first sticker studio and alternative interface for the PixCut S1.
Artwork, layouts, cutlines, and studio documents stay on the machine running
the app.

## Usage

Sapodilla runs as a native desktop application or in Chrome via WebAssembly.
The hosted browser build is available [here](https://sapodilla.pages.dev).

### Features

- [x] Connect to device
    - [x] Native vendor USB discovery and bulk transfer
    - [x] Native PixCut Bluetooth LE GATT discovery and transfer
    - [x] Native USB-serial and paired Bluetooth serial discovery
    - [x] Browser Web Serial
- [x] Get status updates from device
- [x] Canvas Editor
    - [x] Image upload
    - [x] Drag/drop placement, scaling, rotation, locking, and visibility
    - [x] Reorderable layers, multi-selection, sheet alignment, and smart snapping
    - [x] Brightness, contrast, saturation, and hue controls
    - [x] One-click local edge-background removal
    - [x] One-click local neural background removal with first-use model caching
    - [x] MaxRects auto-pack with gaps and optional rotation
    - [x] Cut mark preview
    - [x] Cut mark generation
    - [x] Editable cutline nodes and multiple paths
    - [x] Per-path kiss cut, perforation, or disabled operations
    - [x] Path smoothing and boolean union
    - [x] SVG paths, basic shapes, transforms, units, and viewBox import
    - [x] 18-shape procedural designer
    - [x] Contour perforation, peel-tab paths, overcut ramps, and material profiles
- [x] Local Library
    - [x] Persistent recursive folder libraries with startup/rescan refresh
    - [x] Add, fill, and shuffled/cycled fill workflows
- [x] Share & Save
    - [x] Versioned `.stix`, `.stixcut`, and `.stixtpl` Sapodilla documents
    - [x] Template slots with fit modes, stable artwork identity, and locked cutlines
    - [x] PNG, artwork PDF, cut SVG, PLT, and debug toolpath SVG export
- [x] Production Queue
    - [x] Simultaneous USB/BLE/serial connections and capability-aware automatic routing
    - [x] Retained FIFO jobs with concurrent dispatch as printers become available
    - [x] Progress, cancellation, failure, retry, and offline transitions
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
