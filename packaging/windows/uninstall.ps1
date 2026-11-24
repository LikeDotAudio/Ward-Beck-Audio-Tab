# Copyright (c) 2026 Ward-Beck Systems (Ward-Beck.ca). All rights reserved.
# Removes the Ward-Beck Audio Tap for this user (the log folder is kept).

$task = 'Ward-Beck Audio Tap'
$dest = Join-Path $env:LOCALAPPDATA 'Ward-Beck\Audio Tap'
Unregister-ScheduledTask -TaskName $task -Confirm:$false -ErrorAction SilentlyContinue
Get-Process -Name 'ward-beck-audio-tab' -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 500
Get-ChildItem $dest -File -ErrorAction SilentlyContinue | Remove-Item -Force
Write-Host "Ward-Beck Audio Tap removed. Logs kept in $dest\logs"
