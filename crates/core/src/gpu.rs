//! GPU 采集：NVML 进程内直连（首选）+ nvidia-smi 子进程解析（降级）。
//! NVML 符号手写绑定（libloading 动态加载 nvml.dll / libnvidia-ml.so.1）。
//!
//! 结构体布局全部照 `nvml.h` 逐字抄（本机把 nvml.h 拉到 `%TEMP%\nvml.h` 对照过）；
//! 每个新字段都先由 `examples/nvml_probe.rs` 在本机取到真值再写进来。
//!
//! 两个已经踩过的坑，改这里之前先读：
//! 1. `nvmlMemory_t`（v1）的字段顺序是 `{ total, free, used }`，**没有 reserved**。
//!    本项目早期把它当成 `{ total, reserved, free }`，于是 `used = total - free` 实际算出的是
//!    「剩余显存」—— 面板上 16 GB 卡空载显示 13 GB 已用的原因就在这里。现在优先用
//!    `nvmlDeviceGetMemoryInfo_v2`（显式给出 used/reserved），v1 只作兜底且直接用 `used`。
//! 2. `nvmlPcieUtilCounter_t` 是 `{ TX_BYTES = 0, RX_BYTES = 1 }`，顺序与直觉相反。
//!
//! GPU 热点与显存结温 NVML 给不出来，走 NVAPI 补充（见 `crate::nvapi`）：
//! `NvmlGpu` 在打开时顺带打开一个 `NvapiGpu`，`poll()` 里把这两个通道填进 `GpuMetrics`。

use std::ffi::{c_char, c_void, CStr};

use crate::nvapi::NvapiGpu;
use crate::schema::GpuMetrics;

const NVML_TEMPERATURE_GPU: u32 = 0; // nvmlTemperatureSensors_t
// nvmlTemperatureThresholds_t
const NVML_THRESHOLD_SHUTDOWN: u32 = 0;
const NVML_THRESHOLD_SLOWDOWN: u32 = 1;
const NVML_THRESHOLD_GPU_MAX: u32 = 3;
// nvmlClockType_t
const NVML_CLOCK_GRAPHICS: u32 = 0;
const NVML_CLOCK_MEM: u32 = 2;
// nvmlPcieUtilCounter_t
const NVML_PCIE_UTIL_TX_BYTES: u32 = 0;
const NVML_PCIE_UTIL_RX_BYTES: u32 = 1;

#[repr(C)]
struct NvmlUtilization {
    device: u32,
    memory: u32,
    enc: u32,
    dec: u32,
}

/// nvmlMemory_t = { total, free, used }（顺序如此，没有 reserved）。
#[repr(C)]
#[derive(Default)]
struct NvmlMemory {
    total: u64,
    free: u64,
    used: u64,
}

/// nvmlMemory_v2_t = { version, total, reserved, free, used }。
#[repr(C)]
#[derive(Default)]
struct NvmlMemoryV2 {
    version: u32,
    total: u64,
    reserved: u64,
    free: u64,
    used: u64,
}

/// `NVML_STRUCT_VERSION(Memory, 2)` = `sizeof(struct) | (2 << 24)`。
const MEMORY_V2_VERSION: u32 = std::mem::size_of::<NvmlMemoryV2>() as u32 | (2 << 24);

/// nvmlFanSpeedInfo_v1_t = { version, fan, speed(RPM) }。
#[repr(C)]
#[derive(Default)]
struct NvmlFanSpeedInfo {
    version: u32,
    fan: u32,
    speed: u32,
}

const FAN_SPEED_INFO_VERSION: u32 = std::mem::size_of::<NvmlFanSpeedInfo>() as u32 | (1 << 24);

/// nvmlMarginTemperature_v1_t = { version, marginTemperature }。
#[repr(C)]
#[derive(Default)]
struct NvmlMarginTemperature {
    version: u32,
    margin_temperature: i32,
}

const MARGIN_TEMPERATURE_VERSION: u32 = std::mem::size_of::<NvmlMarginTemperature>() as u32 | (1 << 24);

