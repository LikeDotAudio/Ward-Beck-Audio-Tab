# Copyright (c) 2026 Ward-Beck Systems (Ward-Beck.ca). All rights reserved.
# Ward-Beck Audio Tap for Windows: installs the Tap for this user and starts it at sign-in.
#   Right-click > Run with PowerShell, or:  powershell -ExecutionPolicy Bypass -File install.ps1
# Nothing needs administrator rights. Windows Firewall may ask once to let the Tap listen
# on the network (SAP / PTP / AES67): allow Private networks.

$ErrorActionPreference = 'Stop'
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$dest = Join-Path $env:LOCALAPPDATA 'Ward-Beck\Audio Tap'
$exe = Join-Path $dest 'ward-beck-audio-tab.exe'
$task = 'Ward-Beck Audio Tap'

Write-Host '===================================================================='
Write-Host ' Installing Ward-Beck Audio Tap (Windows)'
Write-Host '===================================================================='

# Stop a running copy so the exe can be replaced.
Get-Process -Name 'ward-beck-audio-tab' -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 500

New-Item -ItemType Directory -Force -Path $dest | Out-Null
Copy-Item -Force (Join-Path $here 'ward-beck-audio-tab.exe') $exe
foreach ($f in 'README.md', 'LICENSE', 'uninstall.ps1') {
  $p = Join-Path $here $f
  if (Test-Path $p) { Copy-Item -Force $p $dest }
}
Write-Host "OK Executable installed to: $exe"

# Start at sign-in, for this user, hidden, restarted if it stops.
$action = New-ScheduledTaskAction -Execute $exe -Argument '--no-browser'
$trigger = New-ScheduledTaskTrigger -AtLogOn -User $env:USERNAME
$settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries `
  -RestartCount 999 -RestartInterval (New-TimeSpan -Minutes 1) -ExecutionTimeLimit ([TimeSpan]::Zero)
Register-ScheduledTask -TaskName $task -Action $action -Trigger $trigger -Settings $settings -Force | Out-Null
Start-ScheduledTask -TaskName $task
Write-Host "OK Starts at sign-in (Task Scheduler: '$task') and is running now"

Start-Sleep -Seconds 3
try {
  $d = Invoke-RestMethod -Uri 'http://127.0.0.1:8467/api/diagnostics' -TimeoutSec 5
  Write-Host "OK The Tap answers on http://127.0.0.1:8467 (version $($d.tap.version), audio: $($d.tap.audio_backend))"
} catch {
  Write-Host "!! The Tap did not answer yet. Log: $env:LOCALAPPDATA\Ward-Beck\Audio Tap\logs\tap.log"
}
Write-Host ''
Write-Host 'Open https://ward-beck.ca (or xtm.wbsps.ca) to meter and route.'
Write-Host "Log file:    $env:LOCALAPPDATA\Ward-Beck\Audio Tap\logs\tap.log"
Write-Host 'Diagnostics: http://127.0.0.1:8467/api/diagnostics/bundle  (send this file with any report)'
Write-Host '===================================================================='
