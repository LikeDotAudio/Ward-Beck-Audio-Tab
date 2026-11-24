#!/usr/bin/env bash
# Copyright (c) 2026 Ward-Beck Systems (Ward-Beck.ca). All rights reserved.
# Quick launcher for Ward-Beck Audio Tab: builds the release binary if needed, then runs it.
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"   # the repo root (the Rust workspace)
BIN="$HERE/target/release/ward-beck-audio-tab"
if [ ! -x "$BIN" ] || [ -n "$(find "$HERE/host" "$HERE/plugin" "$HERE/apps" "$HERE/utils" "$HERE/Cargo.toml" -newer "$BIN" -print -quit)" ]; then
    [ -f "$HERE/Generators/rust/Cargo.toml" ] && [ -f "$HERE/PTP/SRC/Cargo.toml" ] || git -C "$HERE" submodule update --init Generators PTP
    cargo build --release --manifest-path "$HERE/Cargo.toml"
fi
exec "$BIN" "$@"
