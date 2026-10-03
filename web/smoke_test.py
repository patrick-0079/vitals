"""vitals web 壳自检 —— 对已起的服务做端到端校验（HTTP + WebSocket）。

    python web/server.py            # 一个终端起服务
    python web/smoke_test.py        # 另一个终端校验

校验点：
  [1] /api/health 返回 ok 与 dll 路径
  [2] /api/info 的 sources 字典齐全（11 个键）
  [3] /api/metrics 的结构与契约一致（必需字段类型正确）
  [4] WebSocket 收到 hello + N 帧 metrics，且 ts_ms 单调递增
  [5] WebSocket 指令通道（interval / ping）有应答

退出码 0 = 全过，1 = 有失败项。
"""

from __future__ import annotations

import argparse
import asyncio
import json
import sys
import urllib.error
import urllib.request

try:  # websockets >= 13 的新 asyncio 实现
    from websockets.asyncio.client import connect
except ImportError:  # pragma: no cover - 老版本
    from websockets.client import connect

SOURCE_KEYS = {
    "pawnio", "pawnio_access_denied", "nvml", "nvidia_smi", "wmi",
    "storage_smart", "storage_perf", "smu", "superio", "spd", "msr_cores",
}

failures: list[str] = []
checks = 0


def check(name: str, ok: bool, detail: str = "") -> None:
    global checks
    checks += 1
    mark = "PASS" if ok else "FAIL"
    print(f"[{mark}] {name}" + (f" — {detail}" if detail else ""))
    if not ok:
        failures.append(name)


def get_json(base: str, path: str) -> tuple[int, dict | None]:
    try:
        with urllib.request.urlopen(base + path, timeout=10) as r:
            return r.status, json.loads(r.read().decode("utf-8"))
    except urllib.error.HTTPError as e:
        return e.code, None
    except Exception as e:  # noqa: BLE001
        return 0, {"error": repr(e)}


def validate_metrics(m: dict) -> list[str]:
    """返回结构不合格的原因列表（空 = 通过）。"""
    bad: list[str] = []
    if not isinstance(m.get("ts_ms"), int):
        bad.append("ts_ms 不是整数")
    cpu = m.get("cpu")
    if not isinstance(cpu, dict):
        bad.append("cpu 缺失")
    else:
        for k, t in (("name", str), ("usage_pct", (int, float)), ("per_core_pct", list),
                     ("freq_mhz", int), ("ccd_temps_c", list), ("smu", list), ("per_core", list)):
            if not isinstance(cpu.get(k), t):
                bad.append(f"cpu.{k} 类型不对")
        if cpu.get("temp_c") is not None and not isinstance(cpu["temp_c"], (int, float)):
            bad.append("cpu.temp_c 既不是 null 也不是数字")
    mem = m.get("memory")
    if not isinstance(mem, dict) or not all(isinstance(mem.get(k), (int, float))
                                            for k in ("usage_pct", "used_gb", "total_gb")):
        bad.append("memory 字段不全")
    gpu = m.get("gpu")
    if gpu is not None:
        for k in ("name", "usage_pct", "vram_used_mb", "vram_total_mb", "vram_usage_pct", "source"):
            if k not in gpu:
                bad.append(f"gpu.{k} 缺失")
        if gpu.get("hotspot_c") is not None and not isinstance(gpu["hotspot_c"], (int, float)):
            bad.append("gpu.hotspot_c 类型不对")
    if not isinstance(m.get("storage"), list):
        bad.append("storage 不是数组")
    else:
        for d in m["storage"]:
            for k in ("index", "name", "bus", "temp_sensors_c", "source"):
                if k not in d:
                    bad.append(f"storage[].{k} 缺失")
                    break
    if m.get("superio") is not None:
        s = m["superio"]
        if not isinstance(s, dict) or not isinstance(s.get("sensors"), list):
            bad.append("superio 结构不对")
    if not isinstance(m.get("dimms"), list):
        bad.append("dimms 不是数组")
    return bad


async def wait_type(ws, want: str, timeout: float = 15.0) -> dict:
    """读到指定 type 的帧为止 —— 推送流里随时可能插进 metrics 帧，不能假设顺序。"""
    while True:
        msg = json.loads(await asyncio.wait_for(ws.recv(), timeout=timeout))
        if msg.get("type") == want:
            return msg


