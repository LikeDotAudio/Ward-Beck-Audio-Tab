<!-- Copyright (c) 2026 Ward-Beck Systems (Ward-Beck.ca). All rights reserved. -->

# WARD BECK TAP on Windows 10/11 (native)

Research and port plan for running the `ward-beck-audio-tab` daemon (HTTP API on `127.0.0.1:8467`, used by
xtm.wbsps.ca) natively on Windows, without WSL. Researched November 2026|59.

**Short version.** The daemon is far closer to portable than its Linux feature list suggests. A Windows
`cargo check` stops on only **38 errors**, all of them in five files, because almost every Linux dependency is a
*runtime* shell-out (`pactl`, `parec`, `pacat`, `pw-dump`, `wpctl`, `amixer`, `avahi-browse`, `ps`, `ip`,
`xprop`, `xdg-open`) rather than a compile-time API. On Windows those spawns simply fail, so after the compile
fixes the daemon would start and serve HTTP but hear nothing. The work is to put audio, buses, processes, shared
memory and network helpers behind a `platform` module and give Windows real backends. The MVP (VU meters for XTM
from WASAPI loopback, no virtual buses) needs no driver and no undocumented API.

---

## 1. `cargo check --target x86_64-pc-windows-gnu`

The `x86_64-pc-windows-gnu` std target was added with `rustup target add`, and no other toolchain change was
made. Mingw was not needed. Every dependency type-checks for Windows, including `tiny_http`, `serde_json`,
`memmap2`, `socket2`, `ureq`, `signal-hook` and the `wbs-generators` submodule, which is pure Rust with no
dependencies. The `ward-beck-audio-tab` lib fails with **38 errors**:

| Module | Errors | Cause | Windows fix |
|---|---:|---|---|
| `apps/mxl/src/lib.rs` | 13 | `use std::os::unix::io::AsRawFd` (l.19); `libc::posix_fallocate` + `as_raw_fd` (l.687); `libc::kill/pid_t` liveness/terminate (l.1095); `libc::flock` LOCK_UN / LOCK_EX\|LOCK_NB + `as_raw_fd` (l.1127, l.1417); `std::os::unix::fs::symlink` (l.1352) | `File::set_len` only (NTFS needs no fallocate); `std::fs::File::lock/try_lock/unlock` (stable since Rust 1.89, maps to `LockFileEx`); `Child::kill`; directory junction or no alias in place of the symlink |
| `apps/patchbay/src/lib.rs` | 13 | `libc::kill/pid_t` (l.620, 950, 954, 1453); `libc::getuid` for `ps -u` (l.1237); `std::os::unix::process::CommandExt::pre_exec` + `libc::setsid` (l.1465-1482); `libc::SIGKILL` (l.1492) | `platform::proc` (`TerminateProcess` via `Child::kill`, `CREATE_NEW_PROCESS_GROUP` creation flag, process list via `sysinfo`/Toolhelp) |
| `utils/pyshim/src/sock.rs` | 6 | `libc::ESHUTDOWN` in errno mapping (l.268); `libc::if_nameindex/if_freenameindex/socklen_t/AF_INET/AF_INET6` (l.316-340) | Map `io::ErrorKind` (Windows returns WSA codes such as 10013, not errno); interface list via `GetAdaptersAddresses` (`if-addrs`/`netdev` crate); `ntop` via `std::net` |
| `apps/receivers/src/lib.rs` | 5 | `libc::kill/pid_t` (l.366, 448); `Socket::set_reuse_port` (l.613) | `platform::proc`; `#[cfg(unix)]` around `set_reuse_port` (Windows `SO_REUSEADDR` already shares multicast ports) |
| `apps/aoip/src/lib.rs` | 1 | `Socket::set_reuse_port` (l.1192) | same as above |

There are also errors that are **hidden** because the bin crate is never reached after the lib fails:

- `host/src/main.rs` uses `signal_hook::iterator::Signals`. That module is `#[cfg(all(not(windows), feature = "iterator"))]`
  in signal-hook 0.3.18, so it will not compile on Windows. Use the `ctrlc` crate with the `termination`
  feature, or `SetConsoleCtrlHandler`, plus `windows-service` for service stop.
