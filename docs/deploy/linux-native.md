<!-- Copyright (c) 2026 Ward-Beck Systems (Ward-Beck.ca). All rights reserved. -->
# WARD BECK TAP on Linux, native (no Docker)

This is a research document on shipping the `ward-beck-audio-tab` daemon (the WARD BECK TAP)
as a native Linux program: what the code assumes about the host, which distros it runs on,
how to package it, how to run it under systemd, and how to lock it down. Docker is covered
separately in [linux-docker.md](linux-docker.md).

The code was audited from the working tree. That includes the uncommitted client-watchdog
change in `apps/patchbay/src/lib.rs`, `host/src/http.rs` and `host/src/main.rs`. Line numbers refer to that tree.

**Short answer**

- Ship **prebuilt binaries**. Build a static musl binary for x86_64 and aarch64, put it in a
  tarball with an `install.sh` that needs no Rust, and add a `.deb` (cargo-deb) and an `.rpm`
  (cargo-generate-rpm) that install the same binary and a **systemd user unit**.
- Keep the **user service** model. Fix the unit file: the current one fails on hosts that
  run only PulseAudio, opens a browser on every restart, and does not restore the speakers if
  the daemon is killed.
- Do not use Flatpak, Snap or AppImage. The sandboxed formats block exactly what the TAP
  does: `pactl load-module`, multicast, and `/dev/shm`.
- Before wide release, fix three code issues:
  - Requests from other websites are not checked (any site can POST to the API).
  - A second instance rewires the audio before it finds the port already taken.
  - Reset All can kill other programs' `parec`/`pw-cat` processes when it runs under systemd.

---

## 1. Audit: Linux assumptions and runtime dependencies

### 1.1 External commands

Every external program is started with `std::process::Command`. A shell is never used. The
programs are found on `PATH` (see `which` in `utils/common`). There are three helpers:

- `pactl::run_cmd`: 2.5 s timeout, `utils/common` (`pactl` module).
- `pactl::run_mut`: does nothing under `WBS_DRY_RUN=1`, `utils/common`.
- `run_cmd_full`: the caller's timeout, `utils/common`.

| Command | Where (file:line) | What for | Required? | Debian / Ubuntu / RPi OS | Fedora | Arch |
|---|---|---|---|---|---|---|
| `pactl` | `tap.rs:833, 1040-1102, 1219-1227, 1307, 1363-1372`; `listen.rs:114, 333, 347` | list sources, sinks, sink-inputs and modules; `load-module` / `unload-module` null sinks and loopbacks; `set-default-sink`; `move-sink-input` | **Yes**, the core of the daemon | `pulseaudio-utils` | `pulseaudio-utils` | `libpulse` |
| `parec` | `tap.rs:910` (one per source meter), `tap.rs:1406` (main VU), `mxl.rs:1154` (MXL bridge, 48 kHz s16), `testpoint.rs:433` (test point, `--monitor-stream=<serial>` at `:803`) | capture PCM | **Yes** | `pulseaudio-utils` | `pulseaudio-utils` | `libpulse` |
| `pacat` | `tap.rs:1466` (tone and Bit Buddy, `--device=wbs_input_bus`, `setsid` pre-exec) | play generated signals | **Yes** for tone and Bit Buddy | `pulseaudio-utils` | `pulseaudio-utils` | `libpulse` |
| `pw-dump` | `testpoint.rs:74`, `pipeline.rs:220` | PipeWire graph for test points and the pipeline report | PipeWire only | `pipewire-bin` | `pipewire-utils` | `pipewire` |
| `pw-metadata` | `testpoint.rs:124` | graph quantum (`clock.quantum`, `clock.force-quantum`) | PipeWire only | `pipewire-bin` | `pipewire-utils` | `pipewire` |
| `wpctl` | `pipeline.rs:214` | `wpctl status` tree for the pipeline report | WirePlumber only | `wireplumber` | `wireplumber` | `wireplumber` |
| `aplay`, `amixer` | `pipeline.rs:131, 142, 146` | ALSA card list and mixer controls (report only) | Optional | `alsa-utils` | `alsa-utils` | `alsa-utils` |
| `avahi-browse` | `pipeline.rs:311`, `nmos.rs:121` | mDNS discovery: Dante, RAVENNA, NMOS `_nmos-node._tcp` | Optional; needs `avahi-daemon` and the system D-Bus | `avahi-utils` | `avahi-tools` | `avahi` |
| `ip` | `testpoint.rs:87` (`ip -o route get <group>`) | find the interface for an RTP group | Optional | `iproute2` | `iproute` | `iproute2` |
| `ps` | `tap.rs:1238` (`ps -u <uid> -o pid=,ppid=,args=`) | Reset All: find stray helper processes | Optional | `procps` | `procps-ng` | `procps-ng` |
| `xprop` | `listen.rs:173-177` | match windows to PIDs for the listen picker (X11 only; skipped when `DISPLAY` is unset, `:170`) | Optional | `x11-utils` | `xprop` | `xorg-xprop` |
| `xdg-open` | `main.rs:56` | open the browser at start-up (skipped without `DISPLAY`/`WAYLAND_DISPLAY`, `:53`) | Optional | `xdg-utils` | `xdg-utils` | `xdg-utils` |

Other names appear only as process names to match, never as commands to run: `pw-cat`,
`pw-play`, `pw-record`, `pw-loopback` and `paplay` (`listen.rs:23`, `tap.rs:1135, 1173`).

**Minimum runtime set:** `pulseaudio-utils` (or `libpulse` on Arch) plus a running
PulseAudio-protocol server, which is either `pipewire-pulse` or `pulseaudio`. Everything else
only makes features or reports richer.

### 1.2 Paths and files

| Path | Where | Notes |
|---|---|---|
| `/dev/shm/mxl` (override `WBS_SHM_BASE`) | `util.rs:19`; created at `mxl.rs:650, 1331, 1374, 1397`; `flock` at `mxl.rs:1127, 1417` | MXL / EBU DMF domains and flow rings, memory-mapped (`memmap2`). It is shared with the CBC / Radio-Canada test set. If Docker or root creates it first, the user daemon cannot write to it. |
| `/proc/<pid>/stat` | `listen.rs:198` | walk the parent chain for the listen picker |
| `/proc/asound`, `/dev/snd/pcmC0D0c` | `pipeline.rs:158` | checks that ALSA is present |
| `/sys/class/net/<if>/statistics/multicast` | `testpoint.rs:97` | multicast counter used to diagnose RTP |
| `/etc/ufw/ufw.conf` | `testpoint.rs:81` | detects UFW, so the RTP diagnosis can suggest `ufw allow ...` (`testpoint.rs:914-916`). firewalld is not detected. |
| `web/tap.html`, `web/setup.html` | `http.rs:19`, `setup.rs:4` (`include_str!`) | compiled into the binary, so nothing needs installing beside it |
| `$TMPDIR` / `WBS_TEST_SCRATCH` | `mxl.rs:1888` | tests only |