type NvmlReturn = u32; // 0 = NVML_SUCCESS
type FnU32 = unsafe extern "C" fn(*mut c_void, *mut u32) -> NvmlReturn;
type FnIdxU32 = unsafe extern "C" fn(*mut c_void, u32, *mut u32) -> NvmlReturn;
type FnTwoU32 = unsafe extern "C" fn(*mut c_void, *mut u32, *mut u32) -> NvmlReturn;

/// 打开即完成初始化 + 取 0 号卡句柄和名称。
/// 除 handle 与名称外，其余函数指针都是 `Option`：老驱动不一定导出全部符号。
pub struct NvmlGpu {
    _lib: libloading::Library,
    handle: *mut c_void,
    name: String,
    /// NVAPI 补充温度通道（热点 / 显存结温）。NVML 拿不到，打开失败就是 `None`。
    nvapi: Option<NvapiGpu>,
    get_util: unsafe extern "C" fn(*mut c_void, *mut NvmlUtilization) -> NvmlReturn,
    get_temp: FnIdxU32,
    get_fan: FnU32,
    get_power: Option<FnU32>,
    get_mem_v2: Option<unsafe extern "C" fn(*mut c_void, *mut NvmlMemoryV2) -> NvmlReturn>,
    get_mem: Option<unsafe extern "C" fn(*mut c_void, *mut NvmlMemory) -> NvmlReturn>,
    get_clock: Option<FnIdxU32>,
    get_max_clock: Option<FnIdxU32>,
    get_power_limit: Option<FnU32>,
    get_enforced_limit: Option<FnU32>,
    get_temp_threshold: Option<FnIdxU32>,
    get_margin_temp: Option<unsafe extern "C" fn(*mut c_void, *mut NvmlMarginTemperature) -> NvmlReturn>,
    get_num_fans: Option<FnU32>,
    get_fan_rpm: Option<unsafe extern "C" fn(*mut c_void, *mut NvmlFanSpeedInfo) -> NvmlReturn>,
    get_pcie: Option<FnIdxU32>,
    get_link_gen: Option<FnU32>,
    get_link_width: Option<FnU32>,
    get_enc: Option<FnTwoU32>,
    get_dec: Option<FnTwoU32>,
    get_throttle: Option<unsafe extern "C" fn(*mut c_void, *mut u64) -> NvmlReturn>,
}

// NVML 线程安全；句柄只是指针令牌。NvmlGpu 随 Sampler 移入采样线程。
unsafe impl Send for NvmlGpu {}
unsafe impl Sync for NvmlGpu {}

impl NvmlGpu {
    pub fn open() -> Option<NvmlGpu> {
        let lib_name = if cfg!(windows) { "nvml.dll" } else { "libnvidia-ml.so.1" };
        unsafe {
            let lib = libloading::Library::new(lib_name).ok()?;

            // 先把所有函数指针复制出来（Symbol 的借用到此为止），再移动 lib
            let init = *lib.get::<unsafe extern "C" fn() -> NvmlReturn>(b"nvmlInit_v2\0").ok()?;
            let get_count = *lib
                .get::<unsafe extern "C" fn(*mut u32) -> NvmlReturn>(b"nvmlDeviceGetCount_v2\0")
                .ok()?;
            let get_handle = *lib
                .get::<unsafe extern "C" fn(u32, *mut *mut c_void) -> NvmlReturn>(
                    b"nvmlDeviceGetHandleByIndex_v2\0",
                )
                .ok()?;
            let get_name = *lib
                .get::<unsafe extern "C" fn(*mut c_void, *mut c_char, u32) -> NvmlReturn>(
                    b"nvmlDeviceGetName\0",
                )
                .ok()?;
            let get_util = *lib
                .get::<unsafe extern "C" fn(*mut c_void, *mut NvmlUtilization) -> NvmlReturn>(
                    b"nvmlDeviceGetUtilizationRates\0",
                )
                .ok()?;
            let get_temp = *lib
                .get::<unsafe extern "C" fn(*mut c_void, u32, *mut u32) -> NvmlReturn>(
                    b"nvmlDeviceGetTemperature\0",
                )
                .ok()?;
            let get_fan = *lib
                .get::<unsafe extern "C" fn(*mut c_void, *mut u32) -> NvmlReturn>(
                    b"nvmlDeviceGetFanSpeed\0",
                )
                .ok()?;

            // 以下都可能缺失：统一用一个辅助宏收集
            macro_rules! opt {
                ($name:literal, $ty:ty) => {
                    lib.get::<$ty>(concat!($name, "\0").as_bytes()).ok().map(|s| *s)
                };
            }
            let get_power = opt!("nvmlDeviceGetPowerUsage", FnU32);
            let get_mem_v2 =
                opt!("nvmlDeviceGetMemoryInfo_v2", unsafe extern "C" fn(*mut c_void, *mut NvmlMemoryV2) -> NvmlReturn);
            let get_mem =
                opt!("nvmlDeviceGetMemoryInfo", unsafe extern "C" fn(*mut c_void, *mut NvmlMemory) -> NvmlReturn);
            let get_clock = opt!("nvmlDeviceGetClockInfo", FnIdxU32);
            let get_max_clock = opt!("nvmlDeviceGetMaxClockInfo", FnIdxU32);
            let get_enforced_limit = opt!("nvmlDeviceGetEnforcedPowerLimit", FnU32);
            let get_power_limit = opt!("nvmlDeviceGetPowerManagementLimit", FnU32);
            let get_temp_threshold = opt!("nvmlDeviceGetTemperatureThreshold", FnIdxU32);
            let get_margin_temp = opt!(
                "nvmlDeviceGetMarginTemperature",
                unsafe extern "C" fn(*mut c_void, *mut NvmlMarginTemperature) -> NvmlReturn
            );
            let get_num_fans = opt!("nvmlDeviceGetNumFans", FnU32);
            let get_fan_rpm = opt!(
                "nvmlDeviceGetFanSpeedRPM",
                unsafe extern "C" fn(*mut c_void, *mut NvmlFanSpeedInfo) -> NvmlReturn
            );
            let get_pcie = opt!("nvmlDeviceGetPcieThroughput", FnIdxU32);
            let get_link_gen = opt!("nvmlDeviceGetCurrPcieLinkGeneration", FnU32);
            let get_link_width = opt!("nvmlDeviceGetCurrPcieLinkWidth", FnU32);
            let get_enc = opt!("nvmlDeviceGetEncoderUtilization", FnTwoU32);
            let get_dec = opt!("nvmlDeviceGetDecoderUtilization", FnTwoU32);
            let get_throttle = opt!(
                "nvmlDeviceGetCurrentClocksThrottleReasons",
                unsafe extern "C" fn(*mut c_void, *mut u64) -> NvmlReturn
            );

            if init() != 0 {
                return None;
            }
            let mut count: u32 = 0;
            if get_count(&mut count) != 0 || count == 0 {
                return None;
            }
            let mut handle: *mut c_void = std::ptr::null_mut();
            if get_handle(0, &mut handle) != 0 {
                return None;
            }
            let mut buf = [0 as c_char; 128];
            if get_name(handle, buf.as_mut_ptr(), buf.len() as u32) != 0 {
                return None;
            }
            let name = CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned();
            // NVAPI 只在 Windows 且驱动带 nvapi64.dll 时可用；失败不影响 NVML 通路
            let nvapi = nvapi_open();
            Some(NvmlGpu {
                _lib: lib,
                handle,
                name,
                nvapi,
                get_util,
                get_temp,
                get_fan,
                get_power,
                get_mem_v2,
                get_mem,
                get_clock,
                get_max_clock,
                get_power_limit,
                get_enforced_limit,
                get_temp_threshold,
                get_margin_temp,
                get_num_fans,
                get_fan_rpm,
                get_pcie,
                get_link_gen,
                get_link_width,
                get_enc,
                get_dec,
                get_throttle,
            })
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn poll(&self) -> GpuMetrics {
        let mut g = GpuMetrics {
            name: self.name.clone(),
            source: "nvml".into(),
            ..Default::default()
        };
        unsafe {
            let mut util = NvmlUtilization { device: 0, memory: 0, enc: 0, dec: 0 };
            if (self.get_util)(self.handle, &mut util) == 0 {
                g.usage_pct = util.device as f32;
            }
            self.read_memory(&mut g);
            let mut t: u32 = 0;
            if (self.get_temp)(self.handle, NVML_TEMPERATURE_GPU, &mut t) == 0 {
                g.temp_c = Some(t as f32);
            }
            // 风扇：转速百分比 + 真实 RPM（多风扇取最大值）
            let mut fan: u32 = 0;
            if (self.get_fan)(self.handle, &mut fan) == 0 {
                g.fan_pct = Some(fan as f32);
            }
            self.read_fans(&mut g);
            // 功耗（mW → W）与上限（取 enforced，缺则取 management limit）
            if let Some(get_power) = self.get_power {
                let mut mw: u32 = 0;
                if get_power(self.handle, &mut mw) == 0 {
                    g.power_w = Some(mw as f32 / 1000.0);
                }
            }
            let mut limit_mw: Option<f32> = None;
            if let Some(f) = self.get_enforced_limit {
                let mut mw: u32 = 0;
                if f(self.handle, &mut mw) == 0 && mw > 0 {
                    limit_mw = Some(mw as f32 / 1000.0);
                }
            }
            if limit_mw.is_none() {
                if let Some(f) = self.get_power_limit {
                    let mut mw: u32 = 0;
                    if f(self.handle, &mut mw) == 0 && mw > 0 {
                        limit_mw = Some(mw as f32 / 1000.0);
                    }
                }
            }
            g.power_limit_w = limit_mw;
            if let (Some(p), Some(l)) = (g.power_w, limit_mw) {
                g.power_limit_pct = Some(pct(p, l));
            }
            // 温度阈值与「离降频还差多少度」
            if let Some(f) = self.get_temp_threshold {
                let read = |kind: u32| -> Option<f32> {
                    let mut v: u32 = 0;
                    if f(self.handle, kind, &mut v) == 0 {
                        Some(v as f32)
                    } else {
                        None
                    }
                };
                g.temp_shutdown_c = read(NVML_THRESHOLD_SHUTDOWN);
                g.temp_slowdown_c = read(NVML_THRESHOLD_SLOWDOWN);
                g.temp_max_c = read(NVML_THRESHOLD_GPU_MAX);
            }
            if let Some(f) = self.get_margin_temp {
                let mut m = NvmlMarginTemperature {
                    version: MARGIN_TEMPERATURE_VERSION,
                    margin_temperature: 0,
                };
                if f(self.handle, &mut m) == 0 {
                    g.temp_margin_c = Some(m.margin_temperature as f32);
                }
            }
            // 时钟
            if let Some(f) = self.get_clock {
                let read = |kind: u32| -> Option<f32> {
                    let mut v: u32 = 0;
                    if f(self.handle, kind, &mut v) == 0 {
                        Some(v as f32)
                    } else {
                        None
                    }
                };
                g.core_clock_mhz = read(NVML_CLOCK_GRAPHICS);
                g.mem_clock_mhz = read(NVML_CLOCK_MEM);
            }
            if let Some(f) = self.get_max_clock {
                let read = |kind: u32| -> Option<f32> {
                    let mut v: u32 = 0;
                    if f(self.handle, kind, &mut v) == 0 {
                        Some(v as f32)
                    } else {
                        None
                    }
                };
                g.max_core_clock_mhz = read(NVML_CLOCK_GRAPHICS);
                g.max_mem_clock_mhz = read(NVML_CLOCK_MEM);
            }
            // PCIe：吞吐（KB/s → MiB/s）+ 链路代数/宽度
            if let Some(f) = self.get_pcie {
                let mut tx: u32 = 0;
                let mut rx: u32 = 0;
                if f(self.handle, NVML_PCIE_UTIL_TX_BYTES, &mut tx) == 0 {
                    g.pcie_tx_mib_s = Some(tx as f32 / 1024.0);
                }
                if f(self.handle, NVML_PCIE_UTIL_RX_BYTES, &mut rx) == 0 {
                    g.pcie_rx_mib_s = Some(rx as f32 / 1024.0);
                }
            }
            let mut gen: Option<u32> = None;
            if let Some(f) = self.get_link_gen {
                let mut v: u32 = 0;
                if f(self.handle, &mut v) == 0 {
                    gen = Some(v);
                }
            }
            let mut width: Option<u32> = None;
            if let Some(f) = self.get_link_width {
                let mut v: u32 = 0;
                if f(self.handle, &mut v) == 0 {
                    width = Some(v);
                }
            }
            g.pcie_link = pcie_link_label(gen, width);
            // 编解码器利用率（NVENC / NVDEC）
            if let Some(f) = self.get_enc {
                let (mut v, mut _period) = (0u32, 0u32);
                if f(self.handle, &mut v, &mut _period) == 0 {
                    g.encoder_pct = Some(v as f32);
                }
            }
            if let Some(f) = self.get_dec {
                let (mut v, mut _period) = (0u32, 0u32);
                if f(self.handle, &mut v, &mut _period) == 0 {
                    g.decoder_pct = Some(v as f32);
                }
            }
            // 时钟被限制的原因（读成功但掩码为 0 = 没有受限原因）
            if let Some(f) = self.get_throttle {
                let mut mask: u64 = 0;
                if f(self.handle, &mut mask) == 0 {
                    g.throttle_reasons = Some(throttle_reason_names(mask));
                }
            }
        }
        // NVAPI 补充通道：热点与显存结温（NVML 给不出来）
        if let Some(nvapi) = &self.nvapi {
            let (hotspot, mem_junction) = nvapi.read_thermal();
            g.hotspot_c = hotspot;
            g.mem_junction_c = mem_junction;
        }
        g
    }

    /// 显存：优先 v2（used 不含驱动保留），v1 兜底（v1 的 used 含保留）。
    unsafe fn read_memory(&self, g: &mut GpuMetrics) {
        if let Some(f) = self.get_mem_v2 {
            let mut m = NvmlMemoryV2 { version: MEMORY_V2_VERSION, ..Default::default() };
            if f(self.handle, &mut m) == 0 && m.total > 0 {
                g.vram_total_mb = bytes_to_mib(m.total);
                g.vram_used_mb = bytes_to_mib(m.used);
                g.vram_free_mb = Some(bytes_to_mib(m.free));
                g.vram_reserved_mb = Some(bytes_to_mib(m.reserved));
                g.vram_usage_pct = pct(g.vram_used_mb, g.vram_total_mb);
                return;
            }
        }
        if let Some(f) = self.get_mem {
            let mut m = NvmlMemory::default();
            if f(self.handle, &mut m) == 0 && m.total > 0 {
                g.vram_total_mb = bytes_to_mib(m.total);
                // v1 的 used 字段本身就等于 total - free，直接用它而不是自己减
                g.vram_used_mb = bytes_to_mib(m.used);
                g.vram_free_mb = Some(bytes_to_mib(m.free));
                g.vram_usage_pct = pct(g.vram_used_mb, g.vram_total_mb);
            }
        }
    }

    /// 风扇数 + 每风扇 RPM（取最大值；取不到 RPM 时风扇数也仍然有用）。
    unsafe fn read_fans(&self, g: &mut GpuMetrics) {
        if let Some(f) = self.get_num_fans {
            let mut n: u32 = 0;
            if f(self.handle, &mut n) == 0 {
                g.fan_count = Some(n);
            }
        }
        if let Some(f) = self.get_fan_rpm {
            let count = g.fan_count.unwrap_or(1).min(8);
            let mut best: Option<f32> = None;
            for i in 0..count {
                let mut info = NvmlFanSpeedInfo { version: FAN_SPEED_INFO_VERSION, fan: i, speed: 0 };
                if f(self.handle, &mut info) == 0 {
                    let rpm = info.speed as f32;
                    best = Some(best.map_or(rpm, |b: f32| b.max(rpm)));
                }
            }
            g.fan_rpm = best;
        }
    }
}

/// 打开 NVAPI 补充通道（热点 / 显存结温）。
/// 非 Windows 上是 `crate::nvapi::NvapiGpu::open()` 的占位实现，恒返回 `None`。
fn nvapi_open() -> Option<NvapiGpu> {
    NvapiGpu::open()
}

/// nvidia-smi 子进程降级通路。字段顺序与 QUERY 对应。
pub fn nvidia_smi_poll() -> Option<GpuMetrics> {
    let out = std::process::Command::new("nvidia-smi")
        .arg(concat!(
            "--query-gpu=name,utilization.gpu,memory.used,memory.total,memory.free,",
            "temperature.gpu,power.draw,power.limit,fan.speed,",
            "clocks.current.graphics,clocks.current.memory,",
            "utilization.encoder,utilization.decoder,",
            "pcie.link.gen.current,pcie.link.width.current,clocks_throttle_reasons.active"
        ))
        .arg("--format=csv,noheader,nounits")
        .output()
        .ok()?;
    let line = String::from_utf8_lossy(&out.stdout);
    parse_nvidia_smi_line(line.lines().next()?)
}

fn bytes_to_mib(bytes: u64) -> f32 {
    bytes as f32 / 1024.0 / 1024.0
}

/// part / whole * 100，whole 为 0 时返回 0（而不是 NaN/Inf）。
fn pct(part: f32, whole: f32) -> f32 {
    if whole > 0.0 {
        part / whole * 100.0
    } else {
        0.0
    }
}

/// "PCIe Gen5 x16"；两半都缺就返回 None，只有一半也能出标签。
fn pcie_link_label(gen: Option<u32>, width: Option<u32>) -> Option<String> {
    match (gen, width) {
        (None, None) => None,
        (g, w) => {
            let mut s = String::from("PCIe");
            if let Some(g) = g {
                s.push_str(&format!(" Gen{g}"));
            }
            if let Some(w) = w {
                s.push_str(&format!(" x{w}"));
            }
            Some(s)
        }
    }
}

/// 时钟受限原因位掩码 → 人类可读标签（照 nvml.h 的 nvmlClocksEventReason* 位定义）。
/// 0 = 没有受限原因，返回空表。未定义的位用 `未知位(0x..)` 标出，不静默丢弃。
pub fn throttle_reason_names(mask: u64) -> Vec<String> {
    const BITS: [(u64, &str); 11] = [
        (0x0001, "GPU 空闲"),
        (0x0002, "应用时钟设置"),
        (0x0004, "软件功耗上限"),
        (0x0008, "硬件降频"),
        (0x0010, "同步加速"),
        (0x0020, "软件温度降频"),
        (0x0040, "硬件温度降频"),
        (0x0080, "硬件功率刹车"),
        (0x0100, "显示时钟设置"),
        (0x0200, "板级限制策略"),
        (0x0400, "可靠性策略"),
    ];
    let mut out = Vec::new();
    let mut known = 0u64;
    for (bit, name) in BITS {
        if mask & bit != 0 {
            out.push(name.to_string());
            known |= bit;
        }
    }
    let rest = mask & !known;
    if rest != 0 {
        out.push(format!("未知位(0x{rest:X})"));
    }
    out
}

/// `0x0000000000000004` 这类十六进制掩码（nvidia-smi 的 clocks_throttle_reasons.active 输出）。
fn decode_hex_mask(s: &str) -> Option<u64> {
    let s = s.trim();
    let s = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")).unwrap_or(s);
    u64::from_str_radix(s, 16).ok()
}

/// 纯函数解析，便于单测。字段顺序见 `nvidia_smi_poll` 的 QUERY。
fn parse_nvidia_smi_line(line: &str) -> Option<GpuMetrics> {
    let line = line.trim();
    if line.is_empty() || line.to_ascii_lowercase().contains("no devices") {
        return None;
    }
    // 首个字段是显卡名（不含逗号），后面是数值
    let mut parts = line.split(',');
    let name = parts.next()?.trim().to_string();
    let raw: Vec<&str> = parts.map(|p| p.trim()).collect();
    let vals: Vec<Option<f32>> = raw
        .iter()
        .map(|p| {
            let p = p.trim_start_matches('[').trim_end_matches(']');
            p.trim().parse::<f32>().ok().filter(|v| v.is_finite())
        })
        .collect();
    // 允许末尾缺字段（power/fan 在旧驱动上是 N/A 解析为 None）
    let get = |i: usize| vals.get(i).copied().flatten();
    let used = get(1)?;
    let total = get(2)?;
    let free = get(3);
    let mut g = GpuMetrics {
        name,
        usage_pct: get(0).unwrap_or(0.0),
        vram_used_mb: used,
        vram_total_mb: total,
        vram_free_mb: free,
        temp_c: get(4),
        power_w: get(5),
        power_limit_w: get(6),
        fan_pct: get(7),
        core_clock_mhz: get(8),
        mem_clock_mhz: get(9),
        encoder_pct: get(10),
        decoder_pct: get(11),
        pcie_link: pcie_link_label(get(12).map(|v| v as u32), get(13).map(|v| v as u32)),
        vram_usage_pct: pct(used, total),
        source: "nvidia-smi".into(),
        ..Default::default()
    };
    if let Some(l) = g.power_limit_w.filter(|l| *l > 0.0) {
        if let Some(p) = g.power_w {
            g.power_limit_pct = Some(pct(p, l));
        }
    }
    // 保留量 = total - used - free（nvidia-smi 的 used 不含驱动保留，与 NVML v2 同语义）
    if let Some(f) = free {
        let reserved = total - used - f;
        if reserved >= 0.0 {
            g.vram_reserved_mb = Some(reserved);
        }
    }
    // clocks_throttle_reasons.active 是位置 14 的十六进制掩码
    if let Some(mask) = raw.get(14).and_then(|s| decode_hex_mask(s)) {
        g.throttle_reasons = Some(throttle_reason_names(mask));
    }
    Some(g)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 本机 nvidia-smi 的真实一行（16 字段版本）。
    const SMI_LINE: &str = "NVIDIA GeForce RTX 5080, 47, 6173, 16303, 9805, 53, 158.93, 360.00, 42, \
                            2872, 15001, 0, 6, 5, 16, 0x0000000000000000";

    #[test]
    fn parse_smi_line() {
        let g = parse_nvidia_smi_line(SMI_LINE).unwrap();
        assert_eq!(g.name, "NVIDIA GeForce RTX 5080");
        assert_eq!(g.usage_pct, 47.0);
        assert_eq!(g.vram_used_mb, 6173.0);
        assert_eq!(g.vram_total_mb, 16303.0);
        assert_eq!(g.vram_free_mb, Some(9805.0));
        assert_eq!(g.temp_c, Some(53.0));
        assert_eq!(g.power_w, Some(158.93));
        assert_eq!(g.power_limit_w, Some(360.0));
        assert_eq!(g.fan_pct, Some(42.0));
        assert_eq!(g.core_clock_mhz, Some(2872.0));
        assert_eq!(g.mem_clock_mhz, Some(15001.0));
        assert_eq!(g.encoder_pct, Some(0.0));
        assert_eq!(g.decoder_pct, Some(6.0));
        assert_eq!(g.pcie_link.as_deref(), Some("PCIe Gen5 x16"));
        assert!((g.vram_usage_pct - 37.86).abs() < 0.05);
        assert!((g.power_limit_pct.unwrap() - 44.15).abs() < 0.05);
        // 16303 - 6173 - 9805 = 325 MiB 驱动保留
        assert_eq!(g.vram_reserved_mb, Some(325.0));
        assert_eq!(g.throttle_reasons, Some(vec![]));
    }

    #[test]
    fn parse_smi_line_with_na() {
        let g = parse_nvidia_smi_line(
            "RTX 5080, 79, 4576, 16303, 14200, 58, [N/A], [N/A], 35, [N/A], [N/A], [N/A], [N/A], 5, 16, [N/A]",
        )
        .unwrap();
        assert_eq!(g.power_w, None);
        assert_eq!(g.fan_pct, Some(35.0));
        assert_eq!(g.core_clock_mhz, None);
        assert_eq!(g.power_limit_pct, None);
        // 16303 - 4576 - 14200 < 0 → 保留量不可信，必须是 None 而不是负数
        assert_eq!(g.vram_reserved_mb, None);
    }

    #[test]
    fn parse_smi_short_line_tolerates_missing_tail() {
        // 老驱动的旧字段集：只有前 8 个字段
        let g = parse_nvidia_smi_line("RTX 5080, 79, 4576, 16303, 14200, 58, 320.50, 35").unwrap();
        assert_eq!(g.core_clock_mhz, None);
        assert_eq!(g.pcie_link, None);
        // 这一行没有限频字段 → 必须是 None（未知），不能退化成「无限频」
        assert_eq!(g.throttle_reasons, None);
    }

    #[test]
    fn parse_smi_empty() {
        assert!(parse_nvidia_smi_line("").is_none());
        assert!(parse_nvidia_smi_line("No devices were found").is_none());
    }

    #[test]
    fn memory_field_order_is_total_free_used() {
        // 本机 nvml_probe 实测：v1 = {total, free, used}
        let total = 17094934528u64; // 16303 MiB
        let free = 10288496640u64; // 9811 MiB
        let used = 6806437888u64; // 6491 MiB（= total - free，含驱动保留）
        assert_eq!(used, total - free);
        assert!((bytes_to_mib(total) - 16303.0).abs() < 1.0);
        assert!((bytes_to_mib(free) - 9811.0).abs() < 1.0);
        assert!((bytes_to_mib(used) - 6491.0).abs() < 1.0);
        // v2 = {total, reserved, free, used}，used 不含保留
        let reserved = 341835776u64; // 326 MiB
        let used_v2 = 6464602112u64; // 6165 MiB = total - free - reserved
        assert!((bytes_to_mib(reserved) - 326.0).abs() < 1.0);
        assert_eq!(used_v2, total - free - reserved);
        assert!((bytes_to_mib(used_v2) - 6165.0).abs() < 1.0);
        // 旧 bug 的算法（total - used）会得到「剩余显存」
        assert!((bytes_to_mib(total - used) - bytes_to_mib(free)).abs() < 1.0);
    }

    #[test]
    fn pct_guards_against_zero_total() {
        assert_eq!(pct(5.0, 0.0), 0.0);
        assert!((pct(1.0, 4.0) - 25.0).abs() < f32::EPSILON);
    }

    #[test]
    fn throttle_bits_decode() {
        assert!(throttle_reason_names(0).is_empty());
        assert_eq!(throttle_reason_names(0x1), vec!["GPU 空闲".to_string()]);
        assert_eq!(
            throttle_reason_names(0x4 | 0x40),
            vec!["软件功耗上限".to_string(), "硬件温度降频".to_string()]
        );
        assert_eq!(throttle_reason_names(0x400), vec!["可靠性策略".to_string()]);
        // 未定义的位不能静默丢掉
        let v = throttle_reason_names(0x1 | 0x8000_0000);
        assert_eq!(v.len(), 2);
        assert!(v[1].starts_with("未知位(0x"));
    }

    #[test]
    fn hex_mask_parsing() {
        assert_eq!(decode_hex_mask("0x0000000000000000"), Some(0));
        assert_eq!(decode_hex_mask("0x44"), Some(0x44));
        assert_eq!(decode_hex_mask(" 0X8 "), Some(8));
        assert_eq!(decode_hex_mask("N/A"), None);
    }

    #[test]
    fn pcie_link_label_variants() {
        assert_eq!(pcie_link_label(Some(5), Some(16)).as_deref(), Some("PCIe Gen5 x16"));
        assert_eq!(pcie_link_label(Some(4), None).as_deref(), Some("PCIe Gen4"));
        assert_eq!(pcie_link_label(None, Some(8)).as_deref(), Some("PCIe x8"));
        assert_eq!(pcie_link_label(None, None), None);
    }
}
