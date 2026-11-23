<!-- Copyright (c) 2026 Ward-Beck Systems (Ward-Beck.ca). All rights reserved. -->
# WARD BECK TAP on Windows: deployment and packaging

This document covers how a Windows user runs the TAP. There are two routes:

- **Today:** the Linux build inside WSL2, with or without Docker.
- **Later:** a native Windows app, once the port exists. Porting the code is covered elsewhere.
  This document covers installers, signing, services, the firewall and drivers.

Each recipe below comes from vendor documentation (Microsoft, Docker, Google). None of them has
been run on Windows hardware yet. The checks in [§1.7](#17-verification-checklist) are the
acceptance test. Anything not confirmed by a primary source is marked **verify**.

Years are written as `YYYY|n`, where n = YYYY − 1967.

---

## 0. What the TAP needs from the host

From `Dockerfile`, `docker-compose.yml` and `src/`:

| Need | Where in the code | Linux host | WSL2 |
|---|---|---|---|
| A PulseAudio-protocol server on which it can load `module-null-sink` and `module-loopback` | `src/tap.rs` (`WBS_SINKS`, `pactl`) | PipeWire-Pulse or PulseAudio | WSLg's PulseAudio at `/mnt/wslg/PulseServer` |
| PipeWire (`pw-dump`, `wpctl`) | `src/pipeline.rs`, `src/testpoint.rs` | yes | **no**: WSLg is PulseAudio only, so the PipeWire layer reports absent |
| `/dev/snd` (ALSA) | `docker-compose.yml` `devices:` | yes | **no**: WSL has no sound card. Remove the device from the compose file, or the container fails to start |
| `/dev/shm/mxl` | `src/util.rs` `shm_base()` (`WBS_SHM_BASE`) | tmpfs | tmpfs inside the WSL VM. It is shared only with processes in the same VM |
| HTTP `127.0.0.1:8467` | `src/http.rs` `bind()`. **Hard-coded to 127.0.0.1** | loopback | loopback, forwarded to Windows (see §1.4) |
| Multicast: SAP 239.255.255.255 / 224.2.127.254 :9875, SIP 224.0.1.75:5060, PTP 224.0.1.129:319/320, mDNS :5353 | `src/aoip.rs` | host network | **only in mirrored mode** (§1.5) |
| D-Bus system bus + Avahi | `docker-compose.yml` | yes | only with `systemd=true` in `/etc/wsl.conf` and `avahi-daemon` installed |
| MQTT 127.0.0.1:1883 (passive PTP) | `src/aoip.rs` | host | same VM, or Windows host in mirrored mode |

Consequence for deployment: by default `src/http.rs` binds only `127.0.0.1`, so a published
port (`-p 8467:8467`) reaches nothing. Set `WBS_BIND=0.0.0.0` in the container and publish it
as `-p 127.0.0.1:8467:8467` to use bridge networking (the HTTP API only; multicast discovery
still needs host or mirrored networking). The recipes below use `network_mode: host`.

---

## 1. Today: the Linux build in WSL2

### 1.1 WSLg's PulseAudio server

- WSLg runs a Weston compositor, XWayland and a **PulseAudio server** in a hidden "system
  distro". The user distro gets `DISPLAY`, `WAYLAND_DISPLAY` and `PULSE_SERVER` preset to
  sockets under `/mnt/wslg`
  ([WSLg README](https://github.com/microsoft/wslg),
  [WSLg architecture blog](https://devblogs.microsoft.com/commandline/wslg-architecture)).
- The socket is `unix:/mnt/wslg/PulseServer`. `XDG_RUNTIME_DIR` is `/mnt/wslg/runtime-dir`.
- Audio leaves the VM over **RDP**. WSLg loads `module-rdp-sink` (`RDPSink`) and
  `module-rdp-source` (`RDPSource`), and `mstsc.exe` plays the stream on the Windows default
  device ([WSLg architecture](https://devblogs.microsoft.com/commandline/wslg-architecture);
  module names shown in [this trace](https://zenn.dev/okuoku/scraps/4c5a4f631c0a20)).
- Containers reach the server by bind-mounting `/mnt/wslg` and passing `PULSE_SERVER`. This is
  Microsoft's own sample:
  `docker run -v /mnt/wslg:/mnt/wslg -e PULSE_SERVER=$PULSE_SERVER …`
  ([WSLg container samples](https://github.com/microsoft/wslg/blob/main/samples/container/Containers.md)).
  Docker Desktop users run the same command from the Ubuntu shell with WSL integration
  enabled, not from PowerShell ([PX4 WSLg container guide](https://docs.px4.io/main/en/simulation/gazebo_container_gui)).

**Does the audio reach the real Windows speakers?** Yes. Anything played to `RDPSink`
comes out of the Windows default playback device, and `RDPSource` is the Windows default
recording device. The TAP's "Physical Speaker Split" leg plays to `RDPSink`.

**Can Windows apps reach the TAP?** No, not directly. This is the most important limit. Spotify,
Edge or a DAW running *on Windows* plays to Windows' audio engine, not into WSLg. The TAP
can only hear:

1. Linux apps inside WSL, and the TAP's own generators and MXL flows; and
2. `RDPSource`, which is whatever Windows uses as its **default recording device**.

To meter Windows desktop audio today:

1. Install VB-CABLE (§3.6).
2. Set **CABLE Input** as the Windows default playback device.
3. Set **CABLE Output** as the default recording device.
4. In the Sound control panel, open **CABLE Output → Listen → Listen to this device** and pick
   the speakers, so you still hear it.

The audio then reaches the TAP as `RDPSource`. The TAP's microphone guard may flag
`RDPSource` as a mic input, which is the right call for a real microphone. Check how the
picker in `src/listen.rs` labels it.

### 1.2 Null sinks, loopbacks and latency

- WSLg's server is stock PulseAudio with two extra modules, so `pactl load-module
  module-null-sink` and `module-loopback` use the normal PulseAudio code path. **Verify** on
  a real machine: run `pactl load-module module-null-sink sink_name=wbs_probe`, then
  `pactl unload-module <id>`.
- The WSLg server lives in the system distro. It is **reset on every `wsl --shutdown`**, and
  WSLg can restart it on its own. The TAP's buses then disappear. The TAP already rebuilds
  them through `/api/restart`. A Windows deployment should watch for the WBS sinks
  vanishing and rebuild them.
- One user reported that pushing a GStreamer stream into a null sink crashed WSLg's
  PulseAudio, and that a WSL restart was needed afterwards
  ([diyAudio](https://www.diyaudio.com/community/goto/post?id=7201258)). Treat the WSLg
  server as fragile.
- **Latency:** no vendor publishes figures. The path is:

  PulseAudio → RDP audio virtual channel → `mstsc` → Windows audio engine

  That adds RDP buffering on top of PulseAudio's own. Plan for tens to low hundreds of
  milliseconds on the speaker leg. Sound becomes sluggish after idling or lock/unlock
  ([wslg#1026](https://github.com/microsoft/wslg/issues/1026)), and stutters while Hyper-V is
  busy ([Microsoft Q&A](https://learn.microsoft.com/en-us/answers/questions/466983/sound-stutters-lags-pops-(on-host-machine)-wheneve)).
  The **meters are not affected**. They read the null-sink monitors inside the VM and the
  MXL rings, so XTM metering stays tight. Only what you hear on the Windows speakers lags.
  Measure it with `pactl list sinks | grep -i latency` on `RDPSink`.

### 1.3 `network_mode: host` on Docker Desktop

- Docker Desktop 4.34 and later has **opt-in** host networking: Settings → Resources →
  Network → *Enable host networking*. You must be signed in, and it does not work with
  Enhanced Container Isolation.
- It "works on layer 4": TCP and UDP ports are forwarded in both directions, but
  "network protocols that operate below TCP or UDP are not supported". Processes "cannot
  bind to the IP addresses of the host".
- It is Linux containers only
  ([Docker host network driver](https://docs.docker.com/engine/network/drivers/host/)).
- It is **not** real host networking. The container shares the Docker Desktop VM's
  namespace, and Docker proxies ports to Windows
  ([Docker forum](https://forums.docker.com/t/option-network-mode-host-in-docker-compose-file-not-working-as-expected/51682/14)).
- **Multicast does not pass.** IGMP joins and group traffic are below the port proxy. Docker
  users report no multicast or broadcast even with `--net=host`
  ([forum](https://forums.docker.com/t/multicast-not-working-even-with-net-host/134035),
  [ROS 2 on Windows](https://answers.ros.org/question/416676/)).
- Docker Desktop does **not** inherit WSL mirrored mode for its containers
  ([forum](https://forums.docker.com/t/host-networking-not-working-on-docker-desktop-in-wsl2-with-mirrored-mode/147994)).
  Docker's 4.26 notes on mirrored mode concern the WSL integration, not container multicast
  ([Docker blog](https://dev.to/docker/wsl-mirrored-mode-networking-in-docker-desktop-4260-4b86)).
- **Verify** whether a socket bound to `127.0.0.1` inside a host-mode container is forwarded
  to Windows `127.0.0.1:8467`. The docs promise "host processes can reach container
  services" but say nothing about loopback-only listeners.

**Verdict:** Docker Desktop is fine for the patchbay, VU bridge and MXL. It cannot do
SAP/SIP/PTP/mDNS discovery. For AoIP, use **Docker Engine inside the Ubuntu distro** (§1.6
option B) or the binary in WSL (option A), both with mirrored networking.

### 1.4 Reaching `localhost:8467` from a Windows browser

| WSL mode | Windows browser → `http://127.0.0.1:8467` | Notes |
|---|---|---|
| NAT (default) | Yes, through `localhostForwarding` (on by default). Under NAT, Edge and Chrome on Windows reach Linux servers at `localhost` ([MS Learn](https://learn.microsoft.com/en-us/windows/wsl/networking)). | IPv6 `::1` is not forwarded. Use `127.0.0.1`, not `localhost`. |
| Mirrored | Yes. Windows and WSL reach each other on `127.0.0.1`. "IPv6 localhost address `::1` is not supported" ([MS Learn](https://learn.microsoft.com/en-us/windows/wsl/networking)). | Some users lost Windows → container `localhost` after switching to mirrored mode ([WSL#10731](https://github.com/microsoft/WSL/issues/10731)). |
| Docker Desktop host mode | **Verify** (§1.3) | |

Because only IPv4 loopback is bridged, **XTM should call `http://127.0.0.1:8467`, not
`http://localhost:8467`**. On Windows, `localhost` may resolve to `::1` first. Chromium
usually falls back to IPv4, but the fallback costs time, and the first request can fail.

### 1.5 Multicast: NAT versus mirrored

- **NAT:** the VM sits behind a Hyper-V virtual switch on a private `172.x` subnet. Multicast
  from the LAN is not routed in, so SAP, SIP, PTP and mDNS from Dante, RAVENNA or AES67
  devices never arrive.
- **Mirrored:** Windows' interfaces are mirrored into Linux. Microsoft lists "Multicast
  support" and "Connect to WSL directly from your local area network" among its benefits. It
  needs **Windows 11 22H2 or later** ([MS Learn](https://learn.microsoft.com/en-us/windows/wsl/networking)).
- With mirrored mode, the **Hyper-V firewall** applies (on by default since WSL 2.0.9).
  Inbound UDP to the WSL VM needs a Hyper-V firewall rule. Ordinary Windows Defender
  Firewall rules are not enough ([MS Learn](https://learn.microsoft.com/en-us/windows/wsl/networking),
  [Hyper-V firewall](https://learn.microsoft.com/en-us/windows/security/operating-system-security/network-security/windows-firewall/hyper-v-firewall)).
- Windows' own mDNS responder already listens on UDP 5353. In mirrored mode, Avahi in WSL
  and Windows share the port. Expect duplicate or odd mDNS answers. **Verify** that
  Dante/RAVENNA discovery works.
- PTP in `AOIP_PTP_MODE=sockets` needs ports 319/320 (privileged). The default `mqtt` mode
  avoids that. Mirrored-mode timestamps go through Hyper-V, so treat PTP as **monitoring
  only**. Do not use it as a clock reference.

### 1.6 Recipes

#### Windows side (once, admin PowerShell)

```powershell
# WSL2 + Ubuntu (Windows 11 22H2+ for mirrored mode)
wsl --install -d Ubuntu-24.04
wsl --update

# %UserProfile%\.wslconfig
@"
[wsl2]
networkingMode=mirrored
# firewall=true is the default; keep it and add the rules below
"@ | Set-Content -Encoding ascii "$env:USERPROFILE\.wslconfig"

# Hyper-V firewall: allow inbound AoIP multicast into WSL (VMCreatorId = WSL)
New-NetFirewallHyperVRule -Name "WBS-TAP-AoIP" -DisplayName "WARD BECK TAP AoIP (SAP/SIP/PTP/mDNS)" `
  -Direction Inbound -VMCreatorId '{40E0AC32-46A5-438A-A0B2-2B479E8F2E90}' `
  -Protocol UDP -LocalPorts 9875,5060,319,320,5353
# Only needed if something on the LAN should reach the dashboard; it binds 127.0.0.1, so normally not:
# New-NetFirewallHyperVRule -Name "WBS-TAP-HTTP" ... -Protocol TCP -LocalPorts 8467

wsl --shutdown
```

#### Inside Ubuntu (once)

```bash
# systemd for D-Bus/Avahi (and Docker Engine, if used)
sudo tee /etc/wsl.conf >/dev/null <<'EOF'
[boot]
systemd=true
EOF
# then from Windows: wsl --shutdown, and reopen Ubuntu

sudo apt-get update
sudo apt-get install -y pulseaudio-utils alsa-utils avahi-daemon avahi-utils iproute2 curl build-essential
echo $PULSE_SERVER          # expect unix:/mnt/wslg/PulseServer
pactl info | grep -E 'Server (Name|String)'
pactl list short sinks      # expect RDPSink
```

#### Option A: run the binary in WSL (no Docker)

Simplest, and multicast works in mirrored mode:

```bash
git clone --recursive https://github.com/WardBeckSystems/Ward-Beck-Audio-Tap.git
cd Ward-Beck-Audio-Tap
curl https://sh.rustup.rs -sSf | sh -s -- -y && . "$HOME/.cargo/env"
mkdir -p /dev/shm/mxl
PULSE_SERVER=unix:/mnt/wslg/PulseServer ./run.sh --no-browser
```

Then open **https://xtm.wbsps.ca** in Edge or Chrome on Windows, and allow the local network
prompt (§4). Telemetry is at `http://127.0.0.1:8467` from Windows.

`install.sh` installs a systemd **user** service. User services do not inherit WSLg's
variables, so add these lines to the unit, or to a `~/.config/systemd/user/…d/` drop-in:

```ini
[Service]
Environment=PULSE_SERVER=unix:/mnt/wslg/PulseServer
Environment=WBS_NO_BROWSER=1
```

WSL stops an idle distro when no Windows-side `wsl.exe` session is attached. To keep the
TAP running, add a Windows Task Scheduler task at logon that runs
`wsl.exe -d Ubuntu-24.04 --exec /bin/sh -lc "systemctl --user start ward-beck-audio-tab; sleep infinity"`.
**Verify** the idle-shutdown behaviour against the WSL version in use.

#### Option B: Docker Engine inside Ubuntu (recommended container route)

Install `docker-ce` in the distro ([Docker Engine on Ubuntu](https://docs.docker.com/engine/install/ubuntu/)),
not Docker Desktop. `network_mode: host` is then the WSL VM's network, which is mirrored
from Windows. Use this override next to the Linux compose file. It replaces the
Linux-desktop sockets with WSLg's, and drops `/dev/snd`:

```yaml
# docker-compose.windows.yml: WSL2 (WSLg) override for docker-compose.yml
#   docker compose -f docker-compose.yml -f docker-compose.windows.yml up -d
# Needs Compose 2.24+ for !override / !reset.
services:
  tap:
    image: ghcr.io/wardbecksystems/ward-beck-audio-tap:latest
    network_mode: host                 # WSL VM network; mirrored mode carries multicast
    user: "1000:1000"                  # WSLg's sockets belong to the first WSL user (uid 1000)
    environment:
      PULSE_SERVER: unix:/mnt/wslg/PulseServer
      XDG_RUNTIME_DIR: /mnt/wslg/runtime-dir
      PIPEWIRE_RUNTIME_DIR: !reset null   # no PipeWire under WSLg
      WBS_NO_BROWSER: "1"
      MQTT_HOST: 127.0.0.1
      AOIP_PTP_MODE: mqtt
    volumes: !override
      - /mnt/wslg:/mnt/wslg            # WSLg PulseAudio socket + runtime dir
      - /dev/shm/mxl:/dev/shm/mxl
      - /run/dbus/system_bus_socket:/run/dbus/system_bus_socket   # needs systemd=true + avahi-daemon
    devices: !reset []                 # WSL has no /dev/snd
```

The merge was checked with `docker compose config` (Compose 2.40). The result has no
`/dev/snd`, no `/run/user/…` Pulse or PipeWire mounts and no `PIPEWIRE_RUNTIME_DIR`, and it
keeps `network_mode: host`. Before the first `up`, run `mkdir -p /dev/shm/mxl`. WSL clears `/dev/shm` when the VM
restarts.

#### Option C: Docker Desktop

The same override works, with these limits:

- Run it from the Ubuntu shell, with WSL integration on for that distro.
- Turn on *Enable host networking* in Docker Desktop.
- **No AoIP multicast** (§1.3).
- `/dev/shm/mxl` is the Docker Desktop VM's, not the Ubuntu distro's.
- **Verify** that the `/mnt/wslg` Unix socket bind-mount works across the Docker Desktop VM
  boundary. Microsoft's sample uses Docker inside the distro.

### 1.7 Verification checklist

Run these on a Windows 11 machine. Each step needs a "yes" before the Docker/WSL route is
declared supported:

1. `pactl list short sinks` inside the distro or container lists `RDPSink`. After the TAP
   starts, it also lists `wbs_input_bus`, `wbs_meter_tap`, `wbs_insert_bus` and
   `wbs_test_point`.
2. The TAP test tone is heard on the Windows speakers, and the delay is noted.
3. From PowerShell, `curl.exe -s http://127.0.0.1:8467/api/status` returns JSON. Check this
   in both NAT and mirrored mode.
4. XTM at https://xtm.wbsps.ca shows the local network prompt once, then live meters.
5. In mirrored mode, `/api/aoip/multicast` shows SAP announcements from a LAN AES67 source.
6. After `wsl --shutdown` and a restart, the TAP recovers its buses.

---

## 2. Why Windows containers are not an option

- Windows containers expose only GPIO, I²C, COM, SPI and DirectX GPU devices, through
  `--device class/{GUID}`. **There is no audio device class.** Device sharing is
  process-isolation only. Hyper-V-isolated containers get none
  ([Devices in containers on Windows](https://learn.microsoft.com/en-us/virtualization/windowscontainers/deploy-containers/hardware-devices-in-containers)).
- Windows containers are based on Server Core or Nano Server, and run in their own
  session. They have no Windows Audio service (`Audiosrv`), no audio endpoint builder and no
  user session, so WASAPI has nothing to open. The user's audio engine lives in the
  interactive session on the host. The container cannot reach it.
- Host networking does not exist for Windows containers
  ([Docker](https://docs.docker.com/engine/network/drivers/host/)).
- The TAP is a PulseAudio/PipeWire client. A Windows container gains nothing that the
  native app (§3) would not do better.

---

## 3. Native Windows packaging (after the port)

Assumed shape of the port: one `ward-beck-audio-tab.exe`. It does WASAPI loopback and
capture for the meters, HTTP on `127.0.0.1:8467`, MXL rings in a named file mapping in
place of `/dev/shm`, and the same AoIP sockets.

### 3.1 Tray app or Windows service

| | Per-user tray app | Windows service |
|---|---|---|
| Session | Interactive user session | **Session 0**, with no desktop and no user audio endpoints of its own |
| Audio | Full WASAPI: render, capture, per-process loopback (`AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK`, Windows 10 2004+). The consent prompt for `ActivateAudioInterfaceAsync` needs a UI thread ([MS Learn](https://learn.microsoft.com/en-us/windows/win32/api/mmdeviceapi/nf-mmdeviceapi-activateaudiointerfaceasync)) | Endpoint loopback of the system mix is documented as possible from session 0 ([Loopback recording](https://learn.microsoft.com/en-us/windows/win32/coreaudio/loopback-recording)). Per-app loopback, consent prompts, the default-device follow and routing the user's playback are not. Windows 11 mutes devices with no active user session ([Tech Community](https://techcommunity.microsoft.com/discussions/windows11/windows-11-mutes-all-audio-devices-if-there-is-no-active-user-session/3860131/replies/4532787)) |
| Install | No admin needed for the app itself | Admin, plus SCM registration |
| Runtime prompts (firewall, driver) | Can show them, with a UAC elevation on demand | Cannot show UI |
| Lifetime | Logon to logoff, which matches what the TAP does now | Always on |

**Recommendation: a per-user tray app.** It matches the systemd *user* service on Linux.
Later, an optional small service could own only the privileged parts, such as binding
PTP 319/320 and relaying multicast, and talk to the tray app over a named pipe.

### 3.2 Autostart

Pick one of these:

- `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` value `WARD BECK TAP =
  "…\ward-beck-audio-tab.exe" --tray`. This is per user, needs no admin, and the user can
  switch it off in *Settings → Apps → Startup*.
- In MSIX, the `uap5:StartupTask` extension. The user approves it in the same Startup page
  ([StartupTask](https://learn.microsoft.com/en-us/uwp/schemas/appxpackage/uapmanifestschema/element-uap5-startuptask)).
- A Task Scheduler task "At log on". Use this only if a delayed start or a restart-on-failure
  is needed.

### 3.3 Windows Firewall

- **Inbound TCP 8467:** none needed. The listener is loopback-only, and loopback traffic is
  not filtered.
- **Inbound UDP multicast** (9875 SAP, 5060 SIP, 319/320 PTP, 5353 mDNS) needs an allow
  rule. Scope it to the program and to the *Private* (and *Domain*) profiles. Never open it
  on *Public*. Without a rule, the first `bind()` raises the "Windows Defender Firewall has
  blocked some features" prompt, which needs admin.
- **Installer route:** WiX `FirewallException` from `WixToolset.Firewall.wixext` supports
  `Protocol="udp"`, `Port`, `Program` and `Profile`
  ([schema](https://wixtoolset.org/docs/schema/firewall/firewallexception/),
  [NuGet](https://www.nuget.org/packages/WixToolset.Firewall.wixext)). This needs a
  per-machine MSI.
- **Runtime route:** for per-user, Scoop and portable installs, the app shows an in-app
  prompt that elevates once and runs:

  ```powershell
  New-NetFirewallRule -DisplayName "WARD BECK TAP AoIP" -Direction Inbound -Protocol UDP `
    -LocalPort 9875,5060,319,320,5353 -Program "$env:LOCALAPPDATA\Programs\WardBeckTap\ward-beck-audio-tab.exe" `
    -Profile Private,Domain -Action Allow
  ```

- **MSIX route:** full-trust packages can declare `desktop2:FirewallRules` in the manifest
  ([schema](https://learn.microsoft.com/en-us/uwp/schemas/appxpackage/uapmanifestschema/element-desktop2-firewallrules)).

### 3.4 Installer formats

| Format | Tooling for a Rust exe | Per-user | Firewall | Services | Notes |
|---|---|---|---|---|---|
| **MSI (WiX)** | [`cargo-wix`](https://github.com/volks73/cargo-wix) 0.3.x is built around the WiX **v3** `candle`/`light` tool chain. For WiX v4–v6, write a `.wixproj` with `WixToolset.Sdk` and build it with `dotnet build`, or use [`cargo-packager`](https://docs.rs/crate/cargo-packager/latest) | Yes (`Scope="perUser"`) but then no firewall rule | `Firewall.wixext` | `ServiceInstall` | The enterprise standard: GPO/Intune, silent `msiexec /qn`. WiX v6+ binaries carry the Open Source Maintenance Fee for revenue-earning users ([WiX](https://wixtoolset.org/), [OSMF](https://opensourcemaintenancefee.org/)). **Verify** the fee terms |
| **MSIX** | `makeappx` / MSIX Packaging Tool, or a `Package.appxmanifest` | Per-user install | `desktop2:FirewallRules` | Possible since Windows 10 2004, but needs admin to install and a restricted capability ([MS Learn](https://learn.microsoft.com/en-us/windows/msix/packaging-tool/convert-an-installer-with-services)) | Clean uninstall and auto-update. The Store re-signs it for free and **no SmartScreen warning** appears ([MS Learn](https://learn.microsoft.com/en-us/windows/apps/package-and-deploy/code-signing-options)). **Cannot carry a kernel audio driver** |
| **Inno Setup** | `.iss` script and `ISCC` | Yes (`PrivilegesRequired=lowest`, with optional elevation) | `[Run]` netsh/PowerShell | `sc.exe` | Simplest EXE installer. Can chain the VB-CABLE setup |
| **winget** | YAML manifest in [`microsoft/winget-pkgs`](https://github.com/microsoft/winget-pkgs), via [`wingetcreate`](https://github.com/microsoft/winget-create) | n/a | n/a | n/a | Points at the MSI, MSIX or Inno installer on GitHub Releases. `InstallerType: wix`/`msi`/`msix`/`inno` ([manifest docs](https://learn.microsoft.com/en-us/windows/package-manager/package/manifest), [submit](https://learn.microsoft.com/en-us/windows/package-manager/package/repository)). Update it from CI with `wingetcreate update --submit` |
| **Scoop** | JSON manifest in our own bucket repo | Yes, portable zip, no admin | Runtime prompt | No | Suits power users. `checkver`/`autoupdate` read GitHub Releases ([Scoop wiki](https://github.com/ScoopInstaller/Scoop/wiki/App-Manifests)) |
| **Chocolatey** | `.nuspec` + `chocolateyInstall.ps1` wrapping the MSI | n/a | Via the MSI | Via the MSI | Community moderation takes days. Worth it only if customers ask. A [`vb-cable`](https://community.chocolatey.org/packages/vb-cable) package already exists to depend on |

### 3.5 Code signing and SmartScreen

- **Since 2024|57, EV certificates no longer bypass SmartScreen.** Microsoft's Trusted Root
  Program removed the EV code-signing OIDs, and "all Code Signing certificates will be
  treated equally" ([Microsoft Q&A](https://learn.microsoft.com/en-us/answers/questions/1846647/program-requirements-microsoft-trusted-root-progra),
  [MS Learn](https://learn.microsoft.com/en-us/windows/apps/package-and-deploy/code-signing-options),
  [DigiCert KB](https://knowledge.digicert.com/alerts/ev-signed-application-showing-microsoft-defender-smartscreen-warnings)).
- **Azure Artifact Signing** (formerly Trusted Signing) costs about US$9.99 a month for
  5,000 signatures, with no hardware token, and integrates with GitHub Actions.
  Organizations in the **USA, Canada**, EU and UK are eligible, so Ward-Beck Systems
  (Canada) qualifies. It "does **not** provide instant SmartScreen trust", but signing every
  release with the same identity builds publisher reputation
  ([MS Learn](https://learn.microsoft.com/en-us/windows/apps/package-and-deploy/code-signing-options),
  [pricing](https://azure.microsoft.com/pricing/details/artifact-signing/)).
- **OV certificate**, US$150–300 a year on an HSM or token: the same SmartScreen behaviour.
  Use it only if Artifact Signing is unavailable.
- **Store-distributed MSIX:** free re-signing and no SmartScreen warning. This is the
  fastest way past "Windows protected your PC" for consumers.
- Reputation is tracked per publisher and per file hash
  ([SmartScreen reputation](https://learn.microsoft.com/en-us/windows/apps/package-and-deploy/smartscreen-reputation)).
  Ship from one signing identity, keep the release cadence steady, and sign the exe, the
  MSI and any bundled installers.
- **EV still matters for one thing:** a Ward-Beck kernel audio driver (§3.6) must be
  attestation-signed through the Hardware Dev Center, which requires an EV certificate
  ([driver signing requirements](https://learn.microsoft.com/en-us/windows-hardware/drivers/dashboard/code-signing-reqs)).

### 3.6 Virtual audio drivers

Windows has no built-in null sink or loopback module. The port needs fewer of them than
Linux does:

- **Metering the desktop mix** needs no driver. WASAPI endpoint loopback on the default
  render device, or per-process loopback for "what should the meters hear", replaces the
  `wbs_input_bus` monitor.
- **Inserting into the path** (a real `wbs_input_bus` that apps play into) needs a virtual
  render endpoint. The options:

| Driver | Licence for bundling | Notes |
|---|---|---|
| **VB-CABLE** (VB-Audio) | You may distribute the package **AS IS** with your app, freeware or commercial, and a silent install is allowed, "provided that our donationware model remains applicable". The installer or docs must credit VB-CABLE (vb-cable.com) and say it is donationware. For corporate deployments where users cannot see or pay for it, the distributor buys licences (from about 100, at volume prices). **VB-CABLE A+B / C+D cannot be bundled** ([VB-Audio licensing](https://vb-audio.com/Services/licensing.htm)) | One cable. The usual choice. Chain its setup from Inno or Burn. It does not fit inside MSIX |
| Virtual Audio Cable (Muzychenko) | Commercial. Ask for a redistribution licence ([VAC](https://software.muzychenko.net/eng/vac.htm)) | Many cables, low latency |
| Own driver based on Microsoft's [SysVAD sample](https://github.com/microsoft/Windows-driver-samples/tree/main/audio/sysvad) | Ours | Needs an EV certificate, Hardware Dev Center attestation signing and ongoing driver maintenance. Only worth it at volume |

Recommendation: ship without a driver. Detect VB-CABLE, and if it is missing, offer to
install it from an in-app prompt with the attribution text. Revisit a Ward-Beck driver only
if corporate volume makes per-seat VB-CABLE licences costly.

---

## 4. Browser side: https://xtm.wbsps.ca → http://127.0.0.1:8467

- **Mixed content is not the blocker.** `http://127.0.0.1` and `http://localhost` are
  "potentially trustworthy" origins, so an HTTPS page may fetch them.
- **Chrome Local Network Access (LNA):**
  - LNA replaced Private Network Access. The permission prompt shipped in **Chrome 142**.
  - A public site making a request to loopback needs the user's permission.
  - In **Chrome 145** the permission was split into `local-network` (LAN) and
    `loopback-network` (`localhost`, `127.0.0.1`).
  - Restrictions on WebSockets follow in Chrome 147.
  - Sources: [Chrome blog](https://developer.chrome.com/blog/local-network-access),
    [LNA guidance](https://developer.chrome.com/docs/modern-web-guidance/guides/security/local-network-access),
    [blink-dev](https://groups.google.com/a/chromium.org/g/blink-dev/c/lRnFRIfzDMU).
  - The prompt reads roughly "Look for and connect to any device on your local network". It
    is asked once per site.
- **Edge** is Chromium and shows the same prompt
  ([Microsoft Support](https://support.microsoft.com/en-US/edge/control-a-website-s-access-to-the-local-network-in-microsoft-edge)).
  Managed fleets can pre-grant XTM:
  - [`LocalNetworkAccessAllowedForUrls`](https://learn.microsoft.com/en-us/deployedge/microsoft-edge-policies/LocalNetworkAccessAllowedForUrls)
    (Edge 140+)
  - [`LoopbackNetworkAllowedForUrls`](https://learn.microsoft.com/en-my/deployEdge/microsoft-edge-policies/loopbacknetworkallowedforurls)
    (Edge 146+, covers `127.0.0.1`, `::1`, `localhost`)

  `LocalNetworkAccessRestrictionsTemporaryOptOut` is temporary and should not be relied on.
  Chrome has matching enterprise policies.
- **What XTM should do:**
  - Fire the first TAP request from a user gesture, such as a "Connect to TAP" button, not on
    page load. Show an explanation while the permission is in `prompt` state. Service
    workers cannot trigger the prompt.
  - Use `fetch(url, { targetAddressSpace: 'loopback' })` where supported.
  - Use `http://127.0.0.1:8467` (§1.4).
  - If XTM is ever embedded in an iframe, the parent must delegate
    `allow="local-network; loopback-network"`.
- **Server side:** `src/http.rs` already sends `Access-Control-Allow-Origin: *` and
  `Access-Control-Allow-Private-Network: true`. LNA relies on the permission plus ordinary
  CORS, and says "DO NOT rely on deprecated Private Network Access (PNA) preflight headers".
  The PNA header is harmless and can stay for older browsers. No change is needed for
  Windows.
- WSL's localhost forwarding makes the browser see a plain loopback server. The Windows
  browser behaves exactly as it would with the native port.

---

## 5. Recommended path

### Comparison

| Route | Patchbay / meters | Hear on Windows speakers | Windows app audio in | AoIP multicast | Latency (speaker leg) | Setup effort (user) | Our effort |
|---|---|---|---|---|---|---|---|
| WSL2 binary, NAT | ✅ | ✅ via RDPSink | Only via RDPSource and VB-CABLE | ❌ | RDP-buffered, **measure** | Medium | Docs only |
| **WSL2 binary, mirrored** | ✅ | ✅ | Only via RDPSource and VB-CABLE | ✅ (Hyper-V firewall rule) | same | Medium | Docs + one script |
| Docker Engine in WSL, mirrored | ✅ | ✅ | same | ✅ | same | Medium-high | `docker-compose.windows.yml` |
| Docker Desktop (host networking) | ✅ (**verify** loopback forward) | ✅ (**verify** socket mount) | same | ❌ layer 4 only | same | Medium | same file |
| Windows container | ❌ no audio devices | ❌ | ❌ | ❌ | — | — | — |
| **Native tray app (MSI/MSIX + winget)** | ✅ WASAPI | ✅ native | ✅ loopback, no driver | ✅ firewall rule | Native, about 10–20 ms shared mode | Low | Port + packaging |

### Phases

| Phase | Deliverable | Effort |
|---|---|---|
| **1. Now** | This guide. Add `docker-compose.windows.yml` (§1.6 B). Add a `wbs-tap-wsl-setup.ps1` that writes `.wslconfig`, adds the Hyper-V firewall rule and installs Ubuntu. Run the §1.7 checklist on one Windows 11 machine and record the speaker-leg latency. Link it from `/setup`. | 2–3 days |
| **2. XTM** | Add a "Connect to TAP" gesture and an LNA permission explainer. Use `127.0.0.1` and `targetAddressSpace: 'loopback'`. Publish the Edge/Chrome policy snippet for IT. | 1–2 days |
| **3. Native MVP** (after the port) | Unsigned portable zip with a tray icon, HKCU Run autostart and a runtime firewall prompt. Scoop bucket. | 3–5 days of packaging on top of the port |
| **4. Signed release** | Azure Artifact Signing in GitHub Actions. Per-machine WiX v4+ MSI with `Firewall.wixext` and an autostart option. winget manifest that `wingetcreate update --submit` updates on each release. | 1–2 weeks (including a few days for identity validation) |
| **5. Consumer reach** | Store MSIX (free signing, no SmartScreen warning) with `StartupTask` and `desktop2:FirewallRules`. VB-CABLE detect-and-offer, with attribution. | 1–2 weeks |
| **6. Optional** | Chocolatey package. A privileged helper service for PTP 319/320. Revisit a Ward-Beck SysVAD-based driver only at volume. | as needed |

**In short:** for Windows users today, run the Linux binary in WSL2 with **mirrored
networking**, playing through WSLg's PulseAudio (`PULSE_SERVER=unix:/mnt/wslg/PulseServer`),
and accept RDP latency on the speaker leg. Use Docker Engine inside the distro if a
container is wanted. Do not use Docker Desktop if AoIP discovery matters.

Long term, the answer is a native per-user tray app:

- Sign it with Azure Artifact Signing.
- Ship it as a WiX MSI through winget, and as an MSIX through the Store.
- Meter with WASAPI loopback, so no driver is needed.
- Offer VB-CABLE only for the insert path.
