# Ward-Beck Audio Tap

[![License](https://img.shields.io/badge/License-Ward--Beck%20Systems-1a3b5c.svg)](LICENSE)
[![Platform](https://img.shields.io/badge/Platform-Linux%20(PipeWire%20%2F%20PulseAudio)-38bdf8.svg)](#requirements)
[![Rust](https://img.shields.io/badge/Rust-1.80+-f97316.svg)](#requirements)

**Ward-Beck Audio Tap** is a low-latency virtual audio patchbay, test point inserter, and live VU meter bridge daemon engineered for the **Ward-Beck Systems XTM-4 & POD-22 Broadcast Metering Ecosystem**.

It allows Linux users to route desktop audio, YouTube, Spotify, DAWs, and media players directly into browser-based broadcast visualizers (such as [xtm.wbsps.ca](https://xtm.wbsps.ca)) with **zero microphone feedback risk**, **zero browser screen-sharing banners**, and sub-millisecond meter synchronization.

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

### Option A: Direct Run
One native binary, written in Rust. `run.sh` builds it on first use (needs [cargo](https://rustup.rs)):
```bash
./run.sh                 # = cargo build --release && target/release/ward-beck-audio-tab
./run.sh --no-browser    # headless; also --check (self-test) and --abandon (teardown)
```
Then open your web browser to **https://ward-beck.ca** (or [xtm.wbsps.ca](https://xtm.wbsps.ca)) to view the live broadcast visualizer, or **http://127.0.0.1:8467** to inspect the local telemetry dashboard.

In **XTM-4** ([xtm.wbsps.ca](https://xtm.wbsps.ca)), open the Settings Drawer (top-left gear) and select:
> `🎛️ [WBS Router Active] Monitor of WBS Meter Tap`

### Option B: 1-Step Installation (systemd User Service)
```bash
./install.sh
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

| Module | Role |
|---|---|
| `src/tap.rs` | Virtual buses and routes, source list and per-source meters, test tone, Bit Buddy, Reset All |
| `src/http.rs` | HTTP API and UI on `:8467` (SSE meter stream, binary `/pcm` flow audio) |
| `src/mxl.rs` | EBU DMF / MXL shared-memory domains and flow rings in `/dev/shm/mxl`, Ward-Beck Test Signals domain |
| `src/testpoint.rs` | One test point at a time (ALSA / PipeWire / MXL / AES67 RTP) with its own ring and meters |
| `src/aoip.rs` | SAP/SDP, PTP, SIP multicast listeners and passive PTP over MQTT |
| `src/pipeline.rs` | ALSA → PipeWire → MXL → AoIP layer report |
| `src/listen.rs` | "What should the meters hear?" picker |
| `src/cbc.rs`, `src/nmos.rs` | CBC / Radio-Canada MXL test-set proxy, NMOS IS-04 / AoIP discovery |
| `web/` | The dashboard (`/`) and setup guide (`/setup`) pages, compiled into the binary |

`cargo test --lib` runs the unit tests. For side-by-side testing next to a running daemon:
`WBS_PORT=18467 WBS_SHM_BASE=/tmp/mxl-test WBS_DRY_RUN=1 target/release/ward-beck-audio-tab --no-browser`
(dry run skips every change to the live audio graph).

---

## 📄 License

Copyright (c) 2026 Ward-Beck Systems (Ward-Beck.ca). All rights reserved.
