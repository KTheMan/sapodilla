$ErrorActionPreference = 'Stop'
$env:NO_COLOR = 'true'

$env:RUSTUP_TOOLCHAIN = if ($env:RUST_NIGHTLY_VERSION) {
    $env:RUST_NIGHTLY_VERSION
} else {
    'nightly-2026-03-17'
}
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
