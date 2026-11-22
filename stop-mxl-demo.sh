#!/bin/bash
# ==============================================================================
# Ward-Beck Systems - Stop CBC/Radio-Canada MXL Demo Containers
# ==============================================================================
set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "${SCRIPT_DIR}"

echo "Stopping MXL demo containers..."
docker compose -f docker-compose.mxl-demo.yml down
echo "MXL demo containers stopped."