- Test code (`#[cfg(test)]`) was not checked. It holds Linux fixture strings but no OS calls.

Spawns and paths that **compile but do nothing on Windows**, which is the real porting work:

| Where | Linux mechanism | Effect on Windows today |
|---|---|---|
| `tap.rs` (15× `pactl`, 3× `parec`, 2× `pacat`) | null sinks, loopbacks, `move-sink-input`, meters, POD-33 tone/Bit Buddy playback | no sources, no meters, routing no-ops |
| `testpoint.rs` | `parec` taps, `pw-dump`, `/sys/class/net/*/statistics/multicast`, `ip route get`, `/etc/ufw/ufw.conf` | test points fail ("parec ended") |
| `mxl.rs` | `parec` edge bridge, `/dev/shm/mxl` | the engine writes under a non-existent `/dev/shm` |
| `listen.rs` | `pactl list sink-inputs`, `/proc/<pid>/stat`, `xprop` | listen picker shows no apps |
| `pipeline.rs` | `amixer`, `aplay`, `/proc/asound`, `pw-dump`, `wpctl`, `avahi-browse` | empty ALSA/PipeWire layers |
| `nmos.rs` | `avahi-browse -r -t -p _nmos-node._tcp` | no NMOS discovery |
| `main.rs` | `xdg-open`, `DISPLAY` check | no browser launch (`cmd /c start` / `ShellExecuteW`) |
| `util.rs` | `shm_base()` = `/dev/shm/mxl` | wrong default path |

---

## 2. Linux mechanism to Windows equivalent

### 2.1 Audio capture and playback (`parec` / `pacat`)

| Need | Windows API | Rust crate | Maturity |
|---|---|---|---|
| Capture from a microphone or line-in endpoint | WASAPI shared-mode capture | `wasapi` 0.27 (HEnquist), `cpal` 0.18 | Both stable and widely used. `wasapi` exposes more of the WASAPI surface. |
| "Monitor" of an output, i.e. `<sink>.monitor` | WASAPI **system loopback**: `AUDCLNT_STREAMFLAGS_LOOPBACK` on a *render* endpoint, shared mode only. Event-driven loopback works from Windows 10 1703. | `wasapi` (loopback example). `cpal` builds an input stream on an output device (its WASAPI host sets `AUDCLNT_STREAMFLAGS_LOOPBACK`). | Stable since Vista. |
| Per-application tap, i.e. `--monitor-stream=<serial>` | **Application Loopback** via `ActivateAudioInterfaceAsync` with `AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK`, include or exclude a process tree. It is not tied to an endpoint. | `wasapi::AudioClient::new_application_loopback_client(pid, include_tree)` (`record_application` example) | Docs list "Windows 10 build 20348". In practice it was backported to Windows 10 2004 (19041), and OBS 28 ships it as "Application Audio Capture" for 2004+. Some 20H2 reports fail, so the daemon should probe at runtime. |
| Format conversion to the daemon's s16le 24 kHz mono / float32 | `AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM` (+ `SRC_DEFAULT_QUALITY`) in shared mode | `wasapi` (`autoconvert` flag on initialise) | Stable. The docs say the default-quality SRC is fine "for populating a meter". |
| Playback of a tone or Bit Buddy (`pacat --playback`) | WASAPI shared render | `wasapi` / `cpal` | Stable |
| App/stream list (`pactl list sink-inputs`) | `IAudioSessionManager2` → `IAudioSessionEnumerator` → `IAudioSessionControl2::GetProcessId` | `wasapi` (`processes` example lists the processes on each device with peak levels) | Documented API |
| Low-latency pro interfaces | ASIO (bypasses the Windows mixer) | `cpal` `asio` feature | Works, but needs the Steinberg SDK and LLVM at build time. Not needed for meters. |

Gotcha: a loopback client usually receives *no packets* while nothing is rendering. The meter reader must time
out and feed silence so that meters fall to the floor instead of freezing.

