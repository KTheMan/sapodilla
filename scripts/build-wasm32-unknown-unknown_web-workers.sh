#!/bin/sh

set -exu

export RUSTFLAGS="\
    --cfg=web_sys_unstable_apis \
    --cfg=getrandom_backend=\"wasm_js\" \
    -C target-feature=+atomics,+bulk-memory,+mutable-globals \
    -C link-arg=--shared-memory \
    -C link-arg=--max-memory=1073741824 \
    -C link-arg=--import-memory \
    -C link-arg=--export=__wasm_init_tls \
    -C link-arg=--export=__tls_size \
    -C link-arg=--export=__tls_align \
    -C link-arg=--export=__tls_base"

trunk build --release --features web-workers
