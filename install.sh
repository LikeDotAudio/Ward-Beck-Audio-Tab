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

# 1. Install executable
mkdir -p "$BIN_DIR"
cp "$HERE/ward-beck-audio-tab.py" "$BIN_DIR/ward-beck-audio-tab"
chmod +x "$BIN_DIR/ward-beck-audio-tab"
echo "✔ Executable installed to: $BIN_DIR/ward-beck-audio-tab"

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
