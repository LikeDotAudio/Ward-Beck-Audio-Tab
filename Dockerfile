# Copyright (c) 2026 Ward-Beck Systems (Ward-Beck.ca). All rights reserved.
# WARD BECK TAP — virtual patchbay, VU bridge, MXL ring buffers and AoIP listener.
#
# The TAP drives the HOST's audio server, so the container must share it:
# host networking (multicast + :8467), the host's PipeWire/Pulse sockets,
# /dev/shm/mxl, the D-Bus system socket (Avahi) and /dev/snd.
# See docker-compose.yml, or http://127.0.0.1:8467/setup once running.

# ---- build: the Rust daemon ----
FROM rust:1-slim-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock LICENSE ./
COPY src ./src
COPY web ./web
RUN cargo build --release --locked

# ---- runtime ----
FROM debian:bookworm-slim

LABEL org.opencontainers.image.title="WARD BECK TAP" \
      org.opencontainers.image.description="Ward-Beck Systems audio tap: virtual patchbay, VU bridge, EBU DMF / MXL ring buffers, SAP/SIP/PTP AoIP listener" \
      org.opencontainers.image.vendor="Ward-Beck Systems (Ward-Beck.ca)" \
      org.opencontainers.image.licenses="LicenseRef-Ward-Beck-Proprietary" \
      org.opencontainers.image.source="https://github.com/WardBeckSystems/Ward-Beck-Audio-Tab"

# pactl/parec/pacat (pulseaudio-utils), pw-dump/pw-metadata (pipewire-bin), wpctl (wireplumber),
# amixer/aplay (alsa-utils), avahi-browse (avahi-utils), ip (iproute2), curl (healthcheck).
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      pulseaudio-utils pipewire-bin wireplumber alsa-utils avahi-utils iproute2 procps curl ca-certificates \
 && rm -rf /var/lib/apt/lists/*

COPY --from=build /src/target/release/ward-beck-audio-tab /usr/local/bin/ward-beck-audio-tab
COPY LICENSE README.md /opt/wbs-tap/

# Headless: never try to open a browser inside the container.
ENV WBS_NO_BROWSER=1

EXPOSE 8467
HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --retries=3 \
  CMD curl -fsS -o /dev/null --max-time 4 http://127.0.0.1:8467/api/status || exit 1

ENTRYPOINT ["/usr/local/bin/ward-beck-audio-tab", "--no-browser"]
