<!-- Copyright (c) 2026 Ward-Beck Systems (Ward-Beck.com - Ward-Beck.ca). All rights reserved. -->
# WARD BECK TAP on Linux with Docker

The TAP is not a self-contained service. It drives the **host's** PipeWire/PulseAudio
server: it creates the WBS buses, loopbacks and meter taps there. The container is a
packaged binary plus the `pactl` / `pw-dump` / `avahi-browse` tools, and it shares the
host's network, audio sockets and `/dev/shm/mxl`.

Image: `ghcr.io/wardbecksystems/ward-beck-audio-tap` (linux/amd64 and linux/arm64, about 130 MB,
runs as UID 1000 by default).

## Quick start

```bash
git clone https://github.com/WardBeckSystems/Ward-Beck-Audio-Tap && cd Ward-Beck-Audio-Tap
export UID GID=$(id -g)           # run as the desktop user who owns the audio server
mkdir -p /dev/shm/mxl             # create it yourself, or Docker creates it owned by root
docker compose pull
docker compose up -d
xdg-open http://127.0.0.1:8467    # dashboard; /setup has the in-app guide
docker compose logs -f tap
```

To build from the checkout instead, run `git submodule update --init Generators`, then
`docker compose up -d --build`.

To stop it, run `docker compose down`. On SIGTERM the daemon tears down its buses and
restores your speakers.

Without compose, the same settings in one command:

```bash
docker run -d --name wbs-tap --network host --user "$(id -u):$(id -g)" --group-add audio \
  --security-opt apparmor=unconfined --device /dev/snd \
  -e XDG_RUNTIME_DIR=/run/user/$(id -u) -e PULSE_SERVER=unix:/run/user/$(id -u)/pulse/native \
  -e DBUS_SYSTEM_BUS_ADDRESS=unix:path=/run/dbus/system_bus_socket \
  -v /run/user/$(id -u)/pulse:/run/user/$(id -u)/pulse \
  -v /run/user/$(id -u)/pipewire-0:/run/user/$(id -u)/pipewire-0 \
  -v /dev/shm/mxl:/dev/shm/mxl -v /run/dbus/system_bus_socket:/run/dbus/system_bus_socket \
  ghcr.io/wardbecksystems/ward-beck-audio-tap:latest
```

## What the container needs, and why

| Setting | Why |
|---|---|
| `network_mode: host` | The dashboard binds **127.0.0.1:8467** by default. For a bridge network set `WBS_BIND=0.0.0.0` and publish `127.0.0.1:8467:8467` (HTTP only). Host networking is also needed to receive SAP (239.255.255.255:9875) and SIP (224.0.1.75:5060) multicast, for PTP, and to reach MQTT on 127.0.0.1:1883. |
| `user: ${UID}:${GID}` | PulseAudio and PipeWire accept clients from the socket owner only. The container user must be the host user who runs the audio session. |
| `group_add: audio` | Gives access to the ALSA devices in `/dev/snd`. |
| `/run/user/$UID/pulse` → same path | The PulseAudio socket (`pulse/native`), used by `pactl`, `parec` and `pacat`. These create the buses, loopbacks and meters. |
| `/run/user/$UID/pipewire-0` → same path | The native PipeWire socket, used by `pw-dump`, `pw-metadata` and `wpctl` for the pipeline view. |
| `/dev/shm/mxl` → same path | The EBU DMF / MXL shared-memory ring buffers, shared with the CBC/Radio-Canada MXL containers (`docker-compose.mxl-demo.yml`). |
| `/run/dbus/system_bus_socket` | Lets `avahi-browse` reach the host's avahi-daemon, for Dante, RAVENNA and NMOS mDNS discovery. |
| `devices: /dev/snd` | Lets `amixer` and `aplay` see the ALSA cards for the hardware layer of the pipeline view. |
| `security_opt: apparmor:unconfined` | Docker's default AppArmor profile blocks D-Bus, so Avahi fails without it. |

### Environment variables

