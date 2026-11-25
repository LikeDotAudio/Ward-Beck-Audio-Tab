#!/usr/bin/env bash
# Copyright (c) 2026 Ward-Beck Systems (Ward-Beck.com - Ward-Beck.ca). All rights reserved.
# Ward-Beck Audio Tap for macOS: installs the Tap for this user and starts it at login.
#   bash install.sh
# The first time it captures audio, macOS asks for permission to record system audio:
# allow it (System Settings > Privacy & Security > Screen & System Audio Recording).

set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DEST="$HOME/Library/Application Support/Ward-Beck/Audio Tap"
LABEL="ca.ward-beck.audio-tap"
PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
LOG="$HOME/Library/Logs/Ward-Beck Audio Tap/tap.log"

echo "===================================================================="
echo " Installing Ward-Beck Audio Tap (macOS)"
echo "===================================================================="

launchctl bootout "gui/$(id -u)/$LABEL" 2>/dev/null || true
mkdir -p "$DEST" "$HOME/Library/LaunchAgents" "$(dirname "$LOG")"
install -m 755 "$HERE/ward-beck-audio-tab" "$DEST/ward-beck-audio-tab"
for f in README.md LICENSE uninstall.sh; do [ -f "$HERE/$f" ] && cp "$HERE/$f" "$DEST/"; done
# A download is quarantined by Gatekeeper; an unsigned build will not start until it is cleared.
xattr -dr com.apple.quarantine "$DEST/ward-beck-audio-tab" 2>/dev/null || true
echo "✔ Executable installed to: $DEST/ward-beck-audio-tab"

cat > "$PLIST" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>$LABEL</string>
  <key>ProgramArguments</key>
  <array><string>$DEST/ward-beck-audio-tab</string><string>--no-browser</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardOutPath</key><string>$LOG.stdout</string>
  <key>StandardErrorPath</key><string>$LOG.stderr</string>
</dict>
</plist>
EOF
launchctl bootstrap "gui/$(id -u)" "$PLIST"
echo "✔ Starts at login (LaunchAgent $LABEL) and is running now"

sleep 3
if curl -fsS -m 5 http://127.0.0.1:8467/api/diagnostics >/dev/null; then
  echo "✔ The Tap answers on http://127.0.0.1:8467"
else
  echo "⚠ The Tap did not answer yet. Log: $LOG"
fi
echo ""
echo "Open https://ward-beck.ca (or xtm.wbsps.ca) to meter and route."
echo "Log file:    $LOG"
echo "Diagnostics: http://127.0.0.1:8467/api/diagnostics/bundle  (send this file with any report)"
echo "===================================================================="
