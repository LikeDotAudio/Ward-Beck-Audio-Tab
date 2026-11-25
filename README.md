# Ward-Beck Audio Tap

[![License](https://img.shields.io/badge/License-Ward--Beck%20Systems-1a3b5c.svg)](LICENSE)
[![Platform](https://img.shields.io/badge/Platform-Linux%20%7C%20Windows%20%7C%20macOS-38bdf8.svg)](#-downloads)
[![Release](https://github.com/WardBeckSystems/Ward-Beck-Audio-Tap/actions/workflows/release.yml/badge.svg)](https://github.com/WardBeckSystems/Ward-Beck-Audio-Tap/actions/workflows/release.yml)
[![Rust](https://img.shields.io/badge/Rust-1.80+-f97316.svg)](#requirements)

**Ward-Beck Audio Tap** is a low-latency virtual audio patchbay, test point inserter, and live VU meter bridge daemon engineered for the **Ward-Beck Systems XTM-4 & POD-22 Broadcast Metering Ecosystem**.

It allows Linux users to route desktop audio, YouTube, Spotify, DAWs, and media players directly into browser-based broadcast visualizers (such as [xtm.wbsps.ca](https://xtm.wbsps.ca)) with **zero microphone feedback risk**, **zero browser screen-sharing banners**, and sub-millisecond meter synchronization.

---

## ⬇️ Downloads

| Build | Download | Newest build | How to install |
|---|---|---|---|
| **Linux** (x86_64 + ARM64) | [ward-beck-audio-tap-linux.tar.gz](https://github.com/WardBeckSystems/Ward-Beck-Audio-Tap/releases/latest/download/ward-beck-audio-tap-linux.tar.gz) | [nightly](https://github.com/WardBeckSystems/Ward-Beck-Audio-Tap/releases/download/nightly/ward-beck-audio-tap-linux.tar.gz) | [linux/README.md](linux/README.md) |
| **Windows** 10/11 x64 (preview) | [ward-beck-audio-tap-windows.zip](https://github.com/WardBeckSystems/Ward-Beck-Audio-Tap/releases/latest/download/ward-beck-audio-tap-windows.zip) | [nightly](https://github.com/WardBeckSystems/Ward-Beck-Audio-Tap/releases/download/nightly/ward-beck-audio-tap-windows.zip) | [windows/README.md](windows/README.md) |
| **macOS** 14.2+ universal (preview) | [ward-beck-audio-tap-macos.zip](https://github.com/WardBeckSystems/Ward-Beck-Audio-Tap/releases/latest/download/ward-beck-audio-tap-macos.zip) | [nightly](https://github.com/WardBeckSystems/Ward-Beck-Audio-Tap/releases/download/nightly/ward-beck-audio-tap-macos.zip) | [macos/README.md](macos/README.md) |

All releases: <https://github.com/WardBeckSystems/Ward-Beck-Audio-Tap/releases>. Every push to `main` refreshes *nightly*; every `v*` tag makes a release
([`.github/workflows/release.yml`](.github/workflows/release.yml)).

> **Windows and macOS are previews.** They run the core (web app API, MXL rings, generators, AES67,
> SAP/NMOS/PTP/NTP, logging); system-audio capture (WASAPI loopback / Core Audio process tap) is
> still being written.

**Setup manual** (step-by-step pictures and what to expect on each system): [docs/manual](docs/manual/index.html), also in every download as `manual/index.html`.

**Something wrong?** Send the file from <http://127.0.0.1:8467/api/diagnostics/bundle>.

## 🗂️ Layout: a host, plug-in applications, utilities, three builds

| Folder | What |
|---|---|
| [`host/`](host) | The binary (`ward-beck-audio-tab`): HTTP server on `:8467`, plugs every application in |
| [`plugin/`](plugin) | The contract every application plugs into: routes, start-up, and the services they offer each other |
| [`apps/`](apps) | One crate per application (table below) — no application uses another directly |
| [`utils/`](utils) | One crate per utility: `log`, `sys` (Linux / Windows / macOS), `common`, `pyshim`, `net` |
| [`linux/`](linux) | PipeWire / PulseAudio build: `install.sh`, systemd user service, `run.sh`, Docker image |
| [`windows/`](windows) | Windows build: `install.ps1` (per user, starts at sign-in) |
| [`macos/`](macos) | macOS build: `install.sh` (LaunchAgent) |
| `Generators/`, `PTP/` | submodules the workspace builds in (Ward-Beck Generators, the APK PTP sniffer) |

Build from source anywhere: `git submodule update --init Generators PTP && cargo build --release`
(the binary lands in `target/release/`).

---

## 🎛️ Architecture & Virtual Routing Topology

```
   [ Desktop Audio / YouTube / DAW ]
                   │
                   ▼
       wbs_input_bus (Null Sink)
                   │
         ┌─────────┴─────────┐
         ▼                   ▼
  wbs_meter_tap        wbs_insert_bus
 (Direct to XTM-4)     (Optional Processing / Bypass)
         │                   │
         │         ┌─────────┴─────────┐
         │         ▼                   ▼
         │   Physical Speakers   wbs_test_point
         │   (Feedback Guarded)  (Audio Analysis)
         ▼
  [ Ward-Beck XTM-4 Web Visualizer ]
```

1. **`wbs_input_bus`**: Isolated virtual null-sink capturing system playback.
2. **`wbs_meter_tap`**: Parallel mult tapped directly by XTM-4 and internal VU meters.
3. **`wbs_insert_bus`**: Inserter stage allowing real-time DSP bypass or external processing.
4. **`wbs_test_point`**: Secondary tap for analysis tools, spectrum analyzers, or recording.
5. **Physical Speaker Split**: Mirror leg feeding room speakers or headphones, equipped with an **acoustic feedback guard** that automatically blocks direct microphone loops.

---

## 🚀 Quick Start

> Docker, native Linux packaging and Windows: see **[docs/DEPLOYMENT.md](docs/DEPLOYMENT.md)**.

### Option A: Direct Run
One native binary, written in Rust. `linux/run.sh` builds it on first use (needs [cargo](https://rustup.rs)). The generators come from the [Ward-Beck Generators](https://github.com/WardBeckSystems/Ward-Beck-Generators) submodule, so clone with `--recursive` (linux/run.sh fetches it if you forgot):
```bash
git clone --recursive https://github.com/WardBeckSystems/Ward-Beck-Audio-Tap.git && cd Ward-Beck-Audio-Tap
linux/run.sh             # = cargo build --release && target/release/ward-beck-audio-tab
linux/run.sh --no-browser # headless; also --check (self-test) and --abandon (teardown)
```
Then open your web browser to **https://ward-beck.ca** (or [xtm.wbsps.ca](https://xtm.wbsps.ca)) to view the live broadcast visualizer, or **http://127.0.0.1:8467** to inspect the local telemetry dashboard.

In **XTM-4** ([xtm.wbsps.ca](https://xtm.wbsps.ca)), open the Settings Drawer (top-left gear) and select:
> `🎛️ [WBS Router Active] Monitor of WBS Meter Tap`

### Option B: 1-Step Installation (systemd User Service)
```bash
linux/install.sh
```
This installs `ward-beck-audio-tab` to `~/.local/bin/` and registers a systemd user service that automatically starts with your desktop session.

### Option C: Docker (deployable image)
```bash
mkdir -p /dev/shm/mxl
export UID GID=$(id -g)
docker compose up -d --build      # builds ward-beck-audio-tab:latest and starts it
```
The container shares the host network, your PipeWire/Pulse sockets, `/dev/shm/mxl`, the D-Bus
system socket (Avahi) and `/dev/snd`, and runs as your user. PTP is read passively from the APK
MQTT broker. Step-by-step instructions for the TAP, the CBC / Radio-Canada MXL test set
(`./start-mxl-demo.sh`), PTP, SAP/SIP, Avahi discovery and XTM, with live service checks, are
served by the TAP at **http://127.0.0.1:8467/setup**.

---

## 🛡️ Built-in Acoustic Feedback Protection

Bare loopback commands can accidentally link live physical microphones directly into physical speakers, causing immediate acoustic feedback howling. 

**Ward-Beck Audio Tap** includes kernel-level safety logic:
- Physical microphone sources are visually flagged with `🎤 [Mic/Input]`.
- If a microphone source is selected, it is metered on the VU taps but **automatically blocked from directly routing to physical room speakers**, preventing loud feedback while keeping metering intact.

---

## 🌐 HTTP REST API & Telemetry

The daemon provides a CORS-enabled HTTP server on port `8467`:

| Endpoint | Method | Description |
|---|---|---|
| `/` | `GET` | Local telemetry dashboard & gateway to live visualizer (`ward-beck.ca`) |
| `/api/status` | `GET` | JSON status, telemetry, meter dBFS/VU levels, and routes |
| `/api/set_source` | `POST` | Switch active patch input source (`{"source": "name"}`) |
| `/api/toggle` | `POST` | Toggle route legs (`{"route": "meter_tap \| insert_stage \| speaker_split \| test_point_split"}`) |
| `/api/restart` | `POST` | Re-initializes virtual buses from clean state |
| `/api/abandon` | `POST` | Emergency teardown: unloads all virtual modules and restores hardware default sink |

---

## 📦 Requirements

- **Linux**: Ubuntu, Debian, Fedora, Arch Linux, openSUSE, or any distribution running PipeWire or PulseAudio.
- **Rust**: 1.80+ to build (`cargo build --release`); the result is a single binary with the UI pages built in.
- **Audio utilities**: `pulseaudio-utils` (`pactl`, `parec`, `pacat`); optional `pipewire-bin`, `wireplumber`, `alsa-utils`, `avahi-utils` for the pipeline and AoIP layers.

---

## 🦀 Source Layout

Every application is its own crate under `apps/` and depends only on `plugin/` and `utils/`. It
plugs into the host with `plug()`: its HTTP routes, its start-up work, and the service it offers
the others (`wbs_plugin::services::mxl()`, `aoip()`, `patchbay()`, ...). A service nobody provides
answers with an inert default, so any application can be left out of a build.

| Application | Routes | Offers |
|---|---|---|
| `apps/patchbay` | `/api/status`, sources, routes, test tone, Bit Buddy, Reset All, restart | the TAP buses, sources and meters |
| `apps/mxl` | `/api/mxl*` (rings, waveform, binary `/pcm`, levels, slice) | the MXL / EBU DMF shared-memory data plane |
| `apps/receivers` | `/api/testpoint`, `/api/receiver` | the test point and the network / MoIP receivers |
| `apps/aoip` | `/api/aoip*` | SAP / SDP, SIP, the PTP sniffer and the PTP leader |
| `apps/aes67` | `/api/aes67` | AES67 transmitters (RTP + SAP per flow) |
| `apps/hardware` | `/api/hw` | HW 1 / HW 2 and hardware inputs to the sound cards |
| `apps/ntp` | `/api/ntp` | NTP status, configuration and the log of answers |
| `apps/nmos` | `/api/nmos` | NMOS IS-04 / AoIP device discovery |
| `apps/pipeline` | `/api/pipeline*` | the ALSA → PipeWire → MXL → AoIP report |
| `apps/listen` | `/api/listen` | "What should the meters hear?" picker |
| `apps/cbc` | `/api/cbc/*` | CBC / Radio-Canada MXL test-set proxy |
| `apps/diagnostics` | `/api/diagnostics*`, `/api/log` | the report and bundle a tester sends |
| `apps/setup` | `/setup` | the deployment guide page |

The host keeps only the transport: the dashboard (`/`, `host/web/`), the SSE meter stream and shutdown.

`cargo test` runs every crate's unit tests. For side-by-side testing next to a running daemon:
`WBS_PORT=18467 WBS_SHM_BASE=/tmp/mxl-test WBS_DRY_RUN=1 target/release/ward-beck-audio-tab --no-browser`
(dry run skips every change to the live audio graph).

---

## 📄 License

Copyright (c) 2026 Ward-Beck Systems (Ward-Beck.com - Ward-Beck.ca). All rights reserved.