| Variable | Default | Purpose |
|---|---|---|
| `XDG_RUNTIME_DIR` | `/run/user/1000` | Where the Pulse and PipeWire clients look for their sockets. |
| `PULSE_SERVER` | unset | Explicit Pulse socket, `unix:/run/user/$UID/pulse/native`. |
| `PIPEWIRE_RUNTIME_DIR` | unset | Directory that holds the PipeWire socket `pipewire-0`. |
| `DBUS_SYSTEM_BUS_ADDRESS` | unset | System bus, needed by Avahi. |
| `WBS_PORT` | `8467` | HTTP port. The healthcheck follows it. |
| `WBS_BIND` | `127.0.0.1` | Listen address. `0.0.0.0` only on a bridge network, published as `127.0.0.1:8467:8467`; never expose it to the LAN (the API has no auth). |
| `WBS_SHM_BASE` | `/dev/shm/mxl` | MXL root directory. |
| `WBS_DRY_RUN` | `0` | `1` logs every change to the audio graph and performs none of them. Use it for testing. |
| `WBS_NO_BROWSER` | `1` | Never open a browser from the container. |
| `WBS_CLIENT_TIMEOUT` | `12` | A tone or Bit Spitter stops when no page has checked in for this many seconds. `0` disables the watchdog. |
| `WBS_MXL_IP` | auto | The IP address that MXL advertises. |
| `AOIP_PTP_MODE` | `mqtt` | `mqtt` reads PTP passively from the APK broker. `sockets` binds UDP 319/320 instead. |
| `MQTT_HOST` / `MQTT_PORT` | `127.0.0.1` / `1883` | The broker used for PTP discovery. |
| `AOIP_PTP_TOPIC` | `APK.audio/System/Gui/Discovered/ptp` | The MQTT topic for PTP. |
| `AOIP_IFACE` | all | Joins SAP and SIP on a single interface, for example `44.44.44.10`. |

## Rootless Docker and Podman

- **Rootless Docker.** Container UID 0 is your host user, so use `user: "0:0"`, not `${UID}`.
  `network_mode: host` gives you RootlessKit's network namespace, not the real host's.
  As a result, the dashboard on 127.0.0.1 and multicast never reach the host. Rootless
  Docker is therefore only useful with the dashboard reached from inside the container,
  and it is not recommended.
- **Podman (rootless) works.** `--network host` is the real host namespace there:

  ```bash
  podman run -d --name wbs-tap --network host --userns keep-id --group-add keep-groups \
    --device /dev/snd --security-opt label=disable \
    -e XDG_RUNTIME_DIR=/run/user/$(id -u) -e PULSE_SERVER=unix:/run/user/$(id -u)/pulse/native \
    -e DBUS_SYSTEM_BUS_ADDRESS=unix:path=/run/dbus/system_bus_socket \
    -v /run/user/$(id -u)/pulse:/run/user/$(id -u)/pulse \
    -v /run/user/$(id -u)/pipewire-0:/run/user/$(id -u)/pipewire-0 \
    -v /dev/shm/mxl:/dev/shm/mxl -v /run/dbus/system_bus_socket:/run/dbus/system_bus_socket \
    ghcr.io/wardbecksystems/ward-beck-audio-tap:latest
  ```

  `--userns keep-id` maps your UID unchanged. `label=disable` is for SELinux hosts such
  as Fedora and RHEL. `podman compose` reads the same `docker-compose.yml`; add
  `userns_mode: keep-id` to it. Rootless containers cannot bind ports below 1024, so
  `AOIP_PTP_MODE=sockets` (UDP 319/320) also needs
  `sysctl net.ipv4.ip_unprivileged_port_start=319`.

## Troubleshooting