### 1.3 Sockets and network

| Socket | Where | Notes |
|---|---|---|
| TCP `127.0.0.1:8467` (override `WBS_PORT`) | `http.rs:51-53`, `util.rs:13` | HTTP API, SSE meter stream and UI. Loopback only. |
| UDP `0.0.0.0:9875`, joins `239.255.255.255` and `224.2.127.254` | `aoip.rs:44, 1323` | SAP / SDP listener |
| UDP `0.0.0.0:5060`, joins `224.0.1.75` | `aoip.rs:47, 1324` | SIP multicast listener |
| UDP `0.0.0.0:319` and `:320`, joins `224.0.1.129` | `aoip.rs:45-46, 1326-1328` | PTP, **only when `AOIP_PTP_MODE=sockets`**. These are privileged ports below 1024. The error text says to use `setcap` (`aoip.rs:1176`). |
| UDP `<group>:<port>` (for example 5004) | `testpoint.rs:613-634` | AES67 RTP test point, bound to the group address |
| TCP `MQTT_HOST:MQTT_PORT` (default `127.0.0.1:1883`) | `aoip.rs:70-71, 997` | passive PTP over MQTT (the default mode) |
| TCP `127.0.0.1:9600 / 9601 / 9602 / 9699` | `cbc.rs:16-21, 109, 162, 199`; `http.rs:547-601` | proxy to the CBC MXL test-set services |
| UDP connect to `10.255.255.255:1` | `mxl.rs:410-420` | finds the host's main IPv4 address. Nothing is sent. Override with `WBS_MXL_IP`. |
| PulseAudio native socket | implicit, through `pactl` / `parec` / `pacat` | `$XDG_RUNTIME_DIR/pulse/native`, or `PULSE_SERVER` |
| PipeWire socket | implicit, through `pw-dump` / `pw-metadata` / `wpctl` | `$XDG_RUNTIME_DIR/pipewire-0` |
| System D-Bus | implicit, through `avahi-browse` | `/run/dbus/system_bus_socket` |

The multicast listeners join every interface from `if_nameindex()` (`aoip.rs:1205-1214`,
`aoip/py.rs:316`). If none can be joined, they fall back to `INADDR_ANY`. Set `AOIP_IFACE`
to an IPv4 address to join on one interface only (`aoip.rs:81, 1197-1202`).

### 1.4 Environment variables

| Variable | Default | Where |
|---|---|---|
| `WBS_PORT` | `8467` | `util.rs:13` |
| `WBS_SHM_BASE` | `/dev/shm/mxl` | `util.rs:19` |
| `WBS_DRY_RUN` | off (`1` = never change the audio graph) | `tap.rs:345` |
| `WBS_NO_BROWSER` | `0` | `main.rs:122` |
| `WBS_LAUNCH_URL` | `https://ward-beck.ca` | `main.rs:124` |
| `WBS_CLIENT_TIMEOUT` | `12` seconds (`0` turns the watchdog off) | `tap.rs:1957-1959` |
| `WBS_MXL_IP` | auto-detected | `mxl.rs:410` |
| `AOIP_PTP_MODE` | `mqtt` (`sockets` binds 319/320) | `aoip.rs:69` |
| `AOIP_IFACE` | all interfaces | `aoip.rs:81` |
| `AOIP_PTP_TOPIC` | `APK.audio/System/Gui/Discovered/ptp` | `aoip.rs:72` |
| `MQTT_HOST`, `MQTT_PORT` | `127.0.0.1`, `1883` | `aoip.rs:70-71` |
| `DISPLAY`, `WAYLAND_DISPLAY` | from the session | `main.rs:53`, `listen.rs:170` |
| `XDG_RUNTIME_DIR`, `PULSE_SERVER`, `PIPEWIRE_RUNTIME_DIR` | from the session | read by the client tools, not by the daemon |
| `TP_PW_REF`, `TP_AOIP_REF` | test fixtures | `testpoint.rs:1187, 1205` (tests only) |

### 1.5 Privileged or invasive operations

The TAP needs no root for its core work. It does make invasive changes to the user's own
desktop audio:

1. **It takes over the default sink.**
   - `full_restart()` (`tap.rs:1290`) loads three null sinks (`tap.rs:1299-1305`).
   - It sets `wbs_input_bus` as the default sink (`tap.rs:1307`) and moves every playing
     stream there (`tap.rs:1308`).
   - `abandon_ship()` (`tap.rs:1082`) undoes this. It runs on SIGTERM or SIGINT
     (`main.rs:71-90`) and on `--abandon`.
   - After SIGKILL, an OOM kill or a panic it does not run. The desktop is then left silent,
     routed into a null sink.
2. **Reset All** (`tap.rs:1185`):
   - It unloads every unmanaged null-sink, loopback, remap, combine or virtual module, even
     ones that belong to other programs (`tap.rs:1126-1133, 1219-1230`).
   - It sends SIGTERM to `pw-loopback` processes and to orphaned
     `parec`/`pacat`/`pw-cat`/`pw-play`/`pw-record` processes (`tap.rs:1161-1180, 1236-1243`).
   - A process counts as orphaned when its parent is PID 1 **or when the parent's command
     line contains `systemd`** (`tap.rs:1170`). Under a systemd user session, many desktop
     programs have `systemd --user` as their parent. This test can therefore kill other
     programs' audio helpers. See section 1.7.
3. **Binding PTP ports 319/320** needs `CAP_NET_BIND_SERVICE` (section 5.4). It is only done
   in `AOIP_PTP_MODE=sockets`.
4. Signals are sent with `kill(2)` (`tap.rs:618-631`, `testpoint.rs:366, 448`,
   `mxl.rs:1095`). They go only to the daemon's own children, apart from Reset All above.

### 1.6 Fragile spots by audio stack

