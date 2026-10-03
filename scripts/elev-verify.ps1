# scripts\elev-verify.ps1 — 提权验证几件事：
#   (a) NVMe SMART 温度：非提权时句柄只有 FILE_READ_ATTRIBUTES，SMART 查询一律 err=1
#   (b) RAPL 整包功耗：需连续两帧才有 ΔE/Δt 基线，单帧 jsonl 必然 null
#   (c) SMU PM 表：PawnIO 设备只对管理员开放，Core/SoC 电压提权才拿得到
#
# 用法（会弹 UAC，需手动确认）：
#   Start-Process pwsh -Verb RunAs -ArgumentList '-NoProfile','-ExecutionPolicy','Bypass','-File','<repo>\scripts\elev-verify.ps1' -Wait
#
# 输出落在 %TEMP%\cs-elev\（提权后仍是同一个用户的 TEMP，非提权进程可读）

$ErrorActionPreference = 'Continue'
$repo  = Split-Path -Parent $PSScriptRoot
$out   = Join-Path $env:TEMP 'cs-elev'
New-Item -ItemType Directory -Force -Path $out | Out-Null

$cli   = Join-Path $repo 'target\release\cs-cli.exe'
$probe = Join-Path $repo 'target\release\examples\storage_probe.exe'

foreach ($f in @($cli, $probe)) {
    if (-not (Test-Path $f)) { Write-Host "缺少可执行文件: $f"; exit 1 }
}

$isAdmin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
Write-Host "elevated = $isAdmin"

# --- 1) 静态信息（sources 状态） ---
& $cli --info *> (Join-Path $out 'info.json')

# --- 2) jsonl --watch 连续约 4 秒：收集多帧，功耗从第 2 帧起有值 ---
$jsonl = Join-Path $out 'cli-jsonl.txt'
$p = Start-Process -FilePath $cli -ArgumentList '--jsonl', '--watch' `
        -RedirectStandardOutput $jsonl -RedirectStandardError (Join-Path $out 'cli-jsonl.err.txt') `
        -PassThru -NoNewWindow
Start-Sleep -Seconds 4
if (-not $p.HasExited) { Stop-Process -Id $p.Id -Force }

# --- 3) 面板模式（3 帧，末帧打印；含存储段） ---
& $cli *> (Join-Path $out 'panel.txt')

# --- 4) NVMe 探针：每盘 access / health_error / 4 变体穷举 ---
& $probe *> (Join-Path $out 'storage-probe.txt')

# --- 5) 温度/功耗链路探针：Tctl / CCD / RAPL 能量单位与逐帧功率 ---
$tempProbe = Join-Path $repo 'target\release\examples\temp_probe.exe'
if (Test-Path $tempProbe) { & $tempProbe *> (Join-Path $out 'temp-probe.txt') }

# --- 6) SMU PM 表探针：code name / 表版本 / 原始 f32 / 解码后的电压电流 ---
$smuProbe = Join-Path $repo 'target\release\examples\smu_probe.exe'
if (Test-Path $smuProbe) { & $smuProbe *> (Join-Path $out 'smu-probe.txt') }

Write-Host "`n== 结果目录: $out =="
Get-ChildItem $out | Select-Object Name, Length | Format-Table -AutoSize

# 就地摘要：功耗、SMART 温度与 Core/SoC 电压
$lines = @(Get-Content $jsonl -ErrorAction SilentlyContinue)
Write-Host "jsonl 帧数 = $($lines.Count)"
foreach ($l in $lines) {
    if ($l -match '"package_power_w":([^,}]+)') { Write-Host "  package_power_w = $($Matches[1])" }
}
foreach ($l in $lines) {
    if ($l -match '"core_voltage_v":([^,}]+)') { Write-Host "  core_voltage_v  = $($Matches[1]) ; soc = $(if ($l -match '"soc_voltage_v":([^,}]+)') { $Matches[1] })" }
}