### 2.2 Virtual buses (`module-null-sink`, `module-loopback`)

The Linux topology is `wbs_input_bus` (default sink, all apps moved to it) feeding `wbs_meter_tap`,
`wbs_insert_bus`, `wbs_test_point` and the physical speakers through `module-loopback`. Windows has **no
user-mode way to create an audio endpoint**. A new render or capture device always needs a kernel audio driver.

| Option | What it gives | Licence / distribution | Fit |
|---|---|---|---|
| **WASAPI Application Loopback** (no driver) | Per-process or "everything except this process" capture from any endpoint | Built in (Windows 10 2004+) | Covers *taps*, meaning meters and test points per app. It cannot *re-route* audio or create an insert bus. **Use first.** |
| **VB-Audio VB-CABLE** | 1 virtual cable (A+B, C+D add-ons), XP to Windows 11 incl. ARM64 | Donationware. It may be bundled or silently installed if the user can identify it as VB-Audio donationware. Professional or enterprise deployments must buy licences. A+B and C+D may **not** be bundled. | Good optional "bus" when present: detect the endpoint "CABLE Input/Output" and use it as `wbs_input_bus`. |
| **Voicemeeter** (Standard/Banana) | Virtual mixer with several virtual I/O and a free Remote API | Same donationware bundling rules. Remote API free for commercial use. Potato may not be bundled. | Too heavy and too opinionated to be the daemon's bus. Could be an optional backend. |
| **Virtual Audio Cable** (Muzychenko) | Many configurable cables | Paid per seat (Business licence 50 USD, falling with volume). Custom-named builds and source licences are by arrangement. | Best product fit for many buses, but per-seat cost and third-party branding. |
| **Microsoft SYSVAD** sample | WaveRT virtual endpoints to start from | Sample code. It "does not implement audio mixing", its loopback returns a sine tone, and it needs signing or TESTSIGNING. | Starting point only, not shippable. |
| **Custom driver** (SYSVAD/simpleaudiosample-derived "virtual cable", or ACX on newer builds) | Our own `WBS Input Bus` / `WBS Meter Tap` endpoints, named as on Linux | Must be signed. **Attestation signing** needs an EV certificate and a Partner Center (Hardware Dev Center) account. It works on **Windows 10/11 desktop** but not Windows Server 2016+. Attestation-signed drivers cannot go to retail Windows Update, and full WHQL/HLK is needed for that. | Long-term answer if a true patchbay is needed. Weeks of kernel work plus ongoing signing cost. Open-source precedents exist (AudioMirror, Virtual Audio Wire, virtual-ac3-encoder), but none ships a signed general cable. |

**Recommendation.** The MVP has no buses. Phase 2 uses Application Loopback for taps. Phase 3 adds an optional
`ExternalCable` backend that recognises VB-CABLE or VAC endpoints, and decides on a custom signed driver only
if the insert/split routing is a product requirement on Windows.

### 2.3 Routing and default-device control (`pactl set-default-sink`, `move-sink-input`)

| Linux | Windows | Status |
|---|---|---|
| `pactl set-default-sink X` | `IPolicyConfig::SetDefaultEndpoint(id, role)`. This is undocumented COM and is what AudioDeviceCmdlets (MIT, PowerShell `Set-AudioDevice`) and EarTrumpet use. Rust: `com-policy-config` crate (MIT, v0.6, on `windows` 0.61). | Undocumented but stable for over a decade. Global, not per-app. |
| `pactl move-sink-input <id> <sink>` | Per-app default endpoint via `IAudioPolicyConfigFactory::SetPersistedDefaultAudioEndpoint(pid, flow, role, deviceId)` (`Windows.Media.Internal.AudioPolicyConfig`). The same mechanism sits behind Settings → "App volume and device preferences". | **Undocumented and has changed shape.** EarTrumpet carries separate 21H2 and down-level interface variants. Treat as best-effort, version-gated, and off by default. |
| `pactl load-module module-loopback` | No equivalent. Do it in user space: capture with WASAPI and render to another endpoint (latency about 20-30 ms in shared mode). | Implementable in the daemon. |

