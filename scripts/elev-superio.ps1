# scripts\elev-superio.ps1 — 提权跑 SuperIO 探测探针（PawnIO 设备只对管理员开放）。
#
# 用法（会弹 UAC，需手动确认）：
#   Start-Process pwsh -Verb RunAs -ArgumentList '-NoProfile','-ExecutionPolicy','Bypass','-File','<repo>\scripts\elev-superio.ps1' -Wait
#
# 输出：%TEMP%\cs-elev\superio-probe.txt（提权后仍是同一用户的 TEMP，非提权进程可读）
#
# 注意：探针刻意只输出 ASCII —— 这里的 `*>` 重定向按控制台代码页（GBK）解码再写 UTF-8，
# 中文会变成乱码。

$ErrorActionPreference = 'Continue'
$repo = Split-Path -Parent $PSScriptRoot
$out  = Join-Path $env:TEMP 'cs-elev\superio-probe.txt'

New-Item -ItemType Directory -Force -Path (Split-Path -Parent $out) | Out-Null

$probe = Join-Path $repo 'target\release\examples\superio_probe.exe'
if (-not (Test-Path $probe)) { Write-Host "missing executable: $probe"; exit 1 }

& $probe *> $out
