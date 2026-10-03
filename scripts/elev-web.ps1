# 提权端到端验证 web 壳：用管理员身份起服务 -> 自检 -> 落盘 JSON/截图。
#
#   powershell -ExecutionPolicy Bypass -File scripts\elev-web.ps1
#
# 产出（都在 %TEMP%\cs-elev\）：
#   web-server.txt / web-server-err.txt   uvicorn 日志
#   web-smoke.txt                        smoke_test.py 全量输出
#   web-info.json / web-metrics.json     提权状态下的 info 与一帧 metrics（证据）
#   web-elevated.png                     整页截图（尽力而为，失败不影响结论）
#
# 说明：脚本跑完会再撑 20 秒才停服务，方便外部（非提权会话）用无头浏览器补截一张图。
[CmdletBinding()]
param(
    [int]$Port = 8788,
    [int]$HoldSeconds = 20
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$out = Join-Path $env:TEMP "cs-elev"
New-Item -ItemType Directory -Force -Path $out | Out-Null

$py = $env:CS_PYTHON
if (-not $py) { $py = "C:\Users\patri\anaconda3\python.exe" }
if (-not (Test-Path $py)) { throw "找不到 python：$py" }

$edge = "C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe"
if (-not (Test-Path $edge)) { $edge = "C:\Program Files\Microsoft\Edge\Application\msedge.exe" }

$admin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
Write-Host "elevated  : $admin"
if (-not $admin) { Write-Warning "没提权的话温度/电压/SPD/每核数据仍然是空的" }

$serverLog = Join-Path $out "web-server.txt"
$serverErr = Join-Path $out "web-server-err.txt"
$base = "http://127.0.0.1:$Port"

Write-Host "==> 启动服务 $base"
$proc = Start-Process -FilePath $py `
    -ArgumentList @((Join-Path $root "web\server.py"), "--port", "$Port") `
    -PassThru -WindowStyle Hidden `
    -RedirectStandardOutput $serverLog -RedirectStandardError $serverErr

try {
    $ready = $false
    foreach ($i in 1..60) {
        try {
            $h = Invoke-RestMethod "$base/api/health" -TimeoutSec 2
            $ready = $true
            break
        } catch { Start-Sleep -Milliseconds 500 }
    }
    if (-not $ready) { throw "服务在 30 秒内没起来，看 $serverErr" }
    Write-Host "==> 服务就绪（pid $($proc.Id)），core $($h.version)"

    Write-Host "==> smoke_test"
    & $py (Join-Path $root "web\smoke_test.py") --url $base --frames 4 --interval 0.25 *> (Join-Path $out "web-smoke.txt")
    $smokeExit = $LASTEXITCODE

    Write-Host "==> 落盘 info / metrics 证据"
    Invoke-RestMethod "$base/api/info" | ConvertTo-Json -Depth 10 | Set-Content (Join-Path $out "web-info.json") -Encoding UTF8
    Invoke-RestMethod "$base/api/metrics" | ConvertTo-Json -Depth 10 | Set-Content (Join-Path $out "web-metrics.json") -Encoding UTF8

    Write-Host "==> 截图（无头 Edge）"
    $shot = Join-Path $out "web-elevated.png"
    Remove-Item $shot -ErrorAction SilentlyContinue
    if (Test-Path $edge) {
        & $edge --headless=new --disable-gpu --hide-scrollbars --virtual-time-budget=6000 `
            --window-size=1680,1500 --screenshot="$shot" "$base/" 2>&1 | Out-Null
    }
    $shotOk = Test-Path $shot
    Write-Host "截图：$(if ($shotOk) { (Get-Item $shot).Length.ToString() + ' bytes' } else { '失败（不影响结论，可外部补截）' })"

    "ready" | Set-Content (Join-Path $out "web-ready.marker") -Encoding ASCII
    Write-Host "==> 保持服务 $HoldSeconds 秒（供外部补截图），然后停止"
    Start-Sleep -Seconds $HoldSeconds

    Write-Host ""
    Write-Host "==== smoke_test 摘要 ===="
    Get-Content (Join-Path $out "web-smoke.txt") -Encoding UTF8 | Select-Object -Last 12
    Write-Host ""
    Write-Host "smoke exit = $smokeExit（0 = 全过）"
} finally {
    if ($proc -and -not $proc.HasExited) { Stop-Process -Id $proc.Id -Force }
    Remove-Item (Join-Path $out "web-ready.marker") -ErrorAction SilentlyContinue
    Write-Host "==> 服务已停止"
}