### 2.4 Shared memory (MXL / EBU DMF rings under `/dev/shm/mxl`)

- **Upstream MXL SDK** (github.com/dmf-mxl/mxl): its build docs cover Linux (GCC/Clang presets, Ubuntu) and macOS
  (Darwin/Clang). Windows is mentioned only as "development on WSL2". **There is no native Windows MXL SDK.** A
  Windows host therefore cannot share rings with SDK-based MXL producers or consumers except inside WSL2 or a
  Linux VM.
- **Our engine** (`mxl.rs`) is a pure-Rust re-implementation that writes `domain/flow/{ring.header,ring.data,
  flow_def.json,...}` files and `memmap2`-maps them. `memmap2` already uses `CreateFileMapping`/`MapViewOfFile`
  on Windows, so the ring logic, header packing (`<IIIIQQIIIIII8s>`) and HTTP readers port unchanged.
- **Named sections vs files.** `CreateFileMapping(INVALID_HANDLE_VALUE, …, "Local\\wbs-mxl-…")` gives
  pagefile-backed memory like `/dev/shm`, and `Global\\` needs `SeCreateGlobalPrivilege`. However, it would break
  the directory-based domain/flow discovery that both the engine and XTM rely on. **Keep files.** Put them in
  `%LOCALAPPDATA%\WardBeck\mxl` (already overridable through `WBS_SHM_BASE`) and open them with
  `FILE_ATTRIBUTE_TEMPORARY`, so the cache manager keeps them in RAM. A RAM disk works too.
- `flock` becomes `std::fs::File::try_lock`, which maps to `LockFileEx`. `posix_fallocate` is dropped, because
  `set_len` allocates on NTFS. The `wbs-test-signals` → `domain-wbs-test-signals` symlink needs Developer Mode
  or admin on Windows. Use a directory junction, or resolve the alias in code.

### 2.5 Signals and processes

| Linux | Windows |
|---|---|
| `signal_hook::iterator` SIGTERM/SIGINT | `ctrlc` (`termination` feature) or `SetConsoleCtrlHandler`; `windows-service` 0.8 (Mullvad, MIT/Apache) for a Service Control Manager Stop |
| `kill(pid, SIGTERM)` then `SIGKILL` on our own `parec` children | Most children disappear once WASAPI runs in-process. For the rest, `Child::kill` (`TerminateProcess`). |
| `kill(pid, 0)` liveness | `OpenProcess` + `GetExitCodeProcess`, or keep the `Child` |
| `setsid` in `pre_exec` | `CommandExt::creation_flags(CREATE_NEW_PROCESS_GROUP \| DETACHED_PROCESS)` |
| `ps -u $UID`, `/proc/<pid>/stat` ppid | `sysinfo` crate, or Toolhelp32 `PROCESSENTRY32.th32ParentProcessID` |
| `xprop` window titles | `EnumWindows` + `GetWindowThreadProcessId` + `GetWindowTextW` |
| `xdg-open` | `ShellExecuteW` / `cmd /c start` |

**Service versus user session.** WASAPI system loopback works from session 0 and captures every session. Per-app
default-device policy and Application Loopback, however, are tied to the user's processes. Run the MVP as a
per-user autostart or tray process, and add a service mode later.

### 2.6 Network: multicast, mDNS, PTP

- **SAP / SIP / RTP multicast** (`socket2`): works on Windows. Windows `SO_REUSEADDR` already delivers
  multicast to *every* socket bound to the same port and group, so `set_reuse_port` is simply `cfg(unix)`.
  Interface enumeration needs `GetAdaptersAddresses` (`if-addrs`/`netdev` crate) in place of `if_nameindex`.
  The multicast counter (`/sys/class/net/*/statistics/multicast`) becomes `GetIfEntry2` →
  `MIB_IF_ROW2.InMulticastOctets`/`InNUcastPkts`, and `ip route get` becomes `GetBestInterfaceEx`.
- **Firewall:** the HTTP API binds `127.0.0.1` (no prompt). The multicast listeners bind `0.0.0.0`, which
  triggers the Windows Defender Firewall prompt for inbound UDP. The installer should add rules for UDP
  9875/5060/319/320 (`netsh advfirewall firewall add rule …` or `INetFwPolicy2`). The `ufw_enabled()` diagnosis
  becomes a firewall-profile query.
- **mDNS / DNS-SD** (`avahi-browse`): use the **`mdns-sd`** crate (v0.22, pure Rust, Linux/macOS/Windows,
  querier and responder, tested against Avahi and Bonjour) on **both** OSes. This removes the Avahi dependency
  on Linux too. The native fallback is `DnsServiceBrowse` (dnsapi, Windows 10+). Bonjour for Windows is not needed.
- **PTP** (UDP 319/320, passive listen + PTP over MQTT): the parser is OS-agnostic. Ports below 1024 are not
  privileged on Windows. A conflict occurs only if another stack has bound them: the W32Time PTP client
  provider (Windows 10 1809 / Server 2019, off by default), an Intel PTP driver stack, or Dante. Keep the
  existing "port busy" diagnosis text and add the Windows causes.
- **Dante/AES67:** Dante Virtual Soundcard on Windows exposes the Dante channels as ordinary WASAPI (WDM)
  endpoints, so they become normal capture sources.

### 2.7 Option zero: WSL2 (no port)

The daemon could run unchanged in WSL2, because WSLg ships a PulseAudio server at `/mnt/wslg/PulseServer`.
But that server only carries *Linux* audio out to Windows. Windows applications' audio never enters it, so the
meters would not see what the user is playing. Multicast needs WSL mirrored networking (Windows 11 22H2+).
WSL2 stays the path for **MXL SDK interop** only, not for metering Windows audio.

---

## 3. Proposed architecture

### 3.1 Shape

The key observation is that every audio path in the daemon already has the same shape: *spawn `parec`, read a
byte stream, compute RMS or write a ring*. The trait boundary therefore goes at "open a PCM `Read`". Meter
maths, VU ballistics, rings and JSON stay untouched.

```
src/platform/
  mod.rs        traits + `pub fn audio() -> &'static dyn AudioBackend` (cfg-selected)
  linux.rs      today's pactl/parec/pacat code moved here verbatim (no behaviour change)
  windows/
    mod.rs
    wasapi.rs   capture/loopback/app-loopback/render threads -> channel -> impl Read/Write
    devices.rs  IMMDeviceEnumerator, IPolicyConfig, IAudioSessionManager2
    proc.rs     terminate/alive/parent/window titles
    net.rs      GetAdaptersAddresses, GetIfEntry2, GetBestInterfaceEx, firewall
  shm.rs        shm_base(), preallocate(), try_lock(), alias_dir()  (small cfg split)
  discovery.rs  mdns-sd browse (both OSes; replaces avahi-browse)
```

```rust
pub struct PcmSpec { pub rate: u32, pub channels: u16, pub format: Fmt /* S16Le | F32Le */, pub latency_ms: u32 }
pub enum Tap { Device(String), Monitor(String), App { pid: u32, include_tree: bool } }

pub trait AudioBackend: Send + Sync {
    fn sources(&self) -> Vec<SourceInfo>;            // replaces `pactl list sources`
    fn sinks(&self) -> Vec<SinkInfo>;                // `pactl list short sinks`
    fn default_sink(&self) -> String;                // `pactl get-default-sink`
    fn set_default_sink(&self, id: &str) -> bool;    // IPolicyConfig on Windows
    fn streams(&self) -> Vec<StreamInfo>;            // sink-inputs / audio sessions (pid, app, sink)
    fn move_stream(&self, s: &StreamInfo, sink: &str) -> bool; // best-effort on Windows
    fn open_capture(&self, tap: &Tap, spec: PcmSpec) -> io::Result<Box<dyn PcmReader>>; // parec
    fn open_playback(&self, sink: &str, spec: PcmSpec, name: &str) -> io::Result<Box<dyn PcmWriter>>; // pacat
    fn buses(&self) -> &dyn BusManager;              // null sinks + loopbacks
    fn inspect(&self) -> serde_json::Value;          // pipeline layer ("alsa/pipewire" or "wasapi")
}
pub trait PcmReader: Read + Send { fn stop(&mut self); }   // Drop also stops (kills parec / releases IAudioClient)
pub trait BusManager { fn available(&self) -> bool; fn ensure(&self, names: &[(&str,&str)]) -> bool;
                       fn link(&self, src: &str, sink: &str) -> bool; fn teardown(&self); }
