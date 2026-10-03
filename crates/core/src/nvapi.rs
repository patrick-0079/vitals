#![cfg(windows)]
//! NVAPI 补充温度通道：GPU 热点（hotspot）与显存结温（memory junction）。
//!
//! 为什么需要它：NVML 在本机给不出这两项。四条证据（见 README「已知边界」）：
//! `nvmlDeviceGetTemperature(GPU_MAX=1)` 返回 `INVALID_ARGUMENT`、`nvmlDeviceGetMemoryTemp`
//! 符号不存在、`nvmlDeviceGetThermalSettings` 的 `count = 1`（只有核心温度）、
//! `nvidia-smi dmon -s p` 的 `mtemp` 列恒为 `-`。
//!
//! LHM 的 `NvidiaGpu.Update()` 走的是 NVAPI 的 `NvAPI_GPU_GetThermalSensors`
//! （`Interop\NvApi.cs` 里 id = `0x65FE3AAD`，结构体 `NvThermalSensors`），
//! 按型号取不同下标、统一 `/256.0f`：
//!   - RTX 50xx：`Temperatures[1]` = **核心温度**、`Temperatures[2]` = **显存结温**，热点没有（LHM 直接置 0）
//!   - RTX 40xx：`[1]` = 热点、`[7]` = 显存结温
//!   - 其余：  `[1]` = 热点、`[9]` = 显存结温
//!
//! 三个本机实测确认过、写代码时必须记住的点：
//! 1. **掩码不是有效性**。`NvThermalSensors.Mask` 只表示「可以请求哪些通道」，本机是
//!    `0x0007FFFF`（19 路），但其中只有下标 1、2 有真值，其余全是 `65280`（= `0xFF00`，
//!    即 255.0 °C 哨兵）。所以有效性必须看哨兵，不能看掩码位。
//! 2. **掩码发现照抄 LHM**：从 bit 0 起逐个试 `1 << bit`，第一个调用失败的位之前全为有效，
//!    最终掩码 = `(1 << 失败位) - 1`。请求不支持的位时 NVAPI 返回非 0（本机是 `-121`）。
//! 3. **结构体版本字**是 `sizeof(struct) | (ver << 16)`（`MAKE_NVAPI_VERSION`），
//!    本结构 168 字节 → `0x000200A8`；用错版本会直接报错（本机 v1 与 v2 都是 `-121`，
//!    真正能用的是 v2 + 非零掩码的组合）。
//!
//! 本机实测对拍（2025 探针一次运行内）：NVAPI `Temperatures[1]/256 = 59.97 °C`
//! ↔ NVML `temp_c = 60.00 °C`，差 **0.03 °C** —— 两条完全独立的 API 读到同一个物理传感器，
//! 这既验证了下标映射，也再次确认 LHM 对 50 系的映射是对的（`[1]` 是核心而不是热点）。

use std::ffi::c_void;
use std::time::Duration;

/// `nvapi_QueryInterface` 的接口 id，全部取自 LHM `Interop\NvApi.cs` 的 `GetDelegate` 调用。
const NVAPI_INITIALIZE: u32 = 0x0150_E828;
const NVAPI_ENUM_PHYSICAL_GPUS: u32 = 0xE5AC_921F;
const NVAPI_GPU_GET_FULL_NAME: u32 = 0xCEEE_8E9F;
const NVAPI_GPU_GET_BUS_ID: u32 = 0x1BE0_B8E5;
const NVAPI_GPU_GET_BUS_SLOT_ID: u32 = 0x2A0A_350F;
const NVAPI_GPU_GET_THERMAL_SENSORS: u32 = 0x65FE_3AAD;

const MAX_PHYSICAL_GPUS: usize = 64;
/// `NVAPI_MAX_THERMAL_SENSORS_PER_GPU`（LHM 的 `THERMAL_SENSOR_TEMPERATURE_COUNT`）。
pub const THERMAL_CHANNEL_COUNT: usize = 32;
const THERMAL_RESERVED_COUNT: usize = 8;

/// 8.8 定点里的「无效」哨兵：`0xFF00 / 256 = 255.0 °C`。本机所有空通道都返回它。
pub const INVALID_RAW: i32 = 0xFF00;

#[repr(C)]
struct NvThermalSensors {
    version: u32,
    mask: u32,
    reserved: [i32; THERMAL_RESERVED_COUNT],
    temperatures: [i32; THERMAL_CHANNEL_COUNT],
}

impl NvThermalSensors {
    fn new(mask: u32) -> Self {
        NvThermalSensors {
            version: MAKE_NVAPI_VERSION(std::mem::size_of::<NvThermalSensors>(), 2),
            mask,
            reserved: [0; THERMAL_RESERVED_COUNT],
            temperatures: [0; THERMAL_CHANNEL_COUNT],
        }
    }
}

/// `MAKE_NVAPI_VERSION<T>(ver)` = `sizeof(T) | (ver << 16)`（照抄 LHM 的同名宏）。
#[allow(non_snake_case)]
const fn MAKE_NVAPI_VERSION(size: usize, ver: u32) -> u32 {
    size as u32 | (ver << 16)
}

