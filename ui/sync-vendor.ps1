# Copy heavy preview assets from web-dist into ui/vendor for offline Tauri builds.
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $MyInvocation.MyCommand.Path
$src = Join-Path (Split-Path -Parent $root) "web-dist"
$dst = Join-Path $root "vendor"
New-Item -ItemType Directory -Force -Path $dst | Out-Null

$files = @(
  "assets\vendor-BoUatD0H.js",
  "assets\RDKit_minimal-B7RkdM0_.js",
  "assets\RDKit_minimal-tnscgqxm.wasm",
  "assets\3Dmol-DfD4xImO.js",
  "assets\katex-Dn761jRB.js",
  "assets\katex-DwwF5kvc.css",
  "assets\pdf.worker.min-qwK7q_zL.mjs",
  "vendor\nightingale-msa-5.6.0.js"
)
foreach ($f in $files) {
  $from = Join-Path $src $f
  if (-not (Test-Path $from)) { Write-Warning "skip missing $f"; continue }
  $name = Split-Path $f -Leaf
  Copy-Item $from (Join-Path $dst $name) -Force
}

# KaTeX fonts (referenced by katex css)
Get-ChildItem (Join-Path $src "assets") -Filter "KaTeX_*" | ForEach-Object {
  Copy-Item $_.FullName (Join-Path $dst $_.Name) -Force
}
Write-Host "vendor synced to $dst"
