# 只跑 AMD 每核时钟/VID/核心功耗探针（msr_probe），结果落盘到 %TEMP%\cs-elev\msr-probe.txt
# 用法：Start-Process pwsh -Verb RunAs -ArgumentList '-NoProfile','-ExecutionPolicy','Bypass','-File','<repo>\scripts\elev-msr.ps1' -Wait
$ErrorActionPreference = 'Continue'
$repo = Split-Path -Parent $PSScriptRoot
$out = Join-Path $env:TEMP 'cs-elev'
New-Item -ItemType Directory -Force -Path $out | Out-Null

$exe = Join-Path $repo 'target\release\examples\msr_probe.exe'
if (-not (Test-Path $exe)) { Write-Host "missing: $exe"; exit 1 }

& $exe *> (Join-Path $out 'msr-probe.txt')
Write-Host "wrote $(Join-Path $out 'msr-probe.txt')"
