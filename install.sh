#!/usr/bin/env bash
# Copyright (c) 2026 Ward-Beck Systems (Ward-Beck.ca). All rights reserved.
# Installer for Ward-Beck Audio Tab daemon & systemd user service.

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN_DIR="$HOME/.local/bin"
SERVICE_DIR="$HOME/.config/systemd/user"

echo "===================================================================="
echo " Installing Ward-Beck Audio Tab"
echo "===================================================================="

# 1. Build and install the executable (Rust; needs cargo: https://rustup.rs)
command -v cargo >/dev/null || { echo "✘ cargo not found. Install Rust first: https://rustup.rs"; exit 1; }
[ -f "$HERE/Generators/rust/Cargo.toml" ] || git -C "$HERE" submodule update --init Generators
cargo build --release --manifest-path "$HERE/Cargo.toml"
mkdir -p "$BIN_DIR"
systemctl --user stop ward-beck-audio-tab.service 2>/dev/null || true
install -m 755 "$HERE/target/release/ward-beck-audio-tab" "$BIN_DIR/ward-beck-audio-tab"
echo "✔ Executable installed to: $BIN_DIR/ward-beck-audio-tab"

# PTP sniffer (apk-ptp): UDP 319/320 and gPTP raw Ethernet need two capabilities.
# A new binary loses them, so grant them again on every install. Without a terminal
# to ask for the password, the Timing tab asks instead.
CAPS="cap_net_raw,cap_net_bind_service+ep"
if sudo -n setcap "$CAPS" "$BIN_DIR/ward-beck-audio-tab" 2>/dev/null \
   || { [ -t 0 ] && sudo setcap "$CAPS" "$BIN_DIR/ward-beck-audio-tab"; }; then
  echo "✔ PTP capabilities granted"
else
  echo "• PTP capabilities not granted (no sudo); the Timing tab will show the command"
fi

# 2. Install systemd user service
mkdir -p "$SERVICE_DIR"
cp "$HERE/ward-beck-audio-tab.service" "$SERVICE_DIR/ward-beck-audio-tab.service"
systemctl --user daemon-reload
echo "✔ User service installed: $SERVICE_DIR/ward-beck-audio-tab.service"

# 3. Enable and start service
systemctl --user enable ward-beck-audio-tab.service
systemctl --user restart ward-beck-audio-tab.service
echo "✔ Service enabled and started via systemctl --user"

echo ""
echo "Ward-Beck Audio Tab is now active at: http://127.0.0.1:8467"
echo "To view service logs: journalctl --user -u ward-beck-audio-tab.service -f"
echo "===================================================================="
