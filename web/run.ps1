# vitals web 壳启动器：找 Python、验 DLL、装依赖（可选）、起服务。
#
#   .\web\run.ps1                      # 127.0.0.1:8787
#   .\web\run.ps1 -Port 9000
#   .\web\run.ps1 -Install             # 先装 fastapi/uvicorn
#   .\web\run.ps1 -Bind 0.0.0.0        # 允许局域网访问（注意无鉴权）
#
# 想解锁 CPU 温度/功耗、主板 SuperIO、DDR5 SPD、每核 MSR，请用管理员 PowerShell 运行。
[CmdletBinding()]
param(
    [int]$Port = 8787,
    [string]$Bind = "127.0.0.1",
    [string]$Dll = "",
    [switch]$Install
)

$ErrorActionPreference = "Stop"
$here = $PSScriptRoot
$root = Split-Path -Parent $here

# 1) Python：优先 CS_PYTHON，其次 Anaconda 默认位置，最后 PATH
$py = $env:CS_PYTHON
if (-not $py) {
    $candidates = @(
        "C:\Users\patri\anaconda3\python.exe",
        (Join-Path $env:LOCALAPPDATA "Programs\Python\Python313\python.exe")
    )
    foreach ($c in $candidates) { if (Test-Path $c) { $py = $c; break } }
}
if (-not $py) {
    $cmd = Get-Command python -ErrorAction SilentlyContinue
    if ($cmd) { $py = $cmd.Source }
}
if (-not $py -or -not (Test-Path $py)) {
    Write-Error "找不到 python.exe —— 设置环境变量 CS_PYTHON 指向解释器后重试。"
}

# 2) 依赖
$need = & $py -c "import fastapi, uvicorn, websockets" 2>&1
if ($LASTEXITCODE -ne 0) {
    if ($Install) {
        Write-Host "==> 安装依赖（清华镜像）" -ForegroundColor Cyan
        & $py -m pip install -i https://pypi.tuna.tsinghua.edu.cn/simple -r (Join-Path $here "requirements.txt")
        if ($LASTEXITCODE -ne 0) { Write-Error "依赖安装失败" }
    } else {
        Write-Warning "缺少 fastapi/uvicorn/websockets：先跑 .\web\run.ps1 -Install，或按 web/requirements.txt 手动装。"
        Write-Host $need
    }
}

# 3) DLL
$dllPath = if ($Dll) { $Dll } else { Join-Path $root "target\release\cs_core.dll" }
if (-not (Test-Path $dllPath)) {
    Write-Error "找不到 $dllPath —— 先构建核心：cargo build --release"
}

$admin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
Write-Host "vitals web : http://${Bind}:${Port}/" -ForegroundColor Green
Write-Host "dll        : $dllPath"
Write-Host "python     : $py"
if (-not $admin) { Write-Host "权限       : 非管理员（温度/电压/SPD/每核 MSR 会缺失）" -ForegroundColor Yellow }

& $py (Join-Path $here "server.py") --host $Bind --port $Port --dll $dllPath
exit $LASTEXITCODE
