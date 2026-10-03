"""vitals web 壳 —— 把 `cs_core.dll` 的 JSON 快照推给浏览器。

设计要点（前端可替换，同 Rust 核心的解耦原则）：

* **只依赖 C ABI**：`cs_init` / `cs_version` / `cs_get_info_json` /
  `cs_get_metrics_json` / `cs_free_string` / `cs_shutdown`。不 import 任何
  Rust 侧类型，也不自己读硬件 —— 这个进程唯一的职责是搬运 JSON。
* **采样在 DLL 内部**：`cs_init()` 后 Rust 侧有一个采样线程按自己的节奏
  （0.5 s 一帧）刷新快照，`cs_get_metrics_json()` 只是克隆当前快照，不阻塞、
  不碰硬件。所以 web 层可以自由地用任意频率抓帧，抓快了也只是重复读同一份。
* **推送用 WebSocket**：浏览器订阅 `/ws`，服务端按客户端指定的周期推帧；
  `GET /api/metrics` 保留一次性抓取（便于 curl / 其它展示层复用）。

用法::

    python web/server.py                 # 起服务，默认 127.0.0.1:8787
    python web/server.py --port 9000
    python web/server.py --once          # 只打一帧信息+指标后退出（自检用）
    python web/server.py --once --json   # 同上，机器可读

DLL 查找顺序：`--dll` > 环境变量 `CS_CORE_DLL` > `target/release/cs_core.dll`
> `target/debug/cs_core.dll` > 本脚本同目录。
"""

from __future__ import annotations

import argparse
import asyncio
import ctypes
import json
import os
import time
from contextlib import asynccontextmanager
from pathlib import Path

from fastapi import FastAPI, WebSocket, WebSocketDisconnect
from fastapi.responses import JSONResponse, PlainTextResponse
from fastapi.staticfiles import StaticFiles
from starlette.concurrency import run_in_threadpool

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent
STATIC_DIR = HERE / "static"

# 帧间隔下限/上限（秒）。下限保护：DLL 内部固定 0.5 s 一帧，抓更密只是重复读。
MIN_INTERVAL = 0.05
MAX_INTERVAL = 10.0


# ---------------------------------------------------------------------------
# DLL 装载


def find_dll(explicit: str | None = None) -> Path:
    """按固定顺序找 cs_core.dll，找不到就抛 FileNotFoundError 并列出试过的路径。"""
    tried: list[Path] = []

    def consider(p: Path | None) -> Path | None:
        if p is None:
            return None
        tried.append(p)
        return p if p.is_file() else None

    env = os.environ.get("CS_CORE_DLL")
    candidates = [
        Path(explicit) if explicit else None,
        Path(env) if env else None,
        ROOT / "target" / "release" / "cs_core.dll",
        ROOT / "target" / "debug" / "cs_core.dll",
        HERE / "cs_core.dll",
    ]
    for c in candidates:
        hit = consider(c)
        if hit is not None:
            return hit
    raise FileNotFoundError(
        "找不到 cs_core.dll，试过：\n  " + "\n  ".join(str(p) for p in tried)
        + "\n先跑 `cargo build --release`，或用 --dll / CS_CORE_DLL 指定路径。"
    )


class Core:
    """cs_core.dll 的 ctypes 包装。所有调用都是「取快照」，无阻塞风险。"""

    def __init__(self, path: Path) -> None:
        self.path = path
        # WinDLL 用 stdcall 约定；导出函数都是 extern "C" fn，两种约定在
        # x64 上同为 Microsoft ABI，用 CDLL 即可（与 LHM 的 P/Invoke 一致）。
        self.dll = ctypes.CDLL(str(path))
        self._bind()
        self.dll.cs_init()

    def _bind(self) -> None:
        d = self.dll
        d.cs_init.restype = ctypes.c_int
        d.cs_init.argtypes = []
        d.cs_version.restype = ctypes.c_char_p
        d.cs_version.argtypes = []
        d.cs_get_info_json.restype = ctypes.c_void_p
        d.cs_get_info_json.argtypes = []
        d.cs_get_metrics_json.restype = ctypes.c_void_p
        d.cs_get_metrics_json.argtypes = []
        d.cs_free_string.restype = None
        d.cs_free_string.argtypes = [ctypes.c_void_p]
        d.cs_shutdown.restype = None
        d.cs_shutdown.argtypes = []

    def version(self) -> str:
        v = self.dll.cs_version()
        return v.decode("utf-8", "replace") if v else ""

    def _take(self, fn) -> dict | None:
        """调用一个返回 `*mut c_char` 的导出函数，读完立刻交还给 Rust 释放。

        返回 NULL 表示「还没 init / 序列化失败」，此时返回 None 而不是抛错 ——
        宿主据此显示「等待首帧」。
        """
        ptr = fn()
        if not ptr:
            return None
        try:
            raw = ctypes.string_at(ptr)
        finally:
            self.dll.cs_free_string(ptr)
        return json.loads(raw.decode("utf-8"))

    def info(self) -> dict | None:
        return self._take(self.dll.cs_get_info_json)

    def metrics(self) -> dict | None:
        return self._take(self.dll.cs_get_metrics_json)

    # 线程池版本：ctypes 调用期间会释放 GIL，放线程里纯粹是为了不占事件循环
    async def info_async(self) -> dict | None:
        return await run_in_threadpool(self.info)

    async def metrics_async(self) -> dict | None:
        return await run_in_threadpool(self.metrics)

    def shutdown(self) -> None:
        try:
            self.dll.cs_shutdown()
        except OSError:
            pass


