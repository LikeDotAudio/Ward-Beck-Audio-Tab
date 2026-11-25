# Ward-Beck Audio Tap — Windows (preview)

**Download:** [ward-beck-audio-tap-windows.zip](https://github.com/WardBeckSystems/Ward-Beck-Audio-Tap/releases/latest/download/ward-beck-audio-tap-windows.zip)
(Windows 10/11 x64) · newest build: [nightly](https://github.com/WardBeckSystems/Ward-Beck-Audio-Tap/releases/download/nightly/ward-beck-audio-tap-windows.zip)

1. Unzip.
2. Right-click `install.ps1` › **Run with PowerShell** (no administrator rights needed).
3. If Windows Firewall asks, allow the Tap on **Private** networks (SAP, PTP and AES67 use multicast).

The Tap installs to `%LOCALAPPDATA%\Ward-Beck\Audio Tap` and starts at sign-in (Task Scheduler:
"Ward-Beck Audio Tap").

**Preview:** the core runs (web app API, MXL rings, generators, AES67, SAP/NMOS/PTP/NTP, logging);
capturing system audio through WASAPI loopback is still being written.

| | |
|---|---|
| Web app | https://ward-beck.ca · https://xtm.wbsps.ca |
| Tap API | http://127.0.0.1:8467 |
| Log | `%LOCALAPPDATA%\Ward-Beck\Audio Tap\logs\tap.log` |
| Report a problem | send the file from http://127.0.0.1:8467/api/diagnostics/bundle |
| Remove | right-click `uninstall.ps1` › Run with PowerShell |

**Setup manual with pictures:** `manual/windows.html` in the download ([docs/manual/windows.html](../docs/manual/windows.html) here).
