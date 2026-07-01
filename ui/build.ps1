$ErrorActionPreference = "Stop"
Set-Location $PSScriptRoot
Remove-Item Env:TRUNK_NO_COLOR -ErrorAction SilentlyContinue
Remove-Item Env:NO_COLOR -ErrorAction SilentlyContinue
& "$PSScriptRoot\sync-vendor.ps1"
trunk build --release
