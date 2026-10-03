# 只跑 GPU 热点/显存结温的 PawnIO 对拍探针（nvidia_pawnio_probe），结果落盘到 %TEMP%\cs-elev\nvidia-probe.txt
# 用法：Start-Process pwsh -Verb RunAs -ArgumentList '-NoProfile','-ExecutionPolicy','Bypass','-File','<repo>\scripts\elev-nvidia.ps1' -Wait
$ErrorActionPreference = 'Continue'
$repo = Split-Path -Parent $PSScriptRoot
$out = Join-Path $env:TEMP 'cs-elev'
New-Item -ItemType Directory -Force -Path $out | Out-Null

$exe = Join-Path $repo 'target\release\examples\nvidia_pawnio_probe.exe'
if (-not (Test-Path $exe)) { Write-Host "missing: $exe"; exit 1 }

& $exe *> (Join-Path $out 'nvidia-probe.txt')
Write-Host "wrote $(Join-Path $out 'nvidia-probe.txt')"