```

**Name mapping on Windows.** Render endpoints are listed as `<endpoint-id>.monitor` (loopback), with the
friendly name as `description`, and capture endpoints are listed by ID. The Linux names that XTM already uses
are aliases: `wbs_input_bus.monitor` and `wbs_meter_tap.monitor` resolve to the default render endpoint's
loopback, and `app:<pid>` resolves to Application Loopback. XTM's JSON stays identical.
`BusManager::available() == false` makes `/api/toggle` report the route as unavailable instead of silently
doing nothing, and the existing `routes_json()` carries that flag.

### 3.2 What moves behind `platform`

| Module | Moves | Stays shared |
|---|---|---|
| `tap.rs` | `run_cmd(pactl…)`, `parse_pactl_*` (to `linux.rs`), `source_meter_worker`/`meter_reader_loop` spawn, `spawn_pacat`, `apply_routes`/`reset_all` module juggling, `kill_pid`, stray-process sweep | state, `ROUTE_NAMES`, `chunk_rms`, VU filter, NMOS/MXL/AoIP source merge, POD-33/Bit Buddy synthesis (`wbs-generators`) |
| `testpoint.rs` | `parec` spawn, `pw_objects`/`resolve_pipewire_in`, `mcast_count`, `route_iface`, `ufw_enabled` | `TapRing`, `rtp_payload`, `decode_pcm`, `diagnosis` text, multicast RTP test points |
| `mxl.rs` | `parec` edge bridge, `posix_fallocate`, `flock`, symlink, `kill` | ring header/data layout, writer/reader maps, telemetry, test-signals catalog, waveform/slice/levels |
| `listen.rs` | `pactl list sink-inputs`/`move-sink-input`, `/proc` ancestry, `xprop` | app grouping, `wanted_sink` policy, mode state |
| `pipeline.rs` | `inspect_alsa`, `inspect_pipewire` → `audio().inspect()` | `inspect_aoip`, MXL fallback, report assembly |
| `nmos.rs` | `avahi-browse` → `discovery.rs` (mdns-sd) | IS-04 HTTP fetch, parsing, registries |
| `aoip.rs`, `aoip/py.rs` | `if_nameindex`, errno mapping, `set_reuse_port` cfg | SAP/SDP/PTP/SIP parsers, registry, MQTT client, listener loop |
| `main.rs`, `util.rs` | signals, `xdg-open`, `shm_base()` default | CLI, self-test, `run_cmd` helper |
| `http.rs`, `cbc.rs`, `setup.rs`, `lib.rs`, Generators | nothing | entire HTTP API, CBC proxy, setup page |

About 6,500 of the roughly 9,600 lines stay shared unchanged. The `linux.rs` move is mechanical and keeps
today's behaviour byte-for-byte, which matters because of the pending the workspace (`host/`, `apps/`, `utils/`) change and because existing
tests assert exact `pactl` argument lists.

### 3.3 New dependencies (Windows only, `[target.'cfg(windows)'.dependencies]`)

`wasapi = "0.27"` (or `windows` 0.6x directly), `com-policy-config = "0.6"`, `sysinfo`, `if-addrs`,
`ctrlc = { features = ["termination"] }`, `windows-service = "0.8"` (phase 4). The shared `mdns-sd = "0.22"`
replaces `avahi-browse` on both OSes. `libc` and `signal-hook` move to `[target.'cfg(unix)'.dependencies]`.

---

## 4. Phases and effort

Estimates assume one developer who knows the codebase. Days are working days.

| Phase | Scope | Effort |
|---|---|---|
| **0. Compile-clean** | Fix the 38 errors + `main.rs` signals with `cfg` gates and std replacements; `shm_base()` default; `cargo check --target x86_64-pc-windows-gnu` and a `windows-latest` CI job (`cargo test` for the pure modules: AoIP parsers, ring, generators) | **2-3 d** |
| **1. MVP: meters** | `platform` skeleton + `linux.rs` move; `windows/wasapi.rs` loopback/capture reader with silence-on-timeout; `sources()` from `IMMDeviceEnumerator`; aliases for `wbs_*_bus.monitor`; `/api/stream/meters`, `/api/status`, `/api/set_source` working; POD-33/Bit Buddy rendered to the default endpoint (seen by loopback); `BusManager` = unavailable; `inspect()` "wasapi" layer; zip with exe + README; firewall rule script | **7-10 d** |
| **2. Taps and discovery** | Application Loopback per-app sources (`app:<pid>`) with runtime probe; listen picker from `IAudioSessionManager2` + `sysinfo` + `EnumWindows`; test points on endpoints and apps; MXL engine on `%LOCALAPPDATA%` with `try_lock`; `mdns-sd` NMOS discovery (both OSes); AoIP multicast/PTP validated on a Windows NIC; `set_default_sink` via IPolicyConfig | **2-3 wk** |
| **3a. Buses: external cable** | `ExternalCable` `BusManager` for VB-CABLE/VAC endpoints; user-space loopback threads (capture → render) to emulate `module-loopback`; best-effort per-app routing via `IAudioPolicyConfigFactory` behind a version gate | **1-2 wk** |
| **3b. Buses: own driver** (optional) | Virtual-cable WaveRT driver from SYSVAD/simpleaudiosample (4 endpoints named like Linux), INF, attestation signing (EV cert + Partner Center), install/uninstall | **6-12 wk** + EV certificate + Partner Center onboarding |
| **4. Packaging** | Tray/autostart (per-user) and optional `windows-service`; MSI (`cargo-wix`); Authenticode-signed exe (SmartScreen); `install.ps1`/`uninstall.ps1` mirroring `install.sh` | **1 wk** |

**MVP total is about 2 weeks** (phases 0+1). A useful "Windows parity minus patchbay" is **5-6 weeks**
(phases 0-2 + 4). Full patchbay parity depends on 3a (a third-party cable) or 3b (our own signed driver).

### MVP acceptance

1. `ward-beck-audio-tab.exe` starts and serves `http://127.0.0.1:8467`, and xtm.wbsps.ca connects.
2. `/api/status` lists the Windows render endpoints (as `.monitor`) and capture endpoints as sources.
3. Playing audio in any app moves the XTM VU meters, and silence drops them to the floor within about 300 ms.
4. Selecting another endpoint in XTM re-targets the meter.
5. Route toggles report "unavailable on Windows" and do not error.
6. Ctrl-C and window close stop the meter threads and release the WASAPI clients.

### Risks

- The Application Loopback version floor is fuzzy: the docs say 20348, while OBS and win-capture-audio use
  2004+. Probe at runtime and hide `app:` sources when activation fails.
- `IPolicyConfig` and `IAudioPolicyConfigFactory` are undocumented. Gate them by build and fail soft.
- A kernel driver brings signing cost, HVCI compatibility and a support burden. Do not start 3b without a
  product decision.
- There is no native MXL SDK on Windows, so interop with SDK-based MXL tools needs WSL2 or Linux.

---

## Sources

- MXL project: https://github.com/dmf-mxl/mxl and build docs https://github.com/dmf-mxl/mxl/blob/main/docs/Building.md
- WASAPI loopback recording: https://learn.microsoft.com/en-us/windows/win32/coreaudio/loopback-recording
- AUDCLNT_STREAMFLAGS (LOOPBACK, AUTOCONVERTPCM): https://learn.microsoft.com/en-us/windows/win32/coreaudio/audclnt-streamflags-xxx-constants
- Application loopback sample: https://learn.microsoft.com/en-us/samples/microsoft/windows-classic-samples/applicationloopbackaudio-sample/
- AUDIOCLIENT_ACTIVATION_PARAMS: https://learn.microsoft.com/en-us/windows/win32/api/audioclientactivationparams/ns-audioclientactivationparams-audioclient_activation_params
- OBS Application Audio Capture (Windows 10 2004+): https://obsproject.com/kb/application-audio-capture-guide
- win-capture-audio (API backported to 2004): https://github.com/bozbez/win-capture-audio
- miniaudio discussion of the 20348 note: https://github.com/mackron/miniaudio/issues/484
- `wasapi` crate: https://docs.rs/wasapi/latest/wasapi/ and https://github.com/HEnquist/wasapi-rs/blob/master/examples/record_application.rs
- `cpal`: https://github.com/RustAudio/cpal
- VB-CABLE: https://vb-audio.com/Cable/ and licensing https://vb-audio.com/Services/licensing.htm
- Virtual Audio Cable: https://vac.muzychenko.net/en/ and https://vac.muzychenko.net/en/purchase.htm
- SYSVAD sample: https://github.com/microsoft/Windows-driver-samples/tree/main/audio/sysvad
- Open-source virtual audio driver precedents (search summary): https://awesome.ecosyste.ms/projects/github.com%2FJannesP%2FAudioMirror, https://github.com/kelvinwang000/virtual-audio-wire
- ACX objects: https://learn.microsoft.com/en-us/windows-hardware/drivers/audio/acx-summary-of-objects
- Attestation signing: https://learn.microsoft.com/en-us/windows-hardware/drivers/dashboard/code-signing-attestation
- Driver signing options: https://learn.microsoft.com/en-us/windows-hardware/drivers/dashboard/driver-signing-offerings
- AudioDeviceCmdlets: https://github.com/frgnca/AudioDeviceCmdlets
- IPolicyConfig is global (Microsoft Q&A): https://learn.microsoft.com/en-us/answers/questions/226483/set-active-audio-endpoint
- `com-policy-config` crate: https://lib.rs/crates/com-policy-config
- EarTrumpet `IAudioPolicyConfigFactory`: https://github.com/File-New-Project/EarTrumpet/blob/master/EarTrumpet/Interop/MMDeviceAPI/IAudioPolicyConfigFactory.cs
- Per-app routing on Windows 11 (SoundDeck): https://github.com/GeekyEggo/SoundDeck/issues/52
- Named shared memory: https://learn.microsoft.com/en-us/windows/win32/memory/creating-named-shared-memory
- `std::fs::File::lock` / `try_lock`: https://doc.rust-lang.org/std/fs/struct.File.html
- SO_REUSEADDR on Windows: https://learn.microsoft.com/en-us/windows/win32/winsock/using-so-reuseaddr-and-so-exclusiveaddruse
- MIB_IF_ROW2: https://learn.microsoft.com/en-us/windows/win32/api/netioapi/ns-netioapi-mib_if_row2
- `mdns-sd`: https://github.com/keepsimple1/mdns-sd and https://docs.rs/mdns-sd/latest/mdns_sd/
- DnsServiceBrowse: https://learn.microsoft.com/en-us/windows/win32/api/windns/nf-windns-dnsservicebrowse
- Windows PTP client (WSL test write-up): https://techcommunity.microsoft.com/blog/networkingblog/windows-subsystem-for-linux-for-testing-windows-10-ptp-client/389181
- Windows Time service: https://learn.microsoft.com/en-us/windows-server/networking/windows-time-service/windows-time-service-tools-and-settings
- `windows-service` crate: https://docs.rs/windows-service/latest/windows_service/
- WSL networking (mirrored mode, multicast): https://learn.microsoft.com/en-us/windows/wsl/networking
- WSLg architecture (PulseAudio server): https://devblogs.microsoft.com/commandline/wslg-architecture
- signal-hook `iterator` cfg: signal-hook 0.3.18 `host/src/main.rs` l.352 (`#[cfg(all(not(windows), feature = "iterator"))]`)
