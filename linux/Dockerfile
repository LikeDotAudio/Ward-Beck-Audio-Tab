# Copyright (c) 2026 Ward-Beck Systems (Ward-Beck.ca). All rights reserved.
# WARD BECK TAP — virtual patchbay, VU bridge, MXL ring buffers and AoIP listener.
#
# The TAP drives the HOST's audio server, so the container must share it:
# host networking (multicast + :8467), the host's PipeWire/Pulse sockets,
# /dev/shm/mxl, the D-Bus system socket (Avahi) and /dev/snd.
# See docker-compose.yml and docs/deploy/linux-docker.md.
#
# Builds natively for whatever platform the builder targets (linux/amd64, linux/arm64);
# CI builds arm64 under QEMU. Needs the repo plus the Generators/ and PTP/ submodules:
#   git submodule update --init Generators PTP && docker build -f linux/Dockerfile -t ward-beck-audio-tap .

# ---- build: the Rust daemon ----
FROM rust:1-slim-bookworm AS build
WORKDIR /src
# The crate depends on Generators/rust (path dependency, crate wbs-generators),
# whose license-file is Generators/LICENSE.
# Build context is the repo root: the workspace, the core crate and its two submodule crates.
COPY Cargo.toml Cargo.lock LICENSE ./
COPY Generators/LICENSE ./Generators/LICENSE
COPY Generators/rust ./Generators/rust
# The APK PTP sniffer (path dependency apkaudio-ptp).
COPY PTP/SRC ./PTP/SRC
COPY core ./core
RUN cargo build --release --locked \
 && strip target/release/ward-beck-audio-tab

# ---- runtime ----
FROM debian:bookworm-slim

LABEL org.opencontainers.image.title="WARD BECK TAP" \
      org.opencontainers.image.description="Ward-Beck Systems audio tap: virtual patchbay, VU bridge, EBU DMF / MXL ring buffers, SAP/SIP/PTP AoIP listener" \
      org.opencontainers.image.vendor="Ward-Beck Systems (Ward-Beck.ca)" \
      org.opencontainers.image.licenses="LicenseRef-Ward-Beck-Proprietary" \
      org.opencontainers.image.source="https://github.com/WardBeckSystems/Ward-Beck-Audio-Tap"

# pactl/parec/pacat (pulseaudio-utils), pw-dump/pw-metadata (pipewire-bin), wpctl (wireplumber),
# amixer/aplay (alsa-utils), avahi-browse (avahi-utils), ip (iproute2), curl (healthcheck).
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      pulseaudio-utils pipewire-bin wireplumber alsa-utils avahi-utils iproute2 procps curl ca-certificates \
 && rm -rf /var/lib/apt/lists/* /usr/share/doc/* /usr/share/man/*

# Non-root default user. UID/GID 1000 matches the first desktop user on most distros;
# compose overrides it with `user: ${UID}:${GID}` so the host audio sockets accept it.
RUN groupadd -g 1000 tap \
 && useradd -u 1000 -g 1000 -G audio -M -d /nonexistent -s /usr/sbin/nologin tap \
 && install -d -o 1000 -g 1000 -m 0700 /run/user/1000

COPY --from=build /src/target/release/ward-beck-audio-tab /usr/local/bin/ward-beck-audio-tab
COPY LICENSE README.md /opt/wbs-tap/

# Headless: never try to open a browser inside the container.
ENV WBS_NO_BROWSER=1 \
    WBS_PORT=8467 \
    XDG_RUNTIME_DIR=/run/user/1000

USER 1000:1000
EXPOSE 8467
# The daemon binds 127.0.0.1 only, so the probe runs inside the container's network namespace.
HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --retries=3 \
  CMD curl -fsS -o /dev/null --max-time 4 "http://127.0.0.1:${WBS_PORT:-8467}/api/status" || exit 1

ENTRYPOINT ["/usr/local/bin/ward-beck-audio-tab", "--no-browser"]
