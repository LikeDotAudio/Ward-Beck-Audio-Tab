#!/bin/bash
# ==============================================================================
# Ward-Beck Systems - CBC/Radio-Canada MXL Hands-on Demo Launcher
# ==============================================================================
set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DOMAIN_DIR="/dev/shm/mxl/domain_1"
CLIPS_DIR="${SCRIPT_DIR}/mxl-hands-on/docker/exercise-4/data/Clips"

echo "=== Initializing MXL Shared-Memory Domain ==="
mkdir -p "${DOMAIN_DIR}"
cp "${SCRIPT_DIR}/mxl-hands-on/docker/exercise-4/domain_def.json" "${DOMAIN_DIR}/"

mkdir -p "${CLIPS_DIR}"
if [ ! -f "${CLIPS_DIR}/sizzle.ts" ] && [ -f "${SCRIPT_DIR}/mxl-hands-on/build-images/sizzle.ts" ]; then
    echo "Copying demo sizzle clip..."
    cp "${SCRIPT_DIR}/mxl-hands-on/build-images/sizzle.ts" "${CLIPS_DIR}/sizzle.ts"
fi

echo "=== Starting MXL Demo Containers ==="
cd "${SCRIPT_DIR}"
docker compose -f docker-compose.mxl-demo.yml up -d

echo ""
echo "=================================================================="
echo "  MXL Ecosystem Demo is Running!"
echo "=================================================================="
echo "  - Tone & Video Generator:      http://localhost:9600"
echo "  - WebRTC Player (Receiver Demo): http://localhost:9601"
echo "  - File / Clip Player:          http://localhost:9602"
echo "  - MXL Domain Info GUI:         http://localhost:9699"
echo "  - Ward-Beck Audio Tab:         http://localhost:8467"
echo "=================================================================="
echo "Tip: Open http://localhost:9600 to start the tone generator or"
echo "     http://localhost:9602 to start playing sizzle.ts."
echo "     Then open http://localhost:9601 to view and hear the receiver!"
echo "=================================================================="