| Stack | Status | Details |
|---|---|---|
| **PipeWire with pipewire-pulse and WirePlumber** | Main target | Everything works. Module IDs from pipewire-pulse are large (`536870913`, the test at `tap.rs:1900`); the parser handles that. Null sinks and loopbacks belong to the `pipewire-pulse` process: **if `pipewire-pulse.service` restarts, the WBS buses disappear** while the TAP keeps running. The current unit binds to `pipewire.service`, not to `pipewire-pulse.service`. |
| **PipeWire with pipewire-media-session** (Ubuntu 22.04 when switched to PipeWire, older Fedora) | Works, reduced | There is no `wpctl`, so `pipeline.rs:214` reports empty device, sink and source lists. |
| **PulseAudio only** (Ubuntu 22.04 default) | Core works, reduced | `pactl`, `parec` and `pacat` work. `pw-dump` and `pw-metadata` are missing, so PipeWire test points fail. The `--monitor-stream=<object.serial>` serial comes from `pw-dump` (`testpoint.rs:801-803`). The graph quantum falls back to 1024 (`testpoint.rs:118`). The pipeline report still says `"PipeWire 1.0 (with WirePlumber)"` because the text is fixed (`pipeline.rs:235`). The unit file's `BindsTo=pipewire.service` makes the service **fail to start** (section 4). |
| **pipewire-pulse with module loading turned off** | **Broken** | PipeWire has a `pulse.allow-module-loading` server property. Clients are allowed to load modules by default; setting it to `false` turns that off ([man page](https://man.archlinux.org/man/extra/pipewire-pulse/libpipewire-module-protocol-pulse.7.en)). After CVE-2026-5674 (sandbox escape through `module-ladspa-sink`), turning off module loading is a suggested mitigation ([Red Hat bug 2455341](https://bugzilla.redhat.com/show_bug.cgi?id=2455341)). If a distro or an administrator makes that the default, every `pactl load-module` fails and the TAP cannot build its buses. `load_module` only logs a warning (`tap.rs:1066`). The daemon should check for this at start-up and tell the user. |
| **JACK** (jackd without PipeWire) | Not supported | The TAP only uses the PulseAudio protocol. With `jackd` and PulseAudio bridged by `module-jack-sink`/`-source`, the buses can be created, but only on the Pulse side. With PipeWire's JACK replacement (`pipewire-jack`), JACK programs show up in the same graph, and the TAP can meter them like any other stream. |
| **ALSA only** (no sound server) | Not supported | `pactl` fails. The self-test reports 0 sources (`main.rs:34-35`), and the ALSA layer of the pipeline report still works. |

Other differences between distros:

- `pactl get-default-sink` (`tap.rs:1041`) needs pactl 15 or newer. Every distro in section 2
  has it; Debian 11 (PulseAudio 14.2) does not.
- Ubuntu 24.04 and later move `pactl` into `pulseaudio-utils` even when PipeWire is the
  server. Install `pulseaudio-utils` without the `pulseaudio` daemon.
- On Fedora, `pulseaudio-utils` is the client package and conflicts with nothing. Do not pull
  in the `pulseaudio` daemon package.
- On Arch, `pactl` lives in `libpulse`, which is always installed.
- On Raspberry Pi OS **Lite** there is no desktop audio server. Install `pipewire`,
  `pipewire-pulse` and `wireplumber` and enable them for the user, with linger (section 4.3).
- Under Wayland, `xprop` finds nothing (only XWayland windows), so the listen picker matches
  streams by PID only. This is expected behaviour.

### 1.7 Bugs that matter for deployment

| # | Issue | Where | Effect | Fix |
|---|---|---|---|---|
| B1 | The buses are rebuilt **before** the HTTP port is bound | `main.rs:112` (`full_restart`) runs before `main.rs:133` (`http::bind`) | A second instance (Docker and native together, or the user service plus `./run.sh`) tears down the first instance's buses and builds its own. It then fails to bind and calls `exit(1)` **without cleaning up**. With `Restart=on-failure` this repeats every 3 s and re-routes the desktop audio each time. | Bind first, or take a lock file in `$XDG_RUNTIME_DIR/wbs-tap.lock`; exit early if either fails. Also set `StartLimitBurst` in the unit. |
| B2 | Requests from other websites are not checked | `http.rs:22-29` (`Access-Control-Allow-Origin: *`), `http.rs:214-226` (any `Content-Type` is parsed as JSON) | Any website the user visits can send a "simple" `POST` with a `text/plain` body (no preflight) to `/api/abandon`, `/api/reset_all`, `/api/test_tone` or `/api/bitbuddy`. Header checks are missing too, so a DNS-rebinding page can read `/api/status`. | Section 5.1. |
| B3 | The Reset All test for orphaned processes | `tap.rs:1170` | `parent_args.contains("systemd")` matches `systemd --user`, so other programs' `pw-cat`/`parec` processes are killed. | Compare against the daemon's own PID and its known children only, or check `/proc/<pid>/cgroup` for the TAP's own unit. |
| B4 | The unit does not restore the speakers after an unclean stop | `ward-beck-audio-tab.service` | After a SIGKILL (for example `TimeoutStopSec` running out), the default sink stays `wbs_input_bus`. | `ExecStopPost=%h/.local/bin/ward-beck-audio-tab --abandon` |
| B5 | The service opens a browser | `ward-beck-audio-tab.service:8`, `main.rs:121-130` | GNOME and KDE import `DISPLAY` and `WAYLAND_DISPLAY` into the user manager, so every start or restart opens `https://ward-beck.ca`. | Add `--no-browser` to `ExecStart`, or set `Environment=WBS_NO_BROWSER=1`. |
| B6 | The watchdog covers only the tone and Bit Buddy | `tap.rs:1966-1984` | MXL generators, the CBC generator and the speaker split stay on when the client disappears. Most of these are silent, but this should be documented. | Stop `engine().stop_generators()` in the same place if that is wanted. |

---

## 2. Distro support matrix

| Distro | Default sound server | PipeWire version | glibc | `pactl` package | Notes |
|---|---|---|---|---|---|
| **Ubuntu 22.04 LTS** (jammy) | **PulseAudio** 15.99. PipeWire is installed but used only for screen sharing ([OMG! Ubuntu](https://www.omgubuntu.co.uk/2022/05/ubuntu-22-10-makes-pipewire-default), [gHacks](https://www.ghacks.net/2022/05/23/ubuntu-22-10-dropping-pulseaudio/)) | 0.3.48 (with pipewire-media-session) | 2.35 | `pulseaudio-utils` | The core works on PulseAudio. The unit must not depend on `pipewire.service`. |
| **Ubuntu 24.04 LTS** (noble) | PipeWire with pipewire-pulse and WirePlumber (PipeWire default since 22.10) | 1.0.5 (checked on the build host: `Server Name: PulseAudio (on PipeWire 1.0.5)`) | 2.39 | `pulseaudio-utils` | Main development platform. 1.0.5 is affected by CVE-2026-5674 ([RH 2455341](https://bugzilla.redhat.com/show_bug.cgi?id=2455341)), so watch for a module-loading lock-down in updates. |
| **Ubuntu 26.04 LTS** (resolute) | PipeWire | 1.6.2 ([ubuntuupdates](https://ubuntuupdates.org/package/core/resolute/main/base/pipewire)) | newer | `pulseaudio-utils` | Not tested yet. |
| **Debian 12** (bookworm) | PipeWire with WirePlumber on GNOME; other desktops may still use PulseAudio ([Debian wiki](https://wiki.debian.org/PipeWire), [Phoronix](https://phoronix.com/news/Debian-12-PipeWire)) | 0.3.65; 1.4.2 in bookworm-backports ([tracker](https://tracker.debian.org/pkg/pipewire)) | 2.36 | `pulseaudio-utils` | Distro Rust is too old to build the TAP. Ship binaries. |
| **Debian 13** (trixie) | PipeWire | 1.4.x ([tracker](https://tracker.debian.org/pkg/pipewire)) | 2.41 | `pulseaudio-utils` | |
| **Fedora** (current releases) | PipeWire for all audio since Fedora 34 ([Fedora change](https://fedoraproject.org/wiki/Changes/DefaultPipeWire), [Fedora Magazine](https://fedoramagazine.org/pipewire-the-new-audio-and-video-daemon-in-fedora-linux-34/)) | follows upstream (1.4 / 1.6 series) | 2.40+ | `pulseaudio-utils` (`pw-dump` is in `pipewire-utils`, `avahi-browse` in `avahi-tools`) | **firewalld** is on by default, so the multicast ports must be opened (section 5.3). SELinux: a user binary in `~/.local/bin` runs as `unconfined_t`, so nothing extra is needed. |
| **Arch Linux** | PipeWire is the usual choice (the user picks) | 1.6.11 (2026-11-18) ([archlinux.org](https://archlinux.org/packages/extra/x86_64/pipewire/)) | current | `libpulse` | `avahi-daemon` is not enabled by default. Ship an AUR `PKGBUILD`. |
| **Raspberry Pi OS** bookworm (arm64) | PipeWire: "PulseAudio has been replaced with the newer PipeWire audio system" ([raspberrypi.com](https://www.raspberrypi.com/news/bookworm-the-new-version-of-raspberry-pi-os/)) | 0.3.65 (Debian base) | 2.36 | `pulseaudio-utils` | raspi-config can switch back to PulseAudio. Lite has no audio server. |
| **Raspberry Pi OS** trixie (arm64) | PipeWire. PulseAudio was removed in the November update ([raspberrypi.com](https://www.raspberrypi.com/news/trixie-the-new-version-of-raspberry-pi-os/), [Linuxiac](https://linuxiac.com/november-raspberry-pi-os-update-adds-hidpi-scaling/)) | 1.4.x | 2.41 | `pulseaudio-utils` | Forums report HAT and HDMI audio regressions ([Pimoroni forum](https://forums.pimoroni.com/t/pirate-audio-not-working-on-trixie-os-pi-3b/28449)). Test on hardware. |

Pi performance: each source meter starts its own `parec` (`tap.rs:910`), alongside the main
VU reader (`tap.rs:1406`), the MXL bridge (`mxl.rs:1154`) and an SSE thread per client. A
Pi 4 or 5 is fine. On a Pi 3 or Zero 2 W, limit the number of metered sources.

### 2.1 glibc versus static musl

The current release binary, built on Ubuntu 24.04, needs **GLIBC_2.34**. `objdump -T` shows
two GLIBC_2.39 symbols, `pidfd_spawnp` and `pidfd_getpid`. They are **weak**: Rust's standard
library looks them up at run time and falls back without them. The binary links only `libc`,
`libm` and `libgcc_s`.

- It therefore runs on Ubuntu 22.04 (2.35), Debian 12 (2.36), RHEL 9 (2.34) and anything
  newer, but not on Debian 11 (2.31) or Ubuntu 20.04.
- The dependencies are pure Rust. `tiny_http`, `serde_json`, `memmap2`, `socket2`, `libc`,
  `signal-hook`, and `ureq` with `default-features = false` mean no TLS and no OpenSSL. A
  **static musl build therefore needs no C toolchain**:

  ```bash
  rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl
  cargo build --release --locked --target x86_64-unknown-linux-musl
  # aarch64 from x86_64: cargo install cargo-zigbuild && cargo zigbuild --release --target aarch64-unknown-linux-musl
  ```

- musl's memory allocator is slower than glibc's when many threads allocate. The TAP starts a
  thread per HTTP request and does its DSP in pre-allocated buffers, so expect little
  difference. If profiling shows one, switch the global allocator to `mimalloc`.
- The alternative is to build against glibc in the oldest supported image (a Debian 12 or
  Ubuntu 22.04 container). That gives the same reach without musl, but each architecture
  needs its own build image.

**Recommendation:** use musl static builds for the tarball. Use the same musl binary for the
`.deb` and `.rpm`, so there is one artifact per architecture to test. Debian's
`dpkg-shlibdeps` then finds no shared libraries, so state the dependencies by hand.

---

## 3. Packaging options

| Option | Audio graph (`pactl load-module`) | Multicast and `/dev/shm` | Service integration | Effort | Verdict |
|---|---|---|---|---|---|
| **Tarball + `install.sh`** (prebuilt) | Full | Full | Installs the user unit itself | Low | **Yes**: works on every distro, and the current `install.sh` is close (it only needs to stop requiring cargo). |
| **`.deb` via [cargo-deb](https://github.com/kornelski/cargo-deb)** | Full | Full | `[package.metadata.deb.systemd-units]` handles **system** units only ([systemd.md](https://github.com/kornelski/cargo-deb/blob/main/systemd.md)). Install the user unit as an asset to `/usr/lib/systemd/user/` instead. | Low | **Yes**: covers Ubuntu, Debian and Raspberry Pi OS (arm64). `depends` can name `pulseaudio-utils`, with `pipewire-bin, wireplumber, avahi-utils, alsa-utils` under `recommends`. |
| **`.rpm` via [cargo-generate-rpm](https://github.com/cat-in-136/cargo-generate-rpm)** | Full | Full | No systemd keys. Ship the unit as an asset, and use `post_install_script` for `setcap` if wanted. | Low | **Yes**: Fedora. `requires = { pulseaudio-utils = "*" }`; use `auto-req = "no"` for musl. |
| **AUR `PKGBUILD`** | Full | Full | Install the user unit to `/usr/lib/systemd/user/` | Very low | **Yes**: Arch users expect it. |
| **[dist](https://axodotdev.github.io/cargo-dist/book/) (cargo-dist)** | Full | Full | None. Its shell installer copies the binary and has no service or post-install hook. | Low (CI) | **As the CI builder only**: it produces musl tarballs, checksums and GitHub Releases ([releases](https://github.com/axodotdev/cargo-dist/releases)). Keep our own `install.sh` for the unit. |
| **AppImage** | Full (no sandbox) | Full | None. An AppImage is a program to run, not a service. It also needs FUSE 2, which is `libfuse2t64` on Ubuntu 24.04 ([AppImage docs](https://docs.appimage.org/user-guide/troubleshooting/fuse.html)). | Medium | **No**: it adds FUSE for a single static binary that already runs anywhere. |
| **Flatpak** | `--socket=pulseaudio` gives the audio socket, but loading modules from a sandbox is exactly the attack in CVE-2026-5674, and PipeWire may block it ([RH 2455341](https://bugzilla.redhat.com/show_bug.cgi?id=2455341)) | `--share=network` gives network access ([Flatpak permissions](https://docs.flatpak.org/en/latest/sandbox-permissions.html)), but `/dev/shm` sharing with the host and binding ports below 1024 are not available | No background-service model. `avahi-browse`, `pw-dump`, `ps` and `/proc` of host processes need `flatpak-spawn --host` | High | **No**. |
| **Snap** (strict) | PulseAudio's snap policy refuses module loads from confined snaps ([snapcraft forum](https://forum.snapcraft.io/t/loading-pulseaudio-modules-via-pactl-and-webkit2gtk-inside-a-core20-snap/23358), [audio-playback](https://snapcraft.io/docs/audio-playback-interface)) | needs `network-bind`; `/dev/shm/mxl` is outside the snap's allowed area | Snap services run as root, not as the desktop user | High | **No**. Classic confinement would work but needs manual Snap Store review, and it gains nothing over a `.deb`. |

**Recommendation:**

- Ship a GitHub Release per version with:
  - `ward-beck-audio-tap-<ver>-x86_64-linux-musl.tar.gz`
  - `ward-beck-audio-tap-<ver>-aarch64-linux-musl.tar.gz`
  - `.deb` packages (amd64, arm64) and `.rpm` packages (x86_64, aarch64)
  - SHA-256 checksums
- Build all of them with GitHub Actions: either dist with custom jobs, or plain `cargo
  zigbuild` steps followed by `cargo deb --no-build` and `cargo generate-rpm`.
- Each package contains:
  - `/usr/bin/ward-beck-audio-tab`
  - `/usr/lib/systemd/user/ward-beck-audio-tab.service`
  - `/usr/lib/tmpfiles.d/wbs-mxl.conf` (section 4.5)
  - LICENSE and README
- Packages must **not** enable the service for every user. Starting the TAP takes over that
  user's default sink, so enabling it is an opt-in step for each user:
  `systemctl --user enable --now ward-beck-audio-tab`.

---

## 4. Service model

### 4.1 User service or system service

| | systemd **user** service (current) | systemd **system** service |
|---|---|---|
| Can reach the audio server | Yes. It runs in the user's session with `XDG_RUNTIME_DIR` set, which is where pipewire-pulse lives. | Only with `User=<desktop user>` plus `Environment=XDG_RUNTIME_DIR=/run/user/<uid>`, and only while that user's manager runs (login or linger). Ordering against units in another manager is impossible. |
| Files in `/dev/shm/mxl` | Owned by the user | Owned by root, or by the `User=` account |
| Port 319/320 | Needs file capabilities (`setcap`) or a sysctl. A user manager cannot grant `AmbientCapabilities=`. | `AmbientCapabilities=CAP_NET_BIND_SERVICE` works |
| Multi-user | Each user who opts in gets their own instance. Two at once would fight over `:8467` (bug B1). | One instance per machine |
| Logs | `journalctl --user -u ward-beck-audio-tab` | `journalctl -u ...` |

**Use a user service.** The TAP is a per-desktop-session tool: it rewires that user's
PipeWire graph. A system service would only stand in for a user service while gaining the
PTP capability, and there is a simpler route to that (section 5.4).

### 4.2 Recommended unit

```ini
# /usr/lib/systemd/user/ward-beck-audio-tab.service  (or ~/.config/systemd/user/)
[Unit]
Description=WARD BECK TAP - virtual audio patchbay, VU bridge, MXL rings
Documentation=http://127.0.0.1:8467/setup
# Order after whichever audio server exists; Wants= ignores units that are missing
# (systemd.unit(5): Wants= failures "have no impact on the validity of the transaction").
After=pipewire.service pipewire-pulse.service wireplumber.service pulseaudio.service
Wants=pipewire-pulse.service
# The WBS buses live inside pipewire-pulse: restart with it.
PartOf=pipewire-pulse.service
StartLimitIntervalSec=60
StartLimitBurst=5

[Service]
Type=simple
Environment=WBS_NO_BROWSER=1
# Wait (max ~20 s) until the PulseAudio-protocol server answers.
ExecStartPre=/bin/sh -c 'for i in $(seq 40); do pactl info >/dev/null 2>&1 && exit 0; sleep 0.5; done; exit 1'
ExecStart=/usr/bin/ward-beck-audio-tab --no-browser
# Safety net: restore the speakers even after SIGKILL / crash.
ExecStopPost=/usr/bin/ward-beck-audio-tab --abandon
Restart=on-failure
RestartSec=3
TimeoutStopSec=20
KillMode=control-group

[Install]
WantedBy=default.target
```

Changes from the current unit, and why:

- **`BindsTo=pipewire.service` is removed.**
  - On a host running only PulseAudio (Ubuntu 22.04 by default), the unit then pulled in a
    unit that does not exist, and the start failed.
  - On PipeWire it was tied to the wrong process: the buses belong to `pipewire-pulse`.
  - `Wants=` tolerates units that are missing
    ([systemd.unit(5)](https://www.freedesktop.org/software/systemd/man/latest/systemd.unit.html)).
  - `PartOf=` passes stops and restarts of `pipewire-pulse.service` on to the TAP, so the
    buses are rebuilt.
  - Debian has patched PipeWire's own units for the same ordering problems
    ([Debian patch](https://sources.debian.org/data/main/p/pipewire/1.4.7-3/debian/patches/Fix_services.patch),
    [RH 1943867](https://bugzilla.redhat.com/show_bug.cgi?id=1943867)).
- **`ExecStartPre` waits for `pactl info`.** pipewire-pulse is socket-activated, so the first
  `pactl` call starts it anyway, but WirePlumber may not have created the hardware sinks yet.
  Without the wait, `detect_physical_sink()` (`tap.rs:1039`) can come back empty and the
  speaker split has nowhere to go. The check could later move into the daemon itself, which
  would also cover running from `./run.sh`.
- **`ExecStopPost=--abandon`** fixes bug B4. It is safe to run twice, because it only
  unloads modules whose arguments contain `wbs_` (`tap.rs:1070-1078`).
- **`--no-browser`** fixes bug B5.
- **`StartLimitBurst`** limits the restart loop from bug B1.
- **`KillMode=control-group`** (the default, stated for clarity) also reaches the `pacat`
  helpers, which leave the session with `setsid()` (`tap.rs:1481-1486`) but stay in the
  unit's cgroup.
- Do **not** add `NoNewPrivileges=yes` if you use file capabilities for PTP: it turns them
  off. The usual hardening options (`ProtectSystem=`, `PrivateTmp=`) are mostly unavailable
  in user units without user namespaces, and they would hide `/dev/shm` and `/proc`, which
  the TAP needs. Leave them out.

### 4.3 Linger and headless machines

- **On a desktop**, nothing extra is needed. The user manager starts at login, and
  `default.target` pulls in the TAP.
- **On a headless machine** (Pi rack unit, server), the user manager must run without a
  login. Lingering lets it start at boot and keep running without an open session
  ([Arch wiki: User unit](https://wiki.archlinux.org/title/User_unit)). Set it up like this:

  ```bash
  sudo loginctl enable-linger "$USER"
  systemctl --user enable --now pipewire pipewire-pulse wireplumber ward-beck-audio-tab
  ```

- Run `systemctl --user` from a real login, or after `su - user`. A plain `su` does not set
  `XDG_RUNTIME_DIR` ([Arch forum](https://bbs.archlinux.org/viewtopic.php?id=290265)). The
  installer should check this and print the fix.
- Linger also matters for MXL. logind's `RemoveIPC=yes` is the default, and it deletes the
  user's "POSIX shared memory" when the user "fully logs out"
  ([logind.conf(5)](https://www.freedesktop.org/software/systemd/man/latest/logind.conf.html)).
  That includes the user's files under `/dev/shm/mxl`. With linger the user does not fully
  log out, so the MXL rings survive. Without linger, the rings are wiped at logout. That is
  harmless, because the daemon is stopped too, but other MXL readers lose them.
- Some forum reports describe WirePlumber holding back ALSA devices while no session is
  active. Bluetooth is the common case
  ([Arch list](https://lists.archlinux.org/archives/list/arch-general@lists.archlinux.org/thread/ENLWGLMDU4C736FRNNMFJPMMVDHVDKCJ/)).
  On a headless Pi, check `wpctl status` after a reboot before relying on the speaker split.

### 4.4 The safety stop (client watchdog)

From `tap.rs:1934-1985`, `http.rs:159-161, 493` and `main.rs:116`:

- **What counts as a check-in:** any request with a `Sec-Fetch-Mode` header (every request a
  browser sends), or `POST /api/heartbeat`. The heartbeat replies
  `{"idle_limit_s": <limit>}`. XTM polls every 4.5 s and the TAP page every 4 s.
- **The rule:** a 1 s loop stops the **POD-33 test tone** and the **ABB-1 Bit Buddy / ABP-1
  Bit Spitter** when either is on and no client has checked in within `WBS_CLIENT_TIMEOUT`
  seconds (default 12; `0` turns the watchdog off). If no client has checked in since
  start-up, the limit counts as already passed (`watchdog_expired`, `tap.rs:1962`).
- **What it does not cover:** the WBS buses and routes, the speaker split, MXL generators, and
  the CBC test-set generator (bug B6).

What this means for deployment:

- **Keep the default** for desktop installs. A tone stays on only while a page that drives
  it is open.
- `curl` and scripts send no `Sec-Fetch-Mode`. Automation that turns on a tone must
  `POST /api/heartbeat` at least every 12 s, or set `WBS_CLIENT_TIMEOUT` higher in a drop-in
  (`systemctl --user edit ward-beck-audio-tab`). **Do not set `0` on a machine that drives
  speakers**: the stop is a guard for hearing and equipment.
- Because the check-in is "any browser request from anywhere", a stray tab from any website
  that polls `:8467` also keeps the tone alive. This is another reason for the Origin check
  in section 5.1.

### 4.5 `/dev/shm/mxl` ownership

The CBC test set and Docker may create `/dev/shm/mxl` as root before the native daemon
starts. The package can ship a tmpfiles.d rule so the folder always exists with shared
permissions:

```
# /usr/lib/tmpfiles.d/wbs-mxl.conf
d /dev/shm/mxl 1777 root root -
```

Mode 1777 lets every local user create a domain, while the sticky bit stops users from
deleting each other's domains. For a stricter setup, use `2775 root audio` and put the MXL
users in the `audio` group.

---

## 5. Security

### 5.1 HTTP: CORS, Private Network Access / Local Network Access, origin checks

What the daemon does today:

- It binds **127.0.0.1 only** (`http.rs:51-53`). That is correct; keep it, and never
  offer a `0.0.0.0` option without authentication.
- It sends `Access-Control-Allow-Origin: *`, `Allow-Methods: GET, POST, OPTIONS`, and the
  Private Network Access (PNA) header `Access-Control-Allow-Private-Network: true` on every
  reply (`http.rs:22-29`). `OPTIONS` answers 204 (`http.rs:165`).

What browsers do now:

- **Chrome** dropped PNA preflights in favour of **Local Network Access (LNA)**, a
  permission prompt that shipped in **Chrome 142**. It covers requests from public sites to
  loopback (127.0.0.0/8) ([Chrome blog](https://developer.chrome.com/blog/local-network-access),
  [LNA guide](https://developer.chrome.com/docs/modern-web-guidance/guides/security/local-network-access)).
  `Access-Control-Allow-Private-Network` is now deprecated. It is harmless to keep, but it no
  longer grants anything.
- **Firefox** rolls out the same kind of prompt: it started for Strict tracking protection
  in Firefox 149, began reaching everyone from 151, and is on by default for all users in the
  153 beta ([Mozilla support](https://support.mozilla.org/en-US/kb/control-personal-device-local-network-permissions-firefox),
  [enterprise policy](https://firefox-admin-docs.mozilla.org/reference/policies/localnetworkaccess/)).
- So `xtm.wbsps.ca` users will see a one-time "allow access to local devices" prompt. The
  setup guide should say so. Managed fleets can pre-grant it through the Chrome or Firefox
  enterprise policy.

The gap: these prompts protect users who answer "Block", but the daemon itself trusts every
origin.

- `POST` endpoints accept a `text/plain` body (`http.rs:214-226`). Such a request is a CORS
  "simple request" with no preflight. In browsers without LNA, or after the user has allowed
  one site, any page can trigger `/api/abandon`, `/api/reset_all`, `/api/test_tone`,
  `/api/bitbuddy` and `/api/force_speakers`.
- The `Host` header is not checked, so DNS rebinding can read the API.

Recommended code changes, all small and in `http.rs`:

1. **Origin allow-list.** Answer with `Access-Control-Allow-Origin: <origin>` plus
   `Vary: Origin`, only for `https://xtm.wbsps.ca`, `https://ward-beck.ca`,
   `http://127.0.0.1:<port>` and `http://localhost:<port>`. Add extra origins through a new
   `WBS_ALLOWED_ORIGINS` variable.
2. **Refuse state-changing requests** (`POST`, `PUT`, `DELETE`) whose `Origin` header is
   present and not on the list. Answer 403.
3. **Require `Content-Type: application/json`** on POSTs that carry a body. This forces a
   preflight from any other website.
4. **Check the `Host` header.** Accept only `127.0.0.1:<port>`, `localhost:<port>` and
   `[::1]:<port>`, and answer 421 otherwise. This blocks DNS rebinding.
5. Keep the deprecated PNA header for older Chromium builds, and drop `Authorization` from
   `Allow-Headers` unless a token is added.
6. Optional: a per-install token in `$XDG_CONFIG_HOME/wbs-tap/token`, shown once on the
   local dashboard and stored by XTM. Do this only if the TAP is ever exposed beyond
   loopback.

### 5.2 Multicast

- The SAP (9875) and SIP (5060) listeners bind `0.0.0.0` with `SO_REUSEADDR` and
  `SO_REUSEPORT` (`aoip.rs:1191-1193`) and only receive.
- They join on **every** interface (`aoip.rs:1205`), including VPN and Docker bridges. On
  multi-homed machines, set `AOIP_IFACE` to the media network's address.
- The daemon never sends multicast. The SIP listener sits on 5060 on all interfaces: a local
  SIP stack using 5060 can still bind thanks to `SO_REUSEADDR`, but unicast SIP may then go
  to either socket. Document this for intercom hosts.

### 5.3 Firewall rules

UFW and firewalld both block unsolicited inbound UDP by default, and that includes multicast
UDP. Inbound rules are needed only for the features you use. The TAP's own HTTP traffic is
on loopback and is never filtered.

| Feature | Traffic | ufw | firewalld |
|---|---|---|---|
| SAP / SDP discovery | UDP 9875 to 239.255.255.255 and 224.2.127.254 | `sudo ufw allow in on <if> proto udp to 224.0.0.0/4 port 9875` | `sudo firewall-cmd --permanent --zone=<zone> --add-port=9875/udp` |
| SIP multicast | UDP 5060 to 224.0.1.75 | `sudo ufw allow in on <if> proto udp to 224.0.1.75 port 5060` | `--add-service=sip` (5060/tcp+udp) or `--add-port=5060/udp` |
| PTP (sockets mode) | UDP 319/320 to 224.0.1.129 | `sudo ufw allow in on <if> proto udp to 224.0.1.129 port 319:320` | `--add-service=ptp` (firewalld ships `ptp.xml` = 319/udp, 320/udp; [source](https://git.stg.centos.org/source-git/firewalld/blob/6b549668bd7483d20c3db29aa2849e7ec0d94b05/f/config/services/ptp.xml)) |
| AES67 RTP test point | UDP `<port>` (often 5004) to 239.0.0.0/8 | `sudo ufw allow in on <if> proto udp to 239.0.0.0/8 port 5004` (the daemon suggests this itself, `testpoint.rs:914-916`) | `--add-port=5004/udp`, or a rich rule limited to `destination address="239.0.0.0/8"` |
| mDNS (Avahi) | UDP 5353 to 224.0.0.251 | allowed by UFW's default `before.rules` | `--add-service=mdns` (on by default in Fedora Workstation's zone) |
| IGMP | IP protocol 2 | allowed by the default rules | allowed by default |

- With firewalld, finish with `sudo firewall-cmd --reload`.
- Packages should **not** change firewall rules. The `/setup` page should detect them
  instead: extend `ufw_enabled()` (`testpoint.rs:80`) to also check
  `firewall-cmd --state`, and show the right command.
- Switches need IGMP snooping with a querier on the media VLAN, or groups may never arrive.
  This is outside the host, but it is the most common reason for "no SAP" reports.

### 5.4 PTP ports 319/320 and capabilities

- **Default `AOIP_PTP_MODE=mqtt` needs no privilege.** It reads a retained topic from the APK
  MQTT broker (`aoip.rs:18-22`). Keep it as the packaged default.
- **`AOIP_PTP_MODE=sockets`** binds UDP 319 and 320, which are below the unprivileged-port
  limit. There are three ways to allow it:
  1. **File capability** (works with a user service):
     `sudo setcap cap_net_bind_service=+ep /usr/bin/ward-beck-audio-tab`. This is what the
     error text suggests (`aoip.rs:1176`).
     - Every upgrade or `install` replaces the file and clears the capability, so the
       `.deb`/`.rpm` post-install script, or `install.sh`, must run it again.
     - `~/.local/bin` on a `nosuid` mount ignores file capabilities.
     - `NoNewPrivileges=yes` in the unit also turns them off.
     - Grant only `cap_net_bind_service`. Raw sockets (`CAP_NET_RAW`) and admin rights
       (`CAP_NET_ADMIN`) are **not** needed; joining a group needs no privilege.
  2. **sysctl** `net.ipv4.ip_unprivileged_port_start=319`. This is simple, but it lets every
     local user bind ports 319 to 1023. Use it only on dedicated appliances.
  3. **System service** with `AmbientCapabilities=CAP_NET_BIND_SERVICE` (section 4.1). Not
     recommended.
- **Sharing the ports with `ptp4l`:** linuxptp also sets `SO_REUSEADDR` before binding
  319/320 ([udp.c](https://sources.debian.org/src/linuxptp/4.2-1/udp.c)), so both can usually
  bind. Multicast datagrams reach both sockets, but unicast PTP (hybrid or unicast
  profiles) reaches only one of them.
  - On hosts that run `ptp4l`, prefer MQTT mode, or read `pmc` output instead.
  - Never run the TAP as the PTP time source: it only listens.

---

## 6. Recommended plan

| Step | Work | Effort |
|---|---|---|
| 1 | **Code fixes for deployment:** bind (or lock) before `full_restart` (B1); fix the orphaned-process test in Reset All (B3); log a clear error when `load-module` is refused (`pulse.allow-module-loading=false`); stop writing a fixed `PipeWire 1.0` (read `pactl info` "Server Name"). | 1-1.5 days |
| 2 | **HTTP hardening** (section 5.1): Origin allow-list, 403 for other origins, JSON content-type check, Host check, `WBS_ALLOWED_ORIGINS`. Check with XTM in Chrome 142+ and Firefox 151+ (LNA prompt). | 1 day |
| 3 | **New user unit** (section 4.2) and **`install.sh` v2**: installs a prebuilt binary (uses cargo only when run from a checkout without a release binary); checks for `pactl` and offers the right package command per distro (`/etc/os-release`); checks for a user session (`XDG_RUNTIME_DIR`); asks before `loginctl enable-linger` and before `setcap`. Update `uninstall.sh` to match. | 1 day |
| 4 | **Release CI** (GitHub Actions): musl x86_64 and aarch64 via `cargo zigbuild`; `cargo test --lib`; `--check` smoke test inside Debian 12, Ubuntu 22.04/24.04, Fedora and Arch containers with `WBS_DRY_RUN=1 WBS_PORT=18467 WBS_SHM_BASE=/tmp/mxl-ci` (never touches a real audio graph); tarballs and SHA-256 sums. dist can replace hand-written jobs. | 1.5-2 days |
| 5 | **`.deb` and `.rpm`**: `[package.metadata.deb]` and `[package.metadata.generate-rpm]` in `Cargo.toml` with the binary, user unit, tmpfiles rule and docs; `recommends` / weak dependencies for PipeWire tools and Avahi; post-install prints "run `systemctl --user enable --now ward-beck-audio-tab`". Optional apt repository on GitHub Pages later. | 1 day |
| 6 | **AUR `PKGBUILD`** (binary and `-git` variants). | 0.5 day |
| 7 | **Hardware test pass**: Ubuntu 22.04 (PulseAudio), Ubuntu 24.04, Debian 12 GNOME, Fedora (firewalld), Arch, Pi 5 and Pi 4 on Raspberry Pi OS trixie arm64 (desktop and Lite with linger). Check bus creation, speaker restore after `kill -9` (ExecStopPost), restart of pipewire-pulse, the watchdog, and SAP/PTP with the firewall on. | 2 days |
| 8 | **Docs**: update README requirements (PulseAudio-only is supported with reduced features), `/setup` page sections for firewall and LNA prompts, and this document's matrix after testing. | 0.5 day |

**Total: about 8.5-9.5 working days**, or roughly two weeks for one developer, including
hardware testing. Steps 1-3 alone (about 3 days) make the current from-source `install.sh`
safe to hand out. Steps 4-6 make it a one-command install with no Rust.

---

## Sources

- PipeWire pulse module and `pulse.allow-module-loading`: https://man.archlinux.org/man/extra/pipewire-pulse/libpipewire-module-protocol-pulse.7.en
- CVE-2026-5674, PipeWire module loading from sandboxes: https://bugzilla.redhat.com/show_bug.cgi?id=2455341
- Ubuntu 22.10 switches to PipeWire (22.04 stays on PulseAudio): https://www.omgubuntu.co.uk/2022/05/ubuntu-22-10-makes-pipewire-default, https://www.ghacks.net/2022/05/23/ubuntu-22-10-dropping-pulseaudio/
- Ubuntu 26.04 PipeWire version: https://ubuntuupdates.org/package/core/resolute/main/base/pipewire
- Debian PipeWire: https://wiki.debian.org/PipeWire, https://tracker.debian.org/pkg/pipewire, https://phoronix.com/news/Debian-12-PipeWire
- Fedora PipeWire default: https://fedoraproject.org/wiki/Changes/DefaultPipeWire, https://fedoramagazine.org/pipewire-the-new-audio-and-video-daemon-in-fedora-linux-34/
- Arch PipeWire package: https://archlinux.org/packages/extra/x86_64/pipewire/
- Raspberry Pi OS: https://www.raspberrypi.com/news/bookworm-the-new-version-of-raspberry-pi-os/, https://www.raspberrypi.com/news/trixie-the-new-version-of-raspberry-pi-os/, https://linuxiac.com/november-raspberry-pi-os-update-adds-hidpi-scaling/, https://forums.pimoroni.com/t/pirate-audio-not-working-on-trixie-os-pi-3b/28449
- cargo-deb: https://github.com/kornelski/cargo-deb, https://github.com/kornelski/cargo-deb/blob/main/systemd.md
- cargo-generate-rpm: https://github.com/cat-in-136/cargo-generate-rpm
- dist (cargo-dist): https://axodotdev.github.io/cargo-dist/book/, https://github.com/axodotdev/cargo-dist/releases
- AppImage and FUSE: https://docs.appimage.org/user-guide/troubleshooting/fuse.html
- Flatpak sandbox permissions: https://docs.flatpak.org/en/latest/sandbox-permissions.html
- Snap audio and module loading: https://snapcraft.io/docs/audio-playback-interface, https://forum.snapcraft.io/t/loading-pulseaudio-modules-via-pactl-and-webkit2gtk-inside-a-core20-snap/23358
- systemd units and logind: https://www.freedesktop.org/software/systemd/man/latest/systemd.unit.html, https://www.freedesktop.org/software/systemd/man/latest/logind.conf.html, https://wiki.archlinux.org/title/User_unit
- PipeWire unit ordering: https://bugzilla.redhat.com/show_bug.cgi?id=1943867, https://sources.debian.org/data/main/p/pipewire/1.4.7-3/debian/patches/Fix_services.patch
- Headless PipeWire with linger: https://lists.archlinux.org/archives/list/arch-general@lists.archlinux.org/thread/ENLWGLMDU4C736FRNNMFJPMMVDHVDKCJ/, https://bbs.archlinux.org/viewtopic.php?id=290265
- Chrome Local Network Access: https://developer.chrome.com/blog/local-network-access, https://developer.chrome.com/docs/modern-web-guidance/guides/security/local-network-access
- Firefox Local Network Access: https://support.mozilla.org/en-US/kb/control-personal-device-local-network-permissions-firefox, https://firefox-admin-docs.mozilla.org/reference/policies/localnetworkaccess/
- firewalld PTP service: https://git.stg.centos.org/source-git/firewalld/blob/6b549668bd7483d20c3db29aa2849e7ec0d94b05/f/config/services/ptp.xml
- linuxptp UDP transport: https://sources.debian.org/src/linuxptp/4.2-1/udp.c
