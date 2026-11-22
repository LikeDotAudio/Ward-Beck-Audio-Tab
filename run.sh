#!/usr/bin/env bash
# Quick launcher for Ward-Beck Audio Tab
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
exec python3 "$HERE/ward-beck-audio-tab.py" "$@"