| Symptom | Fix |
|---|---|
| `avahi-browse: Failed to create client object: Access denied`, or no NMOS/Dante devices | AppArmor is blocking D-Bus. Keep `apparmor:unconfined`. Also check that the host runs avahi-daemon: `systemctl status avahi-daemon`. |
| `pactl: Connection refused` / `Access denied`, no WBS buses appear | The UID does not match. `export UID GID=$(id -g)` before `docker compose up`, and compare `docker exec wbs-tap id` with `stat -c %u /run/user/$UID/pulse/native`. |
| `/run/user/$UID/pulse` is an empty directory owned by root | The host runs PipeWire without its Pulse shim, so Docker created the missing mount source. Install and enable it: `sudo apt install pipewire-pulse` (or the distro's equivalent), then `systemctl --user enable --now pipewire-pulse.socket`. Remove the empty directory and recreate the container. |
| `/dev/shm/mxl` is owned by root, or MXL rings fail to open | Docker created the directory. Run `sudo chown $UID:$GID /dev/shm/mxl`, or create it before the first start. It does not survive a reboot, so recreate it at login. |
| No SAP or SIP streams | Multicast needs `network_mode: host`, and the right NIC must have joined the groups. Set `AOIP_IFACE` to that interface's IP. Check that the firewall passes UDP 9875 and 5060 and that the switch is not dropping IGMP. Rootless Docker cannot receive host multicast (see above). |
| `OSError: Address already in use` | Something already uses port 8467, for example a host install of the daemon (`systemctl --user status ward-beck-audio-tab`). Run only one, or set `WBS_PORT`. |
| Container shows `unhealthy` | `/api/status` did not answer within 4 s. Check `docker compose logs tap`. |
| `docker compose pull` returns `denied` | The image has not been published yet, or its GHCR package is still private. Run `docker compose up -d --build`. |

## CI publishing

`.github/workflows/docker.yml` builds on every pull request (amd64, nothing is pushed). On
`main` and on `v*` tags it pushes linux/amd64 and linux/arm64 to GHCR using `GITHUB_TOKEN`:

- `main` gets `:latest`, `:main` and `:sha-<short>`.
- A tag such as `v2.0.0` gets `:2.0.0`, `:2.0` and `:2`.

The arm64 build runs under QEMU, which is feasible because the crate has no C or TLS
dependencies. The Dockerfile builds natively for whichever platform it targets.

The first push creates a **private** GHCR package. To make it public, go to the
organization's Packages, open `ward-beck-audio-tap`, then Package settings → Change
visibility. Anonymous `docker compose pull` works only after that.

## What was tested (2026-11-23, Docker 29, Compose 2.40, legacy builder, amd64)

- **Build.** `docker build -t ghcr.io/wardbecksystems/ward-beck-audio-tap:test .` succeeded.
  The release build took 10.7 s and the image is 130 MB (the previous image was 163 MB).
  The image runs as `uid=1000(tap)`.
- **Smoke test, run without touching the host's audio.** The container used the default
  bridge network with no sockets, devices or host volumes mounted, and
  `WBS_DRY_RUN=1 WBS_PORT=18467 WBS_SHM_BASE=/dev/shm/wbs-smoke` (the container's own
  /dev/shm). The probes ran with `docker exec` because the daemon binds 127.0.0.1:
  - The daemon was ready after about 2 s.
  - `GET /api/status` returned HTTP 200 (44 KB of JSON) in 0.022 s.
  - `POST /api/heartbeat` returned HTTP 200 with `{"idle_limit_s": 12.0}`.
    `GET /api/heartbeat` returns 404 because the route is POST only.
  - `GET /` returned HTTP 200 (20 KB).
  - `--check` passed its self-test.
  - The logs showed every `pactl` call as `[DRY RUN]`, and the MXL domains were created
    under the scratch path.
  - The healthcheck reported `healthy`.
  - `docker stop` ran a clean teardown (exit 0, 0.14 s), and the container was then removed.
- **arm64.** `cargo check --release --locked --target aarch64-unknown-linux-gnu` passed in
  `rust:1-slim-bookworm`. The full multi-arch image is built only in CI, because this host
  has no buildx or QEMU.
- **Compose.** `docker compose config` validates. `docker compose pull` returns `denied`
  until the first CI push publishes the image.
- **Not tested here.** Neither compose file was run against live PipeWire, because that
  host drives real speakers.
