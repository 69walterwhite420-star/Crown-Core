#!/usr/bin/env bash
# C compiler shim for wasm32-unknown-unknown: zstd-sys inside the SOL RPC
# client dependency tree compiles C. Uses system clang when it has a wasm
# backend, otherwise zig cc (override the binary with ZIG=...).
set -euo pipefail

if command -v clang >/dev/null 2>&1 && clang --print-targets 2>/dev/null | grep -q wasm; then
    exec clang "$@"
fi

ZIG="${ZIG:-$HOME/.cache/zig/zig-x86_64-linux-0.16.0/zig}"
args=()
for a in "$@"; do
    case "$a" in
        # zig spells this target differently
        --target=wasm32-unknown-unknown) ;;
        *) args+=("$a") ;;
    esac
done
exec "$ZIG" cc -target wasm32-freestanding "${args[@]}"
