//! GPU 采集：NVML 进程内直连（首选）+ nvidia-smi 子进程解析（降级）。
//! NVML 符号手写绑定（libloading 动态加载 nvml.dll / libnvidia-ml.so.1）。

use std::ffi::{c_char, c_void, CStr};

use crate::schema::GpuMetrics;

const NVML_TEMPERATURE_GPU: u32 = 0; // sensor type: GPU core

#[repr(C)]
struct NvmlUtilization {
    device: u32,
    memory: u32,
    enc: u32,
    dec: u32,
}

#[repr(C)]
struct NvmlMemory {
    total: u64,
    reserved: u64,
    free: u64,
}

type NvmlReturn = u32; // 0 = NVML_SUCCESS

/// 打开即完成初始化 + 取 0 号卡句柄和名称。
pub struct NvmlGpu {
    _lib: libloading::Library,
    handle: *mut c_void,
    name: String,
    get_util: unsafe extern "C" fn(*mut c_void, *mut NvmlUtilization) -> NvmlReturn,
    get_mem: unsafe extern "C" fn(*mut c_void, *mut NvmlMemory) -> NvmlReturn,
    get_temp: unsafe extern "C" fn(*mut c_void, u32, *mut u32) -> NvmlReturn,
    get_fan: unsafe extern "C" fn(*mut c_void, *mut u32) -> NvmlReturn,
    // 老驱动可能没有功耗查询 → Option
    get_power: Option<unsafe extern "C" fn(*mut c_void, *mut u32) -> NvmlReturn>,
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
            let init = *lib
                .get::<unsafe extern "C" fn() -> NvmlReturn>(b"nvmlInit_v2\0")
                .ok()?;
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
            let get_mem = *lib
                .get::<unsafe extern "C" fn(*mut c_void, *mut NvmlMemory) -> NvmlReturn>(
                    b"nvmlDeviceGetMemoryInfo\0",
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
            // 功耗：老驱动可能缺失
            let get_power = lib
                .get::<unsafe extern "C" fn(*mut c_void, *mut u32) -> NvmlReturn>(
                    b"nvmlDeviceGetPowerUsage\0",
                )
                .ok()
                .map(|s| *s);

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
            Some(NvmlGpu {
                _lib: lib,
                handle,
                name,
                get_util,
                get_mem,
                get_temp,
                get_fan,
                get_power,
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
            let mut mem = NvmlMemory { total: 0, reserved: 0, free: 0 };
            if (self.get_mem)(self.handle, &mut mem) == 0 {
                g.vram_total_mb = (mem.total / 1024 / 1024) as f32;
                g.vram_used_mb = (mem.total.saturating_sub(mem.free) / 1024 / 1024) as f32;
                if mem.total > 0 {
                    g.vram_usage_pct = g.vram_used_mb / g.vram_total_mb * 100.0;
                }
            }
            let mut t: u32 = 0;
            if (self.get_temp)(self.handle, NVML_TEMPERATURE_GPU, &mut t) == 0 {
                g.temp_c = Some(t as f32);
            }
            // 风扇
            let mut fan: u32 = 0;
            if (self.get_fan)(self.handle, &mut fan) == 0 {
                g.fan_pct = Some(fan as f32);
            }
            // 功耗（mW → W）
            if let Some(get_power) = self.get_power {
                let mut mw: u32 = 0;
                if get_power(self.handle, &mut mw) == 0 {
                    g.power_w = Some(mw as f32 / 1000.0);
                }
            }
        }
        g
    }
}

/// nvidia-smi 子进程降级通路。字段顺序与 QUERY 对应。
pub fn nvidia_smi_poll() -> Option<GpuMetrics> {
    let out = std::process::Command::new("nvidia-smi")
        .arg("--query-gpu=name,utilization.gpu,memory.used,memory.total,temperature.gpu,power.draw,fan.speed")
        .arg("--format=csv,noheader,nounits")
        .output()
        .ok()?;
    let line = String::from_utf8_lossy(&out.stdout);
    parse_nvidia_smi_line(line.lines().next()?)
}

/// 纯函数解析，便于单测。字段：name, util%, mem used MB, mem total MB, temp℃, power W, fan%
fn parse_nvidia_smi_line(line: &str) -> Option<GpuMetrics> {
    let line = line.trim();
    if line.is_empty() || line.to_ascii_lowercase().contains("no devices") {
        return None;
    }
    // 首个字段是显卡名（不含逗号），后面是数值
    let mut parts = line.split(',');
    let name = parts.next()?.trim().to_string();
    let vals: Vec<Option<f32>> = parts
        .map(|p| {
            let p = p.trim().trim_start_matches('[').trim_end_matches(']');
            p.trim().parse::<f32>().ok().filter(|v| v.is_finite())
        })
        .collect();
    // 允许末尾缺字段（power/fan 在旧驱动上是 N/A 解析为 None）
    let get = |i: usize| vals.get(i).copied().flatten();
    let used = get(1)?;
    let total = get(2)?;
    let mut g = GpuMetrics {
        name,
        usage_pct: get(0).unwrap_or(0.0),
        vram_used_mb: used,
        vram_total_mb: total,
        temp_c: get(3),
        power_w: get(4),
        fan_pct: get(5),
        source: "nvidia-smi".into(),
        ..Default::default()
    };
    if total > 0.0 {
        g.vram_usage_pct = used / total * 100.0;
    }
    Some(g)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_smi_line() {
        let g = parse_nvidia_smi_line(
            "NVIDIA GeForce RTX 5080, 79, 4576, 16303, 58, 320.50, 35 ",
        )
        .unwrap();
        assert_eq!(g.name, "NVIDIA GeForce RTX 5080");
        assert_eq!(g.usage_pct, 79.0);
        assert_eq!(g.vram_used_mb, 4576.0);
        assert_eq!(g.vram_total_mb, 16303.0);
        assert_eq!(g.temp_c, Some(58.0));
        assert_eq!(g.power_w, Some(320.5));
        assert_eq!(g.fan_pct, Some(35.0));
        assert!((g.vram_usage_pct - 28.07).abs() < 0.1);
    }

    #[test]
    fn parse_smi_line_with_na() {
        let g = parse_nvidia_smi_line("RTX 5080, 79, 4576, 16303, 58, [N/A], 35")
            .unwrap();
        assert_eq!(g.power_w, None);
        assert_eq!(g.fan_pct, Some(35.0));
    }

    #[test]
    fn parse_smi_empty() {
        assert!(parse_nvidia_smi_line("").is_none());
        assert!(parse_nvidia_smi_line("No devices were found").is_none());
    }
}
