#!/usr/bin/env bash
# Copyright (c) 2026 Ward-Beck Systems (Ward-Beck.ca). All rights reserved.
# Uninstaller for Ward-Beck Audio Tab.

set -euo pipefail

BIN_DIR="$HOME/.local/bin"
SERVICE_DIR="$HOME/.config/systemd/user"

echo "===================================================================="
echo " Uninstalling Ward-Beck Audio Tab"
echo "===================================================================="

# 1. Stop service and abandon virtual buses
if systemctl --user is-active --quiet ward-beck-audio-tab.service 2>/dev/null; then
    echo "Stopping systemd service..."
    systemctl --user stop ward-beck-audio-tab.service
fi
systemctl --user disable ward-beck-audio-tab.service 2>/dev/null || true

# 2. Run teardown to restore hardware sink
if [ -x "$BIN_DIR/ward-beck-audio-tab" ]; then
    "$BIN_DIR/ward-beck-audio-tab" --abandon 2>/dev/null || true
fi

# 3. Remove files
rm -f "$BIN_DIR/ward-beck-audio-tab"
rm -f "$SERVICE_DIR/ward-beck-audio-tab.service"
systemctl --user daemon-reload 2>/dev/null || true

echo "✔ Ward-Beck Audio Tab uninstalled and hardware audio restored."
echo "===================================================================="
