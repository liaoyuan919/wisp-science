$ErrorActionPreference = "Stop"
Push-Location $PSScriptRoot
try {
    $logo = Resolve-Path "../ui/logo.svg"
    $icon = Join-Path $PSScriptRoot "icons/icon.ico"
    if ((Test-Path $icon) -and ((Get-Item $logo).LastWriteTimeUtc -le (Get-Item $icon).LastWriteTimeUtc)) {
        return
    }
    cargo tauri icon $logo
} finally {
    Pop-Location
}