def clamp_interval(value: object, fallback: float = 0.5) -> float:
    try:
        v = float(value)  # type: ignore[arg-type]
    except (TypeError, ValueError):
        return fallback
    if v != v:  # NaN
        return fallback
    return min(MAX_INTERVAL, max(MIN_INTERVAL, v))


# ---------------------------------------------------------------------------
# FastAPI 应用


def build_app(core: Core) -> FastAPI:
    @asynccontextmanager
    async def lifespan(app: FastAPI):
        app.state.core = core
        app.state.started_ms = int(time.time() * 1000)
        yield
        core.shutdown()

    app = FastAPI(title="vitals web", version=core.version(), lifespan=lifespan)
    app.state.core = core

    @app.get("/api/info")
    async def api_info():
        data = await core.info_async()
        if data is None:
            return JSONResponse({"error": "no snapshot yet"}, status_code=503)
        return data

    @app.get("/api/metrics")
    async def api_metrics():
        data = await core.metrics_async()
        if data is None:
            return JSONResponse({"error": "no snapshot yet"}, status_code=503)
        return data

    @app.get("/api/health")
    async def api_health():
        return {
            "ok": True,
            "version": core.version(),
            "dll": str(core.path),
            "uptime_ms": int(time.time() * 1000) - app.state.started_ms,
        }

    @app.get("/api/contract")
    async def api_contract():
        """把 schema 契约的字段注释摊平成纯文本，方便前端开发者对照（无需 Rust 工具链）。"""
        return PlainTextResponse(CONTRACT, media_type="text/plain; charset=utf-8")

    @app.websocket("/ws")
    async def ws_endpoint(ws: WebSocket):
        await ws.accept()
        interval = clamp_interval(ws.query_params.get("interval"), 0.5)
        stop = asyncio.Event()

        async def reader() -> None:
            """客户端可以在帧之间发指令：改周期 / ping。"""
            nonlocal interval
            try:
                while True:
                    msg = await ws.receive_json()
                    cmd = msg.get("cmd") if isinstance(msg, dict) else None
                    if cmd == "interval":
                        interval = clamp_interval(msg.get("value"), interval)
                        await ws.send_json({"type": "interval", "value": interval})
                    elif cmd == "info":
                        await ws.send_json({"type": "info", "data": await core.info_async()})
                    elif cmd == "ping":
                        await ws.send_json({"type": "pong", "ts_ms": int(time.time() * 1000)})
            except (WebSocketDisconnect, RuntimeError, ValueError):
                stop.set()

        reader_task = asyncio.create_task(reader())
        try:
            await ws.send_json({
                "type": "hello",
                "version": core.version(),
                "dll": str(core.path),
                "interval": interval,
                "info": await core.info_async(),
            })
            while not stop.is_set():
                data = await core.metrics_async()
                if data is not None:
                    await ws.send_json({"type": "metrics", "data": data})
                await asyncio.sleep(interval)
        except (WebSocketDisconnect, RuntimeError):
            pass
        finally:
            reader_task.cancel()

    # 静态页放最后：`/` 由 index.html 兜底，/api 与 /ws 先匹配
    app.mount("/", StaticFiles(directory=str(STATIC_DIR), html=True), name="static")
    return app


