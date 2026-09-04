# Third-party notices

## East Bay Makers Club PixCut calibration methodology

Sapodilla's guided Manual calibration uses the seven-target measurement
method publicly documented by East Bay Makers Club in `pixcut-s1`, inspected at
commit `a73cd65b7374e5b9e1eb7a0c54594f3276c14f8f` (target generator commit
`f71a8f882b2462990e95554a0927a6e1d9860993`). The Rust target generator,
artwork, wizard, formulas, and tests in Sapodilla are an independent clean-room
implementation. No upstream source code or generated artwork is included.

Method credit: East Bay Makers Club · pixcut-s1

Source: https://github.com/eastbaymakersclub/pixcut-s1#calibration-workflow

The inspected upstream repository did not declare a software license. This
notice therefore credits the publicly documented methodology and does not
assert or reproduce an upstream license grant.

## honeymaro/pixcut conformance fixtures

The files under `tests/fixtures/honeymaro-pixcut/` named `square-exact.plt`
and `square-overcut.plt` are copied from the `honeymaro/pixcut` conformance
fixture set at commit `08580a69cc8cd6823ad58297f9594000e6ad06ff`
(repository state inspected 2026-09-02). The
upstream manifest identifies these as output captured from `ht_plt_core.dll`.
They are included only as deterministic test vectors. The symmetric Hausdorff
comparison test is adapted from that project's conformance harness. The
upstream project is licensed under the MIT License; its license text is
reproduced alongside the fixtures.

Source: https://github.com/honeymaro/pixcut

## PixCut native USB interoperability references

The native USB constants and framing in `src/raw_usb.rs` and
`src/transports/native_usb.rs` were independently implemented from publicly
documented interoperability behavior in these MIT-licensed projects:

- `ThroatyMumbo/PixCutS1-Linux`, commit
  `a8734f0a16535126534fbc17411b3f37ba27ef00`
- `popcornhax/PixCut-App`, commit
  `bc243ce63d9f458b818cb9e19b3c93f281b85f7a`

No source files from those projects are included here.

## ahkamboh/offline-bg-removal background-removal pipeline

The preprocessing, BiRefNet inference, and matte postprocessing pipeline in
`src/background_ml.rs` is a Rust port of `ahkamboh/offline-bg-removal` at
commit `fdb9e9096975f4b90f1e7bb23eb3be797e1d048f`.

Source: https://github.com/ahkamboh/offline-bg-removal

MIT License

Copyright (c) 2026 ahkamboh

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
