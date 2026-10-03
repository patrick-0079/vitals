# scripts\elev-smu.ps1 — 提权跑 SMU PM 表探针（PawnIO 设备只对管理员开放）。
#
# 用法（会弹 UAC，需手动确认）：
#   Start-Process pwsh -Verb RunAs -ArgumentList '-NoProfile','-ExecutionPolicy','Bypass','-File','<repo>\scripts\elev-smu.ps1' -Wait
#
# 输出：%TEMP%\cs-elev\smu-probe2.txt（提权后仍是同一用户的 TEMP，非提权进程可读）

$ErrorActionPreference = 'Continue'
$repo = Split-Path -Parent $PSScriptRoot
$out  = Join-Path $env:TEMP 'cs-elev\smu-probe2.txt'

New-Item -ItemType Directory -Force -Path (Split-Path -Parent $out) | Out-Null

$probe = Join-Path $repo 'target\release\examples\smu_probe.exe'
if (-not (Test-Path $probe)) { Write-Host "缺少可执行文件: $probe"; exit 1 }

& $probe *> $out
