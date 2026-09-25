# Deploy AkiBloom to the current user's VST3/CLAP directories (no admin needed).
# Usage: powershell -ExecutionPolicy Bypass -File scripts/deploy.ps1
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$bundled = Join-Path $root "target\bundled"
if (-not (Test-Path $bundled)) {
    Write-Error "target\bundled 不存在——先运行: cargo xtask bundle akibloom --release"
}

$vst3Dir = Join-Path $env:LOCALAPPDATA "Programs\Common\VST3"
$clapDir = Join-Path $env:LOCALAPPDATA "Programs\Common\CLAP"
New-Item -ItemType Directory -Force -Path $vst3Dir | Out-Null
New-Item -ItemType Directory -Force -Path $clapDir | Out-Null

Copy-Item -Recurse -Force (Join-Path $bundled "AkiBloom.vst3") $vst3Dir
Copy-Item -Force (Join-Path $bundled "AkiBloom.clap") $clapDir
Copy-Item -Force (Join-Path $bundled "AkiBloom.exe") $vst3Dir  # standalone 放一起方便找

Write-Output "已部署:"
Write-Output "  $vst3Dir\AkiBloom.vst3"
Write-Output "  $clapDir\AkiBloom.clap"
Write-Output "  $vst3Dir\AkiBloom.exe (standalone)"
Write-Output ""
Write-Output "REAPER 若未自动扫描,请在 Options > Preferences > VST/Plug-in Paths 加入:"
Write-Output "  $vst3Dir"
Write-Output "  $clapDir"
Write-Output "然后点 Re-scan。"
