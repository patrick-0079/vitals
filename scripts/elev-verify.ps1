# scripts\elev-verify.ps1 — 提权验证几件事：
#   (a) NVMe SMART 温度：非提权时句柄只有 FILE_READ_ATTRIBUTES，SMART 查询一律 err=1
#   (b) RAPL 整包功耗：需连续两帧才有 ΔE/Δt 基线，单帧 jsonl 必然 null
#   (c) SMU PM 表：PawnIO 设备只对管理员开放，Core/SoC 电压提权才拿得到
#   (d) 主板 SuperIO：LPC 端口同样要管理员，风扇/板温/电压提权才拿得到
#   (e) 磁盘活动率/吞吐：IOCTL_DISK_PERFORMANCE 的访问级别是 FILE_READ_ACCESS，非提权一律 err=5
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

# --- 7) 主板 SuperIO 探针：芯片 ID / 运行时基址 / bank 全转储 ---
$superioProbe = Join-Path $repo 'target\release\examples\superio_probe.exe'
if (Test-Path $superioProbe) { & $superioProbe *> (Join-Path $out 'superio-probe.txt') }

# --- 8) 内存 SPD 探针：DDR5 双地址空间 / 页切换 / 温度 ---
$spdProbe = Join-Path $repo 'target\release\examples\spd_probe.exe'
if (Test-Path $spdProbe) { & $spdProbe *> (Join-Path $out 'spd-probe.txt') }

# --- 9) 每核 MSR 探针：物理核拓扑 / P-state 频率 / APERF 有效频率 / 每核功耗 ---
$msrProbe = Join-Path $repo 'target\release\examples\msr_probe.exe'
if (Test-Path $msrProbe) { & $msrProbe *> (Join-Path $out 'msr-probe.txt') }

# --- 10) GPU 内存热传感器探针：NVAPI 显存结温 vs PawnIO 48 路内存传感器 ---
$nvProbe = Join-Path $repo 'target\release\examples\nvidia_pawnio_probe.exe'
if (Test-Path $nvProbe) { & $nvProbe *> (Join-Path $out 'nvidia-probe.txt') }

# --- 11) 磁盘活动率/吞吐探针：非缓冲写/读 512 MiB，对拍计数器与文件实际吞吐 ---
$dpProbe = Join-Path $repo 'target\release\examples\diskperf_probe.exe'
if (Test-Path $dpProbe) { & $dpProbe *> (Join-Path $out 'diskperf-probe.txt') }

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

# 主板 SuperIO：把提权面板里的 BOARD/FAN/VOLT/TEMP 段落摘出来
$panel = @(Get-Content (Join-Path $out 'panel.txt') -ErrorAction SilentlyContinue)
if ($panel.Count -gt 0) {
    Write-Host "`n-- 面板中的主板段 --"
    $panel | Where-Object { $_ -match '^(BOARD|FAN|VOLT|TEMP)\s' } | ForEach-Object { Write-Host "  $_" }
    # Vcore 交叉验证：SuperIO 分压读数 vs SMU PM 表 VDDCR
    $superioVcore = $null
    foreach ($l in $panel) {
        if ($l -match '^VOLT\s.*?Vcore\s+([0-9.]+)V') { $superioVcore = [double]$Matches[1] }
    }
    $smuVcore = $null
    foreach ($l in $lines) {
        if ($l -match '"core_voltage_v":([0-9.]+)') { $smuVcore = [double]$Matches[1] }
    }
    if ($superioVcore -and $smuVcore) {
        $diff = [Math]::Abs($superioVcore - $smuVcore) / $smuVcore * 100
        Write-Host ("  交叉验证: SuperIO Vcore {0:N4} V  vs  SMU VDDCR {1:N4} V  →  偏差 {2:N2}%" -f $superioVcore, $smuVcore, $diff)
    }

    Write-Host "`n-- 面板中的每核段 --"
    $panel | Where-Object { $_ -match '^(CORE|DIMM)\s' } | ForEach-Object { Write-Host "  $_" }
}

# GPU 显存结温：NVAPI（面板 MEMJ）与 PawnIO 内存传感器最大值是否一致
$nvFile = Join-Path $out 'nvidia-probe.txt'
if (Test-Path $nvFile) {
    $nv = @(Get-Content $nvFile -ErrorAction SilentlyContinue)
    $nvapiMem = $null
    foreach ($l in $nv) { if ($l -match 'NVAPI \[2\].*=\s*([0-9.]+) C') { $nvapiMem = [double]$Matches[1] } }
    $pawnMax = $null
    foreach ($l in $nv) { if ($l -match '^\s+memory\[\s*\d+\]\s*=\s*(-?\d+) C') { $v = [double]$Matches[1]; if (-not $pawnMax -or $v -gt $pawnMax) { $pawnMax = $v } } }
    Write-Host "`n-- GPU 显存结温对拍 --"
    if ($nvapiMem) { Write-Host ("  NVAPI Temperatures[2] = {0:N2} C" -f $nvapiMem) }
    if ($pawnMax)  { Write-Host ("  PawnIO 内存传感器最大值 = {0:N0} C" -f $pawnMax) }
    if ($nvapiMem -and $pawnMax) {
        Write-Host ("  差值 = {0:N2} C" -f [Math]::Abs($nvapiMem - $pawnMax))
    }
}

# 每核功耗之和 vs 整包 RAPL：核心域必须小于整包（差值属于 SoC/IO/内存控制器）
foreach ($l in $lines) {
    if ($l -match '"per_core":\[(.*?)\]\}') {
        $sum = 0.0
        $n = 0
        foreach ($m in [regex]::Matches($Matches[1], '"power_w":([0-9.]+)')) { $sum += [double]$m.Groups[1].Value; $n++ }
        if ($n -gt 0) { Write-Host ("  每核功耗之和 = {0:N2} W（{1} 个核）" -f $sum, $n) }
        break
    }
}

# 磁盘活动率/吞吐：面板详情行 + diskperf_probe 的 QueryTime 走速与文件吞吐对拍
$panel | Where-Object { $_ -match '^\s+act R' } | ForEach-Object { Write-Host "  $_" }
$dpFile = Join-Path $out 'diskperf-probe.txt'
if (Test-Path $dpFile) {
    $dp = @(Get-Content $dpFile -ErrorAction SilentlyContinue)
    Write-Host "`n-- 磁盘活动率/吞吐对拍（diskperf_probe）--"
    foreach ($l in $dp) {
        if ($l -match 'ΔQueryTime=.*比值\s+([0-9.]+)') { Write-Host "  QueryTime/墙钟 比值 = $($Matches[1])" }
        if ($l -match 'WriteFile:.*文件吞吐\s+([0-9.]+) MiB/s') { Write-Host "  文件写吞吐 = $($Matches[1]) MiB/s" }
        if ($l -match 'ReadFile:.*文件吞吐\s+([0-9.]+) MiB/s') { Write-Host "  文件读吞吐 = $($Matches[1]) MiB/s" }
    }
    $dp | Where-Object { $_ -match '^\s+\[[0-9]+\] (写|读)活动率' } | ForEach-Object { Write-Host "  $_" }
}