async def ws_round(base_ws: str, frames: int, interval: float) -> None:
    url = f"{base_ws}/ws?interval={interval}"
    async with connect(url, open_timeout=10) as ws:
        hello = await wait_type(ws, "hello")
        check("WS hello 帧", hello.get("type") == "hello",
              f"version={hello.get('version')} dll={hello.get('dll')}")
        check("WS hello 带 info", isinstance(hello.get("info"), dict))

        # 指令通道：改周期 + ping
        await ws.send(json.dumps({"cmd": "interval", "value": interval}))
        ack = await wait_type(ws, "interval")
        check("WS interval 指令", ack.get("value") == interval, f"value={ack.get('value')}")
        await ws.send(json.dumps({"cmd": "ping"}))
        pong = await wait_type(ws, "pong")
        check("WS ping 指令", pong.get("type") == "pong")

        stamps: list[int] = []
        bad_frames: list[str] = []
        first: dict | None = None
        for _ in range(frames):
            data = (await wait_type(ws, "metrics"))["data"]
            first = first or data
            stamps.append(data.get("ts_ms", 0))
            problem = validate_metrics(data)
            if problem:
                bad_frames.append("; ".join(problem))
        check(f"WS 收到 {frames} 帧 metrics", len(stamps) == frames)
        check("metrics 结构符合契约", not bad_frames, bad_frames[0] if bad_frames else "")
        check("ts_ms 单调递增", all(b >= a for a, b in zip(stamps, stamps[1:])), str(stamps[:5]))
        if first:
            gpu = first.get("gpu") or {}
            print(f"       帧样例：CPU {first['cpu']['usage_pct']:.1f}% · "
                  f"内存 {first['memory']['used_gb']:.1f}/{first['memory']['total_gb']:.1f} GB · "
                  f"GPU {gpu.get('usage_pct', 'n/a')}% {gpu.get('temp_c', 'n/a')}°C · "
                  f"磁盘 {len(first.get('storage', []))} 块 · DIMM {len(first.get('dimms', []))} 条")


def main() -> int:
    ap = argparse.ArgumentParser(description="vitals web 壳自检")
    ap.add_argument("--url", default="http://127.0.0.1:8787", help="服务地址")
    ap.add_argument("--frames", type=int, default=3, help="WebSocket 采几帧")
    ap.add_argument("--interval", type=float, default=0.25, help="推送周期（秒）")
    args = ap.parse_args()

    base = args.url.rstrip("/")
    base_ws = base.replace("http://", "ws://").replace("https://", "wss://")

    status, health = get_json(base, "/api/health")
    check("GET /api/health", status == 200 and isinstance(health, dict) and health.get("ok") is True,
          f"status={status} dll={health.get('dll') if health else None}")

    status, info = get_json(base, "/api/info")
    check("GET /api/info", status == 200 and isinstance(info, dict), f"status={status}")
    if info:
        src = info.get("sources") or {}
        check("sources 键齐全", SOURCE_KEYS.issubset(src.keys()),
              f"缺 {sorted(SOURCE_KEYS - set(src))}" if not SOURCE_KEYS.issubset(src.keys()) else "")
        print(f"       {info.get('cpu_name')} · {info.get('logical_cores')} 线程 · "
              f"{info.get('total_memory_gb'):.1f} GB · {info.get('gpu_name')}")
        on = [k for k, v in src.items() if v]
        print(f"       可用数据源：{', '.join(on)}")

    status, metrics = get_json(base, "/api/metrics")
    check("GET /api/metrics", status == 200 and isinstance(metrics, dict), f"status={status}")
    if metrics:
        problem = validate_metrics(metrics)
        check("HTTP metrics 结构符合契约", not problem, problem[0] if problem else "")

    try:
        asyncio.run(ws_round(base_ws, args.frames, args.interval))
    except Exception as e:  # noqa: BLE001
        check("WebSocket 往返", False, repr(e))

    print()
    if failures:
        print(f"结果：{len(failures)}/{checks} 项失败 -> {failures}")
        return 1
    print(f"结果：{checks}/{checks} 项全过")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
