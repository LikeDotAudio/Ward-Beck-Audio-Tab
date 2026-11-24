# Ward-Beck Audio Tap — Linux

**Download:** [ward-beck-audio-tap-linux.tar.gz](https://github.com/WardBeckSystems/Ward-Beck-Audio-Tap/releases/latest/download/ward-beck-audio-tap-linux.tar.gz)
(x86_64 and ARM64 / Raspberry Pi 4–5 in one download) · newest build:
[nightly](https://github.com/WardBeckSystems/Ward-Beck-Audio-Tap/releases/download/nightly/ward-beck-audio-tap-linux.tar.gz)

Needs PipeWire (with `pipewire-pulse`) or PulseAudio.

```bash
tar xzf ward-beck-audio-tap-linux.tar.gz
cd ward-beck-audio-tap-linux
bash install.sh        # installs ~/.local/bin/ward-beck-audio-tab and a systemd user service
```

`install.sh` picks the binary for this CPU, starts the Tap at login, and asks once (sudo) for the
two capabilities PTP listening needs. From a git checkout it builds `core/` instead.

| | |
|---|---|
| Web app | https://ward-beck.ca · https://xtm.wbsps.ca |
| Tap API | http://127.0.0.1:8467 |
| Log | `~/.local/state/ward-beck-audio-tap/tap.log` · `journalctl --user -u ward-beck-audio-tab -f` |
| Report a problem | send the file from http://127.0.0.1:8467/api/diagnostics/bundle |
| Remove | `bash uninstall.sh` |

Also here: `run.sh` (build and run from a checkout) and `Dockerfile` / `docker-compose.yml` (the container).
