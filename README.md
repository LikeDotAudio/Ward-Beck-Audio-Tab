# Ward-Beck Audio Tab

[![License](https://img.shields.io/badge/License-Ward--Beck%20Systems-1a3b5c.svg)](LICENSE)
[![Platform](https://img.shields.io/badge/Platform-Linux%20(PipeWire%20%2F%20PulseAudio)-38bdf8.svg)](#requirements)
[![Python](https://img.shields.io/badge/Python-3.8+-22c55e.svg)](#requirements)

**Ward-Beck Audio Tab** is a low-latency virtual audio patchbay, test point inserter, and live VU meter bridge daemon engineered for the **Ward-Beck Systems XTM-4 & POD-22 Broadcast Metering Ecosystem**.

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
No installation or pip dependencies required (uses standard Python 3 library):
```bash
python3 ward-beck-audio-tab.py
```
Then open your web browser to **http://127.0.0.1:8467** to access the hardware-styled rack router.

In **XTM-4** ([xtm.wbsps.ca](https://xtm.wbsps.ca)), open the Settings Drawer (top-left gear) and select:
> `🎛️ [WBS Router Active] Monitor of WBS Meter Tap`

### Option B: 1-Step Installation (systemd User Service)
```bash
./install.sh
```
This installs `ward-beck-audio-tab` to `~/.local/bin/` and registers a systemd user service that automatically starts with your desktop session.

---

## 🛡️ Built-in Acoustic Feedback Protection

Bare loopback commands can accidentally link live physical microphones directly into physical speakers, causing immediate acoustic feedback howling. 

**Ward-Beck Audio Tab** includes kernel-level safety logic:
- Physical microphone sources are visually flagged with `🎤 [Mic/Input]`.
- If a microphone source is selected, it is metered on the VU taps but **automatically blocked from directly routing to physical room speakers**, preventing loud feedback while keeping metering intact.

---

## 🌐 HTTP REST API & Telemetry

The daemon provides a CORS-enabled HTTP server on port `8467`:

| Endpoint | Method | Description |
|---|---|---|
| `/` | `GET` | Web patchbay rack interface with animated VU meters |
| `/api/status` | `GET` | JSON status, telemetry, meter dBFS/VU levels, and routes |
| `/api/set_source` | `POST` | Switch active patch input source (`{"source": "name"}`) |
| `/api/toggle` | `POST` | Toggle route legs (`{"route": "meter_tap \| insert_stage \| speaker_split \| test_point_split"}`) |
| `/api/restart` | `POST` | Re-initializes virtual buses from clean state |
| `/api/abandon` | `POST` | Emergency teardown: unloads all virtual modules and restores hardware default sink |

---

## 📦 Requirements

- **Linux**: Ubuntu, Debian, Fedora, Arch Linux, openSUSE, or any distribution running PipeWire or PulseAudio.
- **Python**: 3.8+ (zero pip packages needed).
- **Audio utilities**: `pulseaudio-utils` (provides `pactl` and `parec`).

---

## 📄 License

Copyright (c) 2026 Ward-Beck Systems (Ward-Beck.ca). All rights reserved.
