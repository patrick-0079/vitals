# web/ —— vitals 的网页展示层

一个**极薄的展示层**：用 `ctypes` 调 `cs_core.dll` 的 6 个 C ABI 函数，把 JSON 原样转发给浏览器，再渲染成一张实时面板。

```
浏览器  ──HTTP/WS──►  web/server.py  ──ctypes──►  cs_core.dll (Rust)
                      只做三件事：            采样线程 500ms 一帧
                      · 调 C ABI              温度/功耗/SPD/SMART 多源降级
                      · 原样转发 JSON
                      · 静态文件服务
```

它**不读硬件、不做单位换算、不做降级判断**——这些全在 Rust 侧。所以：

- 换掉这一层（Vue / React / Qt / 别的语言）不需要动核心，也不需要重新验证采集正确性；
- 核心的任何修正（例如显存语义、SMU 电压）会自动出现在页面上，前端不用跟着改；
- 只要 `cs_core.dll` 能被 `ctypes` 加载，这个壳在 Windows 上就是可替换的参考实现。

页面实测截图见 [`../docs/web-dashboard.png`](../docs/web-dashboard.png)（提权状态，9850X3D + RTX 5080）。

## 依赖与运行

要求 **Python 3.10+**（本机 3.13.9 / Anaconda）与已构建好的 `cs_core.dll`。

```powershell
# 1) 构建核心（仓库根目录）
cargo build --release          # 产物：target\release\cs_core.dll

# 2) 装依赖
python -m pip install -r web\requirements.txt
#    国内镜像：-i https://pypi.tuna.tsinghua.edu.cn/simple

# 3) 起服务（默认 127.0.0.1:8787）
python web\server.py
.\web\run.ps1                  # 等价启动器（自动找 Python、校验 DLL）

# 4) 浏览器打开 http://127.0.0.1:8787/
```

**想要完整数据（温度 / 电压 / 每核 MSR / SPD / 主板 SuperIO）就用管理员终端跑第 3 步**：核心靠 PawnIO 设备访问 SMN/MSR/SMBus/SuperIO，非提权会拿不到这些数据源（页面会明确显示原因，不会编数字）。

### `server.py` 参数

| 参数 | 默认 | 说明 |
|---|---|---|
| `--host` | `127.0.0.1` | 监听地址；`0.0.0.0` = 对局域网开放（**无鉴权，自行评估**） |
| `--port` | `8787` | 端口 |
| `--dll` | 自动查找 | 显式指定 DLL；也可用环境变量 `CS_CORE_DLL` |
| `--interval` | `0.5` | 默认推送周期（秒）；会被钳到 `0.05 ~ 10.0` |
| `--once` | 关 | **自检模式**：取一次 info + metrics 打出来就退出（给脚本用，退出码 0/1） |
| `--json` | 关 | `--once` 时输出整包 JSON |
| `--reload` | 关 | uvicorn 热重载（改前端时用） |

DLL 查找顺序：`--dll` → `CS_CORE_DLL` → `target/release/cs_core.dll` → `target/debug/cs_core.dll` → `web/` 同目录；全都找不到会**把试过的路径列出来**再报错。

## HTTP / WebSocket 接口

| 接口 | 说明 |
|---|---|
| `GET /` | 单页面板（`static/`，无构建步骤、无 npm） |
| `GET /api/info` | 硬件静态信息 + 11 个数据源可用性（`cs_get_info_json`） |
| `GET /api/metrics` | 最新一帧指标；启动瞬间还没有快照时返回 **503** `{"error":"no snapshot yet"}` |
| `GET /api/health` | `{ok, version, dll, uptime_ms}` |
| `GET /api/contract` | 数据契约速查（纯文本），换展示层时先看它 |
| `GET /ws?interval=0.5` | WebSocket 推送（默认 0.5 s） |

WebSocket 消息（全部 JSON，`type` 区分）：

```jsonc
// 服务端 → 客户端，连上立刻发一次
{"type":"hello","version":"0.1.0","dll":"...\\cs_core.dll","interval":0.5,"info":{...}}
{"type":"metrics","data":{...}}          // 按 interval 持续推送
{"type":"interval","value":0.25}         // 对客户端改周期的确认
{"type":"pong","ts_ms":1791030568163}    // 对 ping 的应答
{"type":"error","message":"..."}

// 客户端 → 服务端
{"cmd":"interval","value":1}             // 动态改推送周期（仍会钳到 0.05~10）
{"cmd":"info"}                           // 索要一次静态信息
{"cmd":"ping"}
```

## 数据契约（前端唯一依赖的东西）

`crates/core/src/schema.rs` 是契约的**唯一来源**，`/api/contract` 是它的文本版。三条规则必须记住：

1. **拿不到就是 `null`，不是 `0`**。页面上统一渲染成 `n/a` —— 温度未知和"0 度"是两件事。
2. **空数组 = 该通路整条不可用**（例如非提权的 `per_core: []`、`smu: []`、`dimms: []`）。
3. `sources` 的每个布尔键表示"这条通路本轮是否可用"，其中 **`pawnio_access_denied` 是反向语义**（`true` = 被拒，通常是没提权）。

