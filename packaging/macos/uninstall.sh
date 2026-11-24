#!/usr/bin/env bash
# Copyright (c) 2026 Ward-Beck Systems (Ward-Beck.ca). All rights reserved.
# Removes the Ward-Beck Audio Tap for this user (logs are kept).
LABEL="ca.ward-beck.audio-tap"
launchctl bootout "gui/$(id -u)/$LABEL" 2>/dev/null || true
rm -f "$HOME/Library/LaunchAgents/$LABEL.plist"
rm -rf "$HOME/Library/Application Support/Ward-Beck/Audio Tap"
echo "Ward-Beck Audio Tap removed. Logs kept in ~/Library/Logs/Ward-Beck Audio Tap"
