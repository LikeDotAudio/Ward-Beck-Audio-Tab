<!-- Copyright (c) 2026 Ward-Beck Systems (Ward-Beck.ca). All rights reserved. -->
# Deploying the Ward-Beck Audio Tap

The Tap (`ward-beck-audio-tab`, HTTP on `127.0.0.1:8467`) drives the host's audio server, so where
it runs decides what it can do. Start here, then read the document for your platform.

| Platform | Status | How | Document |
|---|---|---|---|
| Linux, Docker | **Ready** | `docker compose up -d` (image `ghcr.io/wardbecksystems/ward-beck-audio-tap`) | [deploy/linux-docker.md](deploy/linux-docker.md) |
| Linux, native | Works today, packaging planned | `install.sh` + systemd user service; `.deb`/`.rpm`/static binaries proposed | [deploy/linux-native.md](deploy/linux-native.md) |
| Windows, WSL2 / Docker | Possible, untested on hardware | Linux build in WSL2 with mirrored networking, audio through WSLg | [deploy/windows-deploy.md](deploy/windows-deploy.md) |
| Windows, native | Not yet ported | WASAPI backend behind a `platform` module; MVP ≈ 2 weeks | [deploy/windows-native.md](deploy/windows-native.md) |

## Linux

- **Docker (ready).** The image builds (130 MB, non-root, amd64 + arm64 via CI) and passed a
  dry-run smoke test. It needs host networking, the user's PipeWire/Pulse sockets, `/dev/shm/mxl`,
  the system D-Bus and `/dev/snd`. `.github/workflows/docker.yml` publishes to GHCR on `main` and
  `v*` tags; make the package public after the first push so `docker compose pull` works.
- **Native.** Ship static musl binaries (x86_64, aarch64) as a tarball, `.deb`, `.rpm` and AUR.
  Flatpak, Snap and AppImage are rejected (sandboxes block module loading, `/dev/shm` and PTP).
  Keep the systemd **user** service, opt-in per user. About two weeks of work.
- **Fix before a wide release** (from the native audit):
  1. Bind the port *before* rewiring audio, so a second instance cannot re-route the first.
  2. Harden HTTP: origin allow-list, JSON content-type and Host checks (any web page can POST today).
  3. Reset All treats children of `systemd --user` as orphans and kills other apps' audio helpers.
  4. The unit file: `BindsTo=pipewire.service` fails on PulseAudio-only hosts; add `--no-browser`
     and an `ExecStopPost` that runs `--abandon` so a crash never leaves the desktop silent.

## Windows

- **Today: WSL2.** Run the Linux build in WSL2 (Windows 11, `networkingMode=mirrored` for multicast)
  or Docker Engine inside the WSL distro. WSLg's PulseAudio (`PULSE_SERVER=unix:/mnt/wslg/PulseServer`)
  reaches the Windows speakers. Windows apps' audio is **not** visible to it on its own: making a
  VB-CABLE the Windows default playback and recording device brings desktop audio back in as the
  WSLg microphone. Docker Desktop's host networking does not pass multicast, so AoIP discovery is lost
  there. Windows containers have no audio and are not an option.
- **Native port.** All dependencies already compile for Windows; the crate has 38 errors in five files
  (`mxl.rs`, `tap.rs`, `aoip/py.rs`, `testpoint.rs`, `aoip.rs`). About two thirds of the code (HTTP API,
  AoIP parsers, MXL rings, meters, generators) is unchanged. A `platform` module gives an
  `AudioBackend` (Linux: today's `pactl`/`parec`; Windows: WASAPI). Virtual buses need a driver
  (VB-CABLE / VAC, or our own signed SYSVAD-based driver); taps and meters do not.
- **Plan:** compile + CI (2–3 d) → MVP meters from WASAPI loopback, tone to the default device
  (7–10 d) → per-app taps, MXL, mDNS, AoIP (2–3 wk) → tray app, signed MSI, winget (1 wk).
  Buses through VB-CABLE add 1–2 weeks; our own driver is 6–12 weeks and needs an EV certificate.
- **Packaging:** a per-user tray app, not a Windows service (session 0 has no user audio). Sign with
  Azure Artifact Signing; EV certificates no longer bypass SmartScreen.

## Browser access (both OSes)

Chrome 142+ and Edge ask the user once before `https://xtm.wbsps.ca` may reach the Tap on the
loopback address. The Tap's CORS headers already allow it. XTM should make its first request from a
button click and use `http://127.0.0.1:8467` (in WSL2 only IPv4 loopback reaches Windows). IT can
pre-approve the site with the `LoopbackNetworkAllowedForUrls` policy.

## Configuration added with this work

| Variable | Default | Purpose |
|---|---|---|
| `WBS_BIND` | `127.0.0.1` | Listen address. `0.0.0.0` only in a bridge-network container published as `127.0.0.1:8467:8467`. |
| `WBS_CLIENT_TIMEOUT` | `12` | Safety stop: the test tone and Bit Spitter stop when no browser has checked in for this many seconds (`0` disables). Scripts keep a tone alive with `POST /api/heartbeat`. |