CONTRACT = """\
vitals 数据契约（crates/core/src/schema.rs 的 JSON 投影）

GET /api/info  ->  Info
  version              str    核心版本
  cpu_name             str
  logical_cores        int
  cpu_family/model     int    CPUID 值（family 0x1A = Zen5）
  total_memory_gb      float
  gpu_name             str|null
  platform             str
  smu_pm_table_version int|null  如 6422789 = 0x00620105
  superio_chip         str|null
  sources              {pawnio, pawnio_access_denied, nvml, nvidia_smi, wmi,
                        storage_smart, storage_perf, smu, superio, spd, msr_cores}

GET /api/metrics -> Metrics
  ts_ms    int
  cpu      { name, usage_pct, per_core_pct[], freq_mhz, temp_c|null, temp_source,
             ccd_temps_c[], package_power_w|null, core_voltage_v|null,
             soc_voltage_v|null, smu[{name,kind,value,unit}],
             per_core[{index,thread,clock_mhz,effective_mhz,power_w|null,
                       aperf_mperf_ratio|null}] }
  memory   { usage_pct, used_gb, total_gb }
  gpu      null | { name, usage_pct, temp_c, hotspot_c, mem_junction_c,
             temp_margin_c, temp_slowdown_c, temp_shutdown_c, temp_max_c,
             vram_used_mb, vram_total_mb, vram_free_mb, vram_reserved_mb,
             vram_usage_pct, core_clock_mhz, mem_clock_mhz, max_core_clock_mhz,
             max_mem_clock_mhz, power_w, power_limit_w, power_limit_pct,
             fan_pct, fan_rpm, fan_count, encoder_pct, decoder_pct,
             pcie_tx_mib_s, pcie_rx_mib_s, pcie_link, throttle_reasons[]|null,
             source }
  storage  [ { index, name, bus, temp_c, temp_sensors_c[], warning_temp_c,
             critical_temp_c, percentage_used_pct, available_spare_pct,
             power_on_hours, data_written_gb, data_read_gb, data_units_read,
             data_units_written, activity_read_pct, activity_write_pct,
             activity_total_pct, read_mib_s, write_mib_s, source } ]
  superio  null | { chip, profile, sensors[{name,kind,value,unit}] }
  dimms    [ { index, address, part_number, serial_number, manufacturer,
             manufacture_date, temp_c, thermal_status, source } ]

约定：拿不到的字段是 null（不是 0）；数组为空表示该通路不可用。
"""


# ---------------------------------------------------------------------------
# 入口


def main() -> int:
    ap = argparse.ArgumentParser(description="vitals web 壳（FastAPI + cs_core.dll）")
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--port", type=int, default=8787)
    ap.add_argument("--dll", default=None, help="cs_core.dll 路径（默认自动查找）")
    ap.add_argument("--interval", type=float, default=0.5, help="默认推送周期（秒）")
    ap.add_argument("--once", action="store_true", help="只打一帧后退出（自检）")
    ap.add_argument("--json", action="store_true", help="--once 时输出 JSON 而不是摘要")
    ap.add_argument("--reload", action="store_true", help="uvicorn 热重载（开发用）")
    args = ap.parse_args()

    try:
        dll_path = find_dll(args.dll)
    except FileNotFoundError as e:
        print(e, file=os.sys.stderr)
        return 2

    if args.once:
        core = Core(dll_path)
        try:
            # DLL 内部采样线程刚起来时第一帧可能还没生成，等一小会
            info = metrics = None
            for _ in range(60):
                info, metrics = core.info(), core.metrics()
                if info is not None and metrics is not None:
                    break
                time.sleep(0.1)
            if args.json:
                print(json.dumps({"dll": str(dll_path), "info": info, "metrics": metrics},
                                 ensure_ascii=False, indent=2))
            else:
                print(f"dll      : {dll_path}")
                print(f"version  : {core.version()}")
                print(f"info     : {'ok' if info else 'NULL'}")
                print(f"metrics  : {'ok' if metrics else 'NULL'}")
                if info:
                    print(f"cpu      : {info.get('cpu_name')} ({info.get('logical_cores')} 线程)")
                    print(f"sources  : {info.get('sources')}")
                if metrics:
                    cpu, mem = metrics.get("cpu", {}), metrics.get("memory", {})
                    gpu = metrics.get("gpu") or {}
                    print(f"cpu      : {cpu.get('usage_pct')}% {cpu.get('freq_mhz')}MHz "
                          f"temp={cpu.get('temp_c')} pkg={cpu.get('package_power_w')}")
                    print(f"memory   : {mem.get('used_gb')}/{mem.get('total_gb')} GB")
                    if gpu:
                        print(f"gpu      : {gpu.get('usage_pct')}% "
                              f"{gpu.get('vram_used_mb')}/{gpu.get('vram_total_mb')} MiB "
                              f"temp={gpu.get('temp_c')} memj={gpu.get('mem_junction_c')}")
                    print(f"storage  : {[s.get('name') for s in metrics.get('storage', [])]}")
                    print(f"dimms    : {len(metrics.get('dimms', []))} 条")
            return 0 if (info and metrics) else 1
        finally:
            core.shutdown()

    core = Core(dll_path)
    app = build_app(core)
    import uvicorn

    print(f"vitals web : http://{args.host}:{args.port}/")
    print(f"dll        : {dll_path} (core {core.version()})")
    print(f"api        : /api/info  /api/metrics  /api/health  /api/contract  ws /ws")
    if not core.metrics():
        print("注意       : 首帧还没就绪，稍等 1 秒刷新即可")
    uvicorn.run(app, host=args.host, port=args.port, log_level="info",
                reload=False if not args.reload else True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
