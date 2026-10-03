# 只跑磁盘活动率/吞吐探针（diskperf_probe），结果落盘到 %TEMP%\cs-elev\diskperf-probe-elev.txt
# 该探针会非缓冲写入 512 MiB 测试文件（默认 %TEMP%），跑完自动删除。
# 不改任何系统设置：IOCTL_DISK_PERFORMANCE 在 Windows 上默认由 diskperf 按需启用，
# 遗留 IOCTL 通路要 diskperf -Y 才开 —— 本脚本只观测，不开。
# 用法：Start-Process pwsh -Verb RunAs -ArgumentList '-NoProfile','-ExecutionPolicy','Bypass','-File','<repo>\scripts\elev-diskperf.ps1' -Wait
$ErrorActionPreference = 'Continue'
$repo = Split-Path -Parent $PSScriptRoot
$out = Join-Path $env:TEMP 'cs-elev'
New-Item -ItemType Directory -Force -Path $out | Out-Null

$exe = Join-Path $repo 'target\release\examples\diskperf_probe.exe'
if (-not (Test-Path $exe)) { Write-Host "missing: $exe"; exit 1 }

Write-Host "diskperf 当前状态:"; diskperf
$log = Join-Path $out 'diskperf-probe-elev.txt'
& $exe *> $log
Write-Host "wrote $log"
