# Ward-Beck Audio Tap — macOS (preview)

**Download:** [ward-beck-audio-tap-macos.zip](https://github.com/WardBeckSystems/Ward-Beck-Audio-Tap/releases/latest/download/ward-beck-audio-tap-macos.zip)
(macOS 14.2+, Apple silicon and Intel) · newest build:
[nightly](https://github.com/WardBeckSystems/Ward-Beck-Audio-Tap/releases/download/nightly/ward-beck-audio-tap-macos.zip)

```bash
unzip ward-beck-audio-tap-macos.zip
cd ward-beck-audio-tap-macos
bash install.sh        # installs for this user and starts at login (LaunchAgent)
```

**Preview:** the core runs (web app API, MXL rings, generators, AES67, SAP/NMOS/PTP/NTP, logging);
capturing system audio through a Core Audio process tap is still being written. When it lands,
macOS will ask once for permission to record system audio.

| | |
|---|---|
| Web app | https://ward-beck.ca · https://xtm.wbsps.ca |
| Tap API | http://127.0.0.1:8467 |
| Log | `~/Library/Logs/Ward-Beck Audio Tap/tap.log` |
| Report a problem | send the file from http://127.0.0.1:8467/api/diagnostics/bundle |
| Remove | `bash uninstall.sh` |
