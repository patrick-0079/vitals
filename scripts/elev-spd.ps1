# 内存 SPD 里程碑提权验证：SPD 探针 + CLI 面板 + JSONL 帧
# 用法（先征得用户同意再弹 UAC）：
#   Start-Process pwsh -Verb RunAs -ArgumentList '-NoProfile','-ExecutionPolicy','Bypass',
#     '-File','<repo>\scripts\elev-spd.ps1' -Wait
$ErrorActionPreference = 'Continue'
$repo = Split-Path -Parent $PSScriptRoot
$outDir = Join-Path $env:TEMP 'cs-elev'
New-Item -ItemType Directory -Force -Path $outDir | Out-Null

$cli = Join-Path $repo 'target\release\cs-cli.exe'
$probe = Join-Path $repo 'target\release\examples\spd_probe.exe'

# 1) 静态信息
& $cli --info *> (Join-Path $outDir 'info-spd.txt')

# 2) JSONL 连续帧（12 秒 = 至少两轮 DIMM 温度刷新），用于看 DIMM 温度是否随时间变化
$jsonl = Join-Path $outDir 'cli-jsonl-spd.txt'
$p = Start-Process -FilePath $cli -ArgumentList '--jsonl', '--watch' -RedirectStandardOutput $jsonl `
    -RedirectStandardError (Join-Path $outDir 'cli-jsonl-spd.err.txt') -PassThru -WindowStyle Hidden
Start-Sleep -Seconds 12
if (-not $p.HasExited) { Stop-Process -Id $p.Id -Force }

# 3) 面板（3 帧只打末帧，占用率/温度更稳）
$panel = Join-Path $outDir 'panel-spd.txt'
& $cli *> $panel

# 4) SPD 探针（原始寄存器 + EEPROM 转储）
$probeLog = Join-Path $outDir 'spd-probe.txt'
if (Test-Path $probe) {
    & $probe *> $probeLog
} else {
    "missing $probe" | Set-Content $probeLog
}

Write-Host "=== panel ==="
Get-Content $panel -Encoding UTF8 | Select-String -Pattern '^(CPU|CCD|VOLT|SMU|BOARD|FAN|TEMP|DIMM|MEM|GPU|DISK|\s+!)' |
    ForEach-Object { $_.Line }

Write-Host "=== info (sources) ==="
Get-Content (Join-Path $outDir 'info-spd.txt') -Encoding UTF8 | Select-String -Pattern 'sources|spd|pawnio|smu|superio'

Write-Host "=== dimm temperature per frame ==="
$frames = Get-Content $jsonl -Encoding UTF8
Write-Host ("  frames: {0}" -f $frames.Count)
$i = 0
foreach ($line in $frames) {
    $i++
    $ms = [regex]::Matches($line, '"address":(\d+),"part_number":"([^"]*)","serial_number":"([^"]*)","manufacturer":"([^"]*)"[^}]*?"temp_c":([0-9.]+|null)')
    if ($ms.Count -eq 0) { Write-Host ("  frame {0}: no dimm" -f $i); continue }
    foreach ($mm in $ms) {
        Write-Host ("  frame {0}: 0x{1:X2} {2} {3} temp={4} C" -f `
            $i, [int]$mm.Groups[1].Value, $mm.Groups[2].Value, $mm.Groups[4].Value, $mm.Groups[5].Value)
    }
}

Write-Host "=== spd probe (head) ==="
Get-Content $probeLog -Encoding UTF8 | Select-Object -First 60
Write-Host "--- wrote: $panel , $probeLog , $jsonl"