固定字段的位置：`metrics.ts_ms` 是**核心**的采样时间戳（毫秒）。核心内部 500 ms 一帧，所以把前端周期调得比 0.5 s 更密只会读到重复帧（相邻帧 `ts_ms` 相同）——这是设计，不是卡顿。

## 自检

```powershell
python web\smoke_test.py                          # 打默认 127.0.0.1:8787
python web\smoke_test.py --url http://127.0.0.1:8787 --frames 6 --interval 0.25
```

12 项断言：`/api/health`、`/api/info`（11 个 sources 键齐全）、`/api/metrics`（**逐字段校验契约**，含 `temp_c` 必须是 null 或数字、`superio` 结构、`storage[].temp_sensors_c` 等）、WS 握手、hello 带 info、`interval` 指令往返、`ping` 往返、收到 N 帧、帧结构合格、`ts_ms` 单调不减。提权与非提权下都应 **12/12** —— 它断言的是结构与通道，不是"数据够不够多"。

> 写这个自检踩过的坑：**不要假设指令应答就是下一帧**。推送流随时会插进来一帧 `metrics`，必须"读到目标 `type` 为止"（`smoke_test.py` 里的 `wait_type`）。

`scripts/elev-web.ps1` 是提权版的一键验证：起服务 → 自检 → 落盘 `web-info.json` / `web-metrics.json` → 无头 Edge 整页截图 → 写 `web-ready.marker` 并保持服务 20 s（留窗口期给别的会话补截图）。产物都在 `%TEMP%\cs-elev\`。

## 文件

| 文件 | 作用 |
|---|---|
| `server.py` | C ABI 绑定 + FastAPI 路由 + WebSocket 循环 + `--once` 自检 |
| `smoke_test.py` | 端到端自检（HTTP 契约 + WebSocket 往返 + 指令通道） |
| `run.ps1` | 启动器：找 Python、验 DLL、`-Install` 装依赖、打印权限提示 |
| `requirements.txt` | `fastapi`、`uvicorn[standard]`（后者带 websockets） |
| `static/index.html` | 页面骨架：顶栏 + banner + CPU/内存/GPU/存储/主板/数据源卡片 |
| `static/app.js` | WebSocket 客户端（自动重连）+ 各卡片渲染；`null` 统一渲染 `n/a` |
| `static/style.css` | 深色主题；两列栅格（≤900px 单列），宽卡片 `grid-column: 1/-1` |

## 换成别的展示层

三种粒度：

1. **换样式/布局** —— 只改 `static/`，服务不用重启。
2. **换前端框架** —— 保留 `server.py`，用 `/api/*`（一次性轮询）或 `/ws`（推送）当数据源，静态目录换成构建产物即可。
3. **换语言/进程** —— 复制 `server.py` 里 `class Core` 那 20 行 `ctypes` 绑定，任何能加载 DLL 的语言都能接。C ABI 一共 6 个函数：

```c
int         cs_init();               // 幂等；0 = 成功
const char* cs_version();            // 静态字符串，不要释放
char*       cs_get_info_json();      // 用完 cs_free_string
char*       cs_get_metrics_json();   // 用完 cs_free_string；只读快照，不阻塞
void        cs_free_string(char* p);
void        cs_shutdown();
```

> ctypes 细节：`restype` 要用 `c_void_p`，再用 `ctypes.string_at(ptr)` 拷贝、最后 `cs_free_string(ptr)`。如果写成 `c_char_p`，Python 会直接转成 `bytes` 丢掉指针，就没法释放了。

## 排障

| 现象 | 原因 / 处理 |
|---|---|
| `找不到 cs_core.dll`（错误里列出试过的路径） | 先 `cargo build --release`；或用 `--dll` / `CS_CORE_DLL` 指定 |
| `/api/metrics` 一直 503 | 核心还没采到第一帧（刚启动的瞬间）；等 1 s 再来 |
| 页面大半是 `n/a`，banner 提示要管理员 | 正常：非提权拿不到 PawnIO/SMU/SuperIO/SPD/每核 MSR |
| `[WinError 10048] 端口被占用` | 换 `--port`，或先关掉上一个 `server.py` |
| `ModuleNotFoundError: fastapi` | `python -m pip install -r web\requirements.txt` |
| `python` 不是内部或外部命令 | 本机 `python`/`pip` 不在 PATH，用 `web\run.ps1` 或写全路径（Anaconda 在 `C:\Users\patri\anaconda3\python.exe`） |
| 控制台中文乱码 | 旧代码页问题，`chcp 65001` |
| 改了 `static/` 没生效 | 浏览器缓存；无构建步骤，直接刷新（或强刷） |

**没有鉴权、没有 HTTPS**：默认只监听 `127.0.0.1`。要用 `--host 0.0.0.0` 在局域网里看，请自己在前面加反向代理与认证。
