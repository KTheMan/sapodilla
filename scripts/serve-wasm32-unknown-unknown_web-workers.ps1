$ErrorActionPreference = 'Stop'
$env:NO_COLOR = 'true'

$env:RUSTUP_TOOLCHAIN = if ($env:RUST_NIGHTLY_VERSION) {
    $env:RUST_NIGHTLY_VERSION
} else {
    'nightly-2026-03-17'
}

# A global sccache wrapper can fail to launch rustc for the large threaded WASM
# dependency graph on Windows. An executable pass-through avoids that setting;
# unlike a cmd wrapper, it also supports Cargo's very long rustc command lines.
$rustcWrapper = Join-Path ([System.IO.Path]::GetTempPath()) 'sapodilla-rustc-wrapper.exe'
if (-not (Test-Path -LiteralPath $rustcWrapper)) {
    $wrapperSource = Join-Path ([System.IO.Path]::GetTempPath()) 'sapodilla-rustc-wrapper.rs'
    $source = @'
use std::{env, process::{exit, Command}};
fn main() {
    let mut args = env::args_os().skip(1);
    let Some(rustc) = args.next() else { exit(1) };
    let code = Command::new(rustc).args(args).status()
        .ok().and_then(|status| status.code()).unwrap_or(1);
    exit(code);
}
'@
    [System.IO.File]::WriteAllText($wrapperSource, $source, [System.Text.Encoding]::UTF8)
    & rustc $wrapperSource -o $rustcWrapper
    if ($LASTEXITCODE -ne 0) {
        throw "Could not build the Sapodilla rustc pass-through wrapper."
    }
}
$env:RUSTC_WRAPPER = $rustcWrapper
$env:CARGO_BUILD_RUSTC_WRAPPER = $rustcWrapper
$env:RUSTFLAGS = @(
    '--cfg=web_sys_unstable_apis'
    '--cfg=getrandom_backend="wasm_js"'
    '-C target-feature=+atomics,+bulk-memory,+mutable-globals'
    '-C link-arg=--shared-memory'
    '-C link-arg=--max-memory=1073741824'
    '-C link-arg=--import-memory'
    '-C link-arg=--export=__wasm_init_tls'
    '-C link-arg=--export=__tls_size'
    '-C link-arg=--export=__tls_align'
    '-C link-arg=--export=__tls_base'
    '-C link-arg=--export=__heap_base'
) -join ' '

trunk serve --features web-workers --address 127.0.0.1 --port 8910 @args
exit $LASTEXITCODE