type QueryInterface = unsafe extern "C" fn(u32) -> *mut c_void;
type GetThermalSensors = unsafe extern "C" fn(*mut c_void, *mut NvThermalSensors) -> i32;

/// 打开即完成 `NvAPI_Initialize` + 枚举物理 GPU + 温度通道掩码发现。
pub struct NvapiGpu {
    _lib: libloading::Library,
    handle: *mut c_void,
    name: String,
    bus_id: Option<u32>,
    bus_slot_id: Option<u32>,
    get_thermal_sensors: GetThermalSensors,
    mask: u32,
}

// NVAPI 线程安全；句柄只是令牌。NvapiGpu 随 Sampler 移入采样线程。
unsafe impl Send for NvapiGpu {}
unsafe impl Sync for NvapiGpu {}

impl NvapiGpu {
    /// 失败即返回 `None`：NVAPI 是「锦上添花」的补充通道，不该让 GPU 采集整体失败。
    pub fn open() -> Option<NvapiGpu> {
        unsafe {
            let lib = libloading::Library::new("nvapi64.dll").ok()?;
            let query: QueryInterface = *lib.get::<QueryInterface>(b"nvapi_QueryInterface\0").ok()?;

            let init_ptr = query(NVAPI_INITIALIZE);
            if init_ptr.is_null() {
                return None;
            }
            let init: unsafe extern "C" fn() -> i32 = std::mem::transmute(init_ptr);
            if init() != 0 {
                return None;
            }

            let enum_ptr = query(NVAPI_ENUM_PHYSICAL_GPUS);
            if enum_ptr.is_null() {
                return None;
            }
            let enum_gpus: unsafe extern "C" fn(*mut c_void, *mut u32) -> i32 =
                std::mem::transmute(enum_ptr);
            let mut handles = [0usize; MAX_PHYSICAL_GPUS];
            let mut count: u32 = 0;
            if enum_gpus(handles.as_mut_ptr() as *mut c_void, &mut count) != 0 || count != 1 {
                // 见模块顶部说明：多 GPU 时不做 bus id 配对，宁可不报也不报错值
                return None;
            }
            let handle = handles[0] as *mut c_void;

            let name = read_full_name(&query, handle).unwrap_or_default();

            let bus_id = read_out_u32(&query, NVAPI_GPU_GET_BUS_ID, handle);
            let bus_slot_id = read_out_u32(&query, NVAPI_GPU_GET_BUS_SLOT_ID, handle);

            let ptr = query(NVAPI_GPU_GET_THERMAL_SENSORS);
            if ptr.is_null() {
                return None;
            }
            let get_thermal_sensors: GetThermalSensors = std::mem::transmute(ptr);

            // 掩码发现：照抄 LHM 的循环
            let mut mask: u32 = 0;
            for bit in 0..32u32 {
                let probe = 1u32 << bit;
                let mut s = NvThermalSensors::new(probe);
                if get_thermal_sensors(handle, &mut s) == 0 {
                    mask = probe;
                    continue;
                }
                mask = probe - 1;
                break;
            }

            Some(NvapiGpu {
                _lib: lib,
                handle,
                name,
                bus_id,
                bus_slot_id,
                get_thermal_sensors,
                mask,
            })
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// 可请求的通道掩码（`0` = 一个通道都没有）。注意它不是有效性，见模块顶部说明。
    pub fn mask(&self) -> u32 {
        self.mask
    }

    pub fn has_thermal_sensors(&self) -> bool {
        self.mask != 0
    }

    /// PCI bus / device（LHM 用它喂 PawnIO 的 `Nvidia.bin` 模块；本项目目前只做记录）。
    pub fn pci_bus_device(&self) -> (Option<u32>, Option<u32>) {
        (self.bus_id, self.bus_slot_id)
    }

    /// 原始 32 路（8.8 定点）。`None` = 调用失败。
    pub fn read_raw(&self) -> Option<[i32; THERMAL_CHANNEL_COUNT]> {
        if self.mask == 0 {
            return None;
        }
        let mut s = NvThermalSensors::new(self.mask);
        let rc = unsafe { (self.get_thermal_sensors)(self.handle, &mut s) };
        if rc != 0 {
            return None;
        }
        Some(s.temperatures)
    }

    /// 按型号映射取 (热点, 显存结温)，单位 °C。取不到就是 `None`。
    pub fn read_thermal(&self) -> (Option<f32>, Option<f32>) {
        let (hotspot_idx, mem_idx) = model_channels(&self.name);
        let raw = match self.read_raw() {
            Some(r) => r,
            None => return (None, None),
        };
        (
            hotspot_idx.and_then(|i| decode_channel(raw[i])),
            mem_idx.and_then(|i| decode_channel(raw[i])),
        )
    }
}

/// 按型号给出（热点下标，显存结温下标）；热点为 `None` 表示该型号没有独立热点通道。
/// 映射逐字来自 LHM `NvidiaGpu.Update()` 的三个分支。
pub fn model_channels(name: &str) -> (Option<usize>, Option<usize>) {
    let n = name.to_ascii_uppercase();
    if n.starts_with("NVIDIA GEFORCE RTX 50") {
        (None, Some(2))
    } else if n.starts_with("NVIDIA GEFORCE RTX 40") {
        (Some(1), Some(7))
    } else {
        (Some(1), Some(9))
    }
}

/// 8.8 定点 → °C。哨兵 `0xFF00`（255.0 °C）与非正值都当「没有读数」。
pub fn decode_channel(raw: i32) -> Option<f32> {
    if raw == INVALID_RAW || raw <= 0 {
        return None;
    }
    Some(raw as f32 / 256.0)
}

/// RTX 50 系上 `Temperatures[1]` 是**核心温度**而不是热点 —— 用它给 NVML 的核心温度做交叉校验。
pub fn is_50_series_core_channel(name: &str) -> bool {
    name.to_ascii_uppercase().starts_with("NVIDIA GEFORCE RTX 50")
}

unsafe fn read_full_name(query: &QueryInterface, handle: *mut c_void) -> Option<String> {
    let ptr = query(NVAPI_GPU_GET_FULL_NAME);
    if ptr.is_null() {
        return None;
    }
    let f: unsafe extern "C" fn(*mut c_void, *mut u8) -> i32 = std::mem::transmute(ptr);
    let mut buf = [0u8; 64];
    if f(handle, buf.as_mut_ptr()) != 0 {
        return None;
    }
    let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
    Some(String::from_utf8_lossy(&buf[..end]).to_string())
}

unsafe fn read_out_u32(query: &QueryInterface, id: u32, handle: *mut c_void) -> Option<u32> {
    let ptr = query(id);
    if ptr.is_null() {
        return None;
    }
    let f: unsafe extern "C" fn(*mut c_void, *mut u32) -> i32 = std::mem::transmute(ptr);
    let mut v: u32 = 0;
    if f(handle, &mut v) == 0 {
        Some(v)
    } else {
        None
    }
}

/// 一个「读两次取稳定值」的小工具：NVAPI 在首次调用时偶尔返回空数据。
pub fn read_thermal_stable(gpu: &NvapiGpu) -> (Option<f32>, Option<f32>) {
    let first = gpu.read_thermal();
    if first.0.is_some() || first.1.is_some() {
        return first;
    }
    std::thread::sleep(Duration::from_millis(10));
    gpu.read_thermal()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_channels_matches_lhm() {
        // RTX 50 系：没有独立热点，[2] 是显存结温（[1] 是核心温度）
        assert_eq!(
            model_channels("NVIDIA GeForce RTX 5080"),
            (None, Some(2))
        );
        // 大小写不敏感
        assert_eq!(
            model_channels("nvidia geforce rtx 5090"),
            (None, Some(2))
        );
        // RTX 40 系：热点 [1]、显存结温 [7]
        assert_eq!(
            model_channels("NVIDIA GeForce RTX 4090"),
            (Some(1), Some(7))
        );
        // 其余：热点 [1]、显存结温 [9]
        assert_eq!(
            model_channels("NVIDIA GeForce RTX 3090"),
            (Some(1), Some(9))
        );
        assert_eq!(model_channels("NVIDIA GeForce GTX 1080"), (Some(1), Some(9)));
    }

    #[test]
    fn decode_channel_uses_255_sentinel() {
        // 本机空通道的真实原始值
        assert_eq!(decode_channel(65280), None);
        // 探针实测：核心 59.97、显存结温 66.00
        assert_eq!(decode_channel(15351), Some(59.964844));
        assert_eq!(decode_channel(16896), Some(66.0));
        // 0 与负数都当没有读数
        assert_eq!(decode_channel(0), None);
        assert_eq!(decode_channel(-1), None);
    }

    #[test]
    fn struct_version_word_is_size_and_version() {
        // 168 | (2 << 16)
        assert_eq!(std::mem::size_of::<NvThermalSensors>(), 168);
        assert_eq!(
            MAKE_NVAPI_VERSION(std::mem::size_of::<NvThermalSensors>(), 2),
            0x0002_00A8
        );
    }

    #[test]
    fn mask_discovery_rule() {
        // 照抄 LHM：第一个失败的位为 bit 19 → 掩码 = (1 << 19) - 1 = 0x7FFFF（本机实测值）
        let bit = 19u32;
        assert_eq!((1u32 << bit) - 1, 0x0007_FFFF);
        // bit 0 就失败 → 掩码 0，即没有通道
        assert_eq!((1u32 << 0u32) - 1, 0);
    }

    #[test]
    fn fifty_series_core_channel_detection() {
        assert!(is_50_series_core_channel("NVIDIA GeForce RTX 5080"));
        assert!(!is_50_series_core_channel("NVIDIA GeForce RTX 4090"));
    }
}
