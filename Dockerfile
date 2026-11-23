# Copyright (c) 2026 Ward-Beck Systems (Ward-Beck.ca). All rights reserved.
# WARD BECK TAP — virtual patchbay, VU bridge, MXL ring buffers and AoIP listener.
#
# The TAP drives the HOST's audio server, so the container must share it:
# host networking (multicast + :8467), the host's PipeWire/Pulse sockets,
# /dev/shm/mxl, the D-Bus system socket (Avahi) and /dev/snd.
# See docker-compose.yml, or http://127.0.0.1:8467/setup once running.

FROM python:3.12-slim-bookworm

LABEL org.opencontainers.image.title="WARD BECK TAP" \
      org.opencontainers.image.description="Ward-Beck Systems audio tap: virtual patchbay, VU bridge, EBU DMF / MXL ring buffers, SAP/SIP/PTP AoIP listener" \
      org.opencontainers.image.vendor="Ward-Beck Systems (Ward-Beck.ca)" \
      org.opencontainers.image.licenses="LicenseRef-Ward-Beck-Proprietary" \
      org.opencontainers.image.source="https://github.com/WardBeckSystems/Ward-Beck-Audio-Tab"

# pactl/parec (pulseaudio-utils), pw-dump/pw-metadata (pipewire-bin), wpctl (wireplumber),
# amixer/aplay (alsa-utils), avahi-browse (avahi-utils), ip (iproute2).
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      pulseaudio-utils pipewire-bin wireplumber alsa-utils avahi-utils iproute2 \
 && rm -rf /var/lib/apt/lists/*

WORKDIR /opt/wbs-tap
COPY *.py LICENSE README.md ./

# Headless: never try to open a browser inside the container.
ENV WBS_NO_BROWSER=1 \
    PYTHONUNBUFFERED=1

EXPOSE 8467
HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --retries=3 \
  CMD python3 -c "import urllib.request,sys; sys.exit(0 if urllib.request.urlopen('http://127.0.0.1:8467/api/status', timeout=4).status == 200 else 1)"

ENTRYPOINT ["python3", "/opt/wbs-tap/ward-beck-audio-tab.py", "--no-browser"]
