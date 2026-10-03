#![cfg(windows)]
//! NVAPI 探针：验证能否拿到 GPU 热点温度与显存结温。
//!
//! 为什么需要它：NVML 在本机拿不到这两项（`nvmlDeviceGetTemperature` 的 sensor 1 返回
//! INVALID_ARGUMENT、`nvmlDeviceGetMemoryTemp` 符号不存在、ThermalSettings 只有 1 路）。
//! LHM 的 `NvidiaGpu.Update()` 走的是 **NVAPI** 的 `NvAPI_GPU_GetThermalSensors`
//! （`Interop\NvApi.cs` 里 id = 0x65FE3AAD，结构体 `NvThermalSensors`），
//! 按型号取不同下标、统一 `/256.0f`：
//!   - RTX 50xx：`Temperatures[1]` = 核心温度、`Temperatures[2]` = 显存结温（热点置 0，即没有）
//!   - RTX 40xx：`[1]` = 热点、`[7]` = 显存结温
//!   - 其余：`[1]` = 热点、`[9]` = 显存结温
//! 掩码发现方式也照抄 LHM：从 bit 0 起逐个试 `1 << bit`，第一个失败的位之前全为有效。
//!
//! 输出全 ASCII。无需管理员权限（NVAPI 不是内核驱动，与 NVML 一样普通用户可用）。

use std::ffi::c_void;
use std::time::Duration;

use libloading::Library;

type QueryInterface = unsafe extern "C" fn(u32) -> *mut c_void;

// nvapi_QueryInterface 的接口 id，全部取自 LHM `Interop\NvApi.cs` 的 GetDelegate 调用
const NVAPI_INITIALIZE: u32 = 0x0150_E828;
const NVAPI_GET_INTERFACE_VERSION_STRING: u32 = 0x0105_3FA5;
const NVAPI_ENUM_PHYSICAL_GPUS: u32 = 0xE5AC_921F;
const NVAPI_GPU_GET_FULL_NAME: u32 = 0xCEEE_8E9F;
const NVAPI_GPU_GET_BUS_ID: u32 = 0x1BE0_B8E5;
const NVAPI_GPU_GET_BUS_SLOT_ID: u32 = 0x2A0A_350F;
const NVAPI_GPU_GET_PCI_IDENTIFIERS: u32 = 0x2DDF_B66E;
const NVAPI_GPU_GET_THERMAL_SETTINGS: u32 = 0xE364_0A56;
const NVAPI_GPU_GET_THERMAL_SENSORS: u32 = 0x65FE_3AAD;
const NVAPI_GPU_GET_ALL_CLOCK_FREQUENCIES: u32 = 0xDCB6_16C3;

const MAX_PHYSICAL_GPUS: usize = 64;
const MAX_THERMAL_SENSORS_PER_GPU: usize = 3;
const THERMAL_SENSOR_RESERVED_COUNT: usize = 8;
const THERMAL_SENSOR_TEMPERATURE_COUNT: usize = 32;
const MAX_GPU_PUBLIC_CLOCKS: usize = 32;

#[repr(C)]
#[derive(Clone, Copy)]
struct NvSensor {
    controller: i32,
    default_min_temp: u32,
    default_max_temp: u32,
    current_temp: u32,
    target: i32,
}

#[repr(C)]
struct NvThermalSettings {
    version: u32,
    count: u32,
    sensor: [NvSensor; MAX_THERMAL_SENSORS_PER_GPU],
}

#[repr(C)]
struct NvThermalSensors {
    version: u32,
    mask: u32,
    reserved: [i32; THERMAL_SENSOR_RESERVED_COUNT],
    temperatures: [i32; THERMAL_SENSOR_TEMPERATURE_COUNT],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct NvClockDomain {
    is_present: u32,
    frequency: u32,
}

#[repr(C)]
struct NvClockFrequencies {
    version: u32,
    clock_type: u32,
    clocks: [NvClockDomain; MAX_GPU_PUBLIC_CLOCKS],
}

/// `MAKE_NVAPI_VERSION<T>(ver)` = `sizeof(T) | (ver << 16)`
fn make_version(size: usize, ver: u32) -> u32 {
    size as u32 | (ver << 16)
}

fn status_name(code: i32) -> &'static str {
    match code {
        0 => "OK",
        -1 => "ERROR",
        -2 => "LIBRARY_NOT_FOUND",
        -3 => "NO_IMPLEMENTATION",
        -4 => "API_NOT_INITIALIZED",
        -5 => "INVALID_ARGUMENT",
        -6 => "NVIDIA_DEVICE_NOT_FOUND",
        -9 => "INCOMPATIBLE_STRUCT_VERSION",
        -11 => "INVALID_USER_PRIVILEGE",
        -12 => "HANDLE_INVALIDATED",
        -104 => "NOT_SUPPORTED",
        _ => "?",
    }
}

fn controller_name(id: i32) -> &'static str {
    match id {
        0 => "None",
        1 => "GpuInternal",
        2 => "Adm1032",
        3 => "Max6649",
        4 => "Max1617",
        5 => "Lm99",
        6 => "Lm89",
        7 => "Lm64",
        8 => "Adt7473",
        9 => "SbMax6649",
        10 => "VBiosEvt",
        11 => "OS",
        _ => "?",
    }
}

fn target_name(id: i32) -> &'static str {
    match id {
        0 => "None",
        1 => "Gpu",
        2 => "Memory",
        4 => "PowerSupply",
        8 => "Board",
        9 => "VisualComputingBoard",
        10 => "VisualComputingInlet",
        11 => "VisualComputingOutlet",
        15 => "All",
        -1 => "Unknown",
        _ => "?",
    }
}

fn clock_id_name(i: usize) -> &'static str {
    match i {
        0 => "Graphics",
        4 => "Memory",
        7 => "Processor",
        8 => "Video",
        _ => "-",
    }
}

fn main() {
    let lib = match unsafe { Library::new("nvapi64.dll") } {
        Ok(l) => l,
        Err(e) => {
            println!("[!] failed to load nvapi64.dll: {e}");
            return;
        }
    };
    println!("[1] nvapi64.dll loaded");

    let query: libloading::Symbol<QueryInterface> =
        match unsafe { lib.get(b"nvapi_QueryInterface\0") } {
            Ok(q) => q,
            Err(e) => {
                println!("[!] nvapi_QueryInterface not found: {e}");
                return;
            }
        };
    let query = *query;

    // ---- NvAPI_Initialize ----
    let init: unsafe extern "C" fn() -> i32 = match unsafe { query(NVAPI_INITIALIZE) } {
        p if p.is_null() => {
            println!("[!] NvAPI_Initialize not exported");
            return;
        }
        p => unsafe { std::mem::transmute(p) },
    };
    let rc = unsafe { init() };
    println!(
        "[1] NvAPI_Initialize -> {rc} ({})",
        status_name(rc)
    );

    if let Some(p) = unsafe { std::ptr::NonNull::new(query(NVAPI_GET_INTERFACE_VERSION_STRING)) } {
        let f: unsafe extern "C" fn(*mut u8) -> i32 = unsafe { std::mem::transmute(p.as_ptr()) };
        let mut buf = [0u8; 64];
        let rc = unsafe { f(buf.as_mut_ptr()) };
        let s = String::from_utf8_lossy(&buf).trim_end_matches('\0').to_string();
        println!("[1] interface version = {s} (rc {rc})");
    }

    // ---- 物理 GPU 枚举 ----
    let enum_gpus: unsafe extern "C" fn(*mut c_void, *mut u32) -> i32 = match unsafe {
        query(NVAPI_ENUM_PHYSICAL_GPUS)
    } {
        p if p.is_null() => {
            println!("[!] NvAPI_EnumPhysicalGPUs not exported");
            return;
        }
        p => unsafe { std::mem::transmute(p) },
    };
    let mut handles = [0usize; MAX_PHYSICAL_GPUS];
    let mut count: u32 = 0;
    let rc = unsafe {
        enum_gpus(
            handles.as_mut_ptr() as *mut c_void,
            &mut count as *mut u32,
        )
    };
    println!(
        "[2] NvAPI_EnumPhysicalGPUs -> rc {rc} ({}), count = {count}",
        status_name(rc)
    );
    if rc != 0 || count == 0 {
        println!("[!] no physical GPU");
        return;
    }
    let handle = handles[0] as *mut c_void;

    // ---- 名字 / 总线 ----
    if let Some(p) = unsafe { std::ptr::NonNull::new(query(NVAPI_GPU_GET_FULL_NAME)) } {
        let f: unsafe extern "C" fn(*mut c_void, *mut u8) -> i32 = unsafe { std::mem::transmute(p.as_ptr()) };
        let mut buf = [0u8; 64];
        let rc = unsafe { f(handle, buf.as_mut_ptr()) };
        println!(
            "[2] full name = {} (rc {rc})",
            String::from_utf8_lossy(&buf).trim_end_matches('\0')
        );
    }
    if let Some(p) = unsafe { std::ptr::NonNull::new(query(NVAPI_GPU_GET_BUS_ID)) } {
        let f: unsafe extern "C" fn(*mut c_void, *mut u32) -> i32 = unsafe { std::mem::transmute(p.as_ptr()) };
        let mut v: u32 = 0;
        let rc = unsafe { f(handle, &mut v) };
        println!("[2] bus id = {v} (rc {rc})");
    }
    if let Some(p) = unsafe { std::ptr::NonNull::new(query(NVAPI_GPU_GET_BUS_SLOT_ID)) } {
        let f: unsafe extern "C" fn(*mut c_void, *mut u32) -> i32 = unsafe { std::mem::transmute(p.as_ptr()) };
        let mut v: u32 = 0;
        let rc = unsafe { f(handle, &mut v) };
        println!("[2] bus slot id = {v} (rc {rc})");
    }
    if let Some(p) = unsafe { std::ptr::NonNull::new(query(NVAPI_GPU_GET_PCI_IDENTIFIERS)) } {
        let f: unsafe extern "C" fn(*mut c_void, *mut u32, *mut u32, *mut u32, *mut u32) -> i32 =
            unsafe { std::mem::transmute(p.as_ptr()) };
        let (mut dev, mut sub, mut rev, mut ext) = (0u32, 0u32, 0u32, 0u32);
        let rc = unsafe { f(handle, &mut dev, &mut sub, &mut rev, &mut ext) };
        println!(
            "[2] pci ids = device 0x{dev:04X} subsys 0x{sub:08X} rev 0x{rev:02X} ext 0x{ext:04X} (rc {rc})"
        );
    }

    // ---- [3] NvAPI_GPU_GetThermalSettings ----
    println!();
    println!("[3] NvAPI_GPU_GetThermalSettings (id 0xE3640A56)");
    if let Some(p) = unsafe { std::ptr::NonNull::new(query(NVAPI_GPU_GET_THERMAL_SETTINGS)) } {
        let f: unsafe extern "C" fn(*mut c_void, i32, *mut NvThermalSettings) -> i32 =
            unsafe { std::mem::transmute(p.as_ptr()) };
        let mut s = NvThermalSettings {
            version: make_version(std::mem::size_of::<NvThermalSettings>(), 2),
            count: MAX_THERMAL_SENSORS_PER_GPU as u32,
            sensor: [NvSensor {
                controller: 0,
                default_min_temp: 0,
                default_max_temp: 0,
                current_temp: 0,
                target: 0,
            }; MAX_THERMAL_SENSORS_PER_GPU],
        };
        println!(
            "    struct size = {} bytes, version word = 0x{:08X}",
            std::mem::size_of::<NvThermalSettings>(),
            s.version
        );
        let rc = unsafe { f(handle, 15, &mut s) }; // target All = 15
        println!(
            "    rc {rc} ({}), count = {}",
            status_name(rc),
            s.count
        );
        for i in 0..(s.count as usize).min(MAX_THERMAL_SENSORS_PER_GPU) {
            let d = s.sensor[i];
            println!(
                "    #{i}: controller={} ({}) target={} ({}) min={} max={} current={}",
                d.controller,
                controller_name(d.controller),
                d.target,
                target_name(d.target),
                d.default_min_temp,
                d.default_max_temp,
                d.current_temp
            );
        }
    } else {
        println!("    not exported");
    }

    // ---- [4] NvAPI_GPU_GetThermalSensors：先定版本号，再定掩码 ----
    println!();
    println!("[4] NvAPI_GPU_GetThermalSensors (id 0x65FE3AAD)");
    let sensors_fn = unsafe { query(NVAPI_GPU_GET_THERMAL_SENSORS) };
    if sensors_fn.is_null() {
        println!("    not exported");
    } else {
        let f: unsafe extern "C" fn(*mut c_void, *mut NvThermalSensors) -> i32 =
            unsafe { std::mem::transmute(sensors_fn) };
        println!(
            "    struct size = {} bytes",
            std::mem::size_of::<NvThermalSensors>()
        );

        // 版本号：NVAPI 用结构体大小 + 版本字打包，试 v1 / v2
        for ver in 1..=2u32 {
            let mut s = NvThermalSensors {
                version: make_version(std::mem::size_of::<NvThermalSensors>(), ver),
                mask: 0,
                reserved: [0; THERMAL_SENSOR_RESERVED_COUNT],
                temperatures: [0; THERMAL_SENSOR_TEMPERATURE_COUNT],
            };
            let rc = unsafe { f(handle, &mut s) };
            println!(
                "    version v{ver} (0x{:08X}) -> rc {rc} ({})",
                s.version,
                status_name(rc)
            );
        }

        // 哨兵缓冲：把整块内存填成 0xEF，看 NVAPI 实际改写多少字节、哪些字是 -1/0
        let mut raw = vec![0xEFu8; std::mem::size_of::<NvThermalSensors>() + 64];
        // 先把 version 写到开头（必须是对的版本才能读出内容）
        let v2 = make_version(std::mem::size_of::<NvThermalSensors>(), 2);
        raw[0..4].copy_from_slice(&v2.to_le_bytes());
        let rc = unsafe { f(handle, raw.as_mut_ptr() as *mut NvThermalSensors) };
        let changed = raw
            .iter()
            .enumerate()
            .filter(|(_, b)| **b != 0xEF)
            .map(|(i, _)| i)
            .collect::<Vec<_>>();
        match (changed.first(), changed.last()) {
            (Some(a), Some(b)) => println!(
                "    sentinel(0xEF) buffer: rc {rc}, written bytes [{a}..={b}] => NVAPI 至少用了 {} 字节",
                b + 1
            ),
            _ => println!("    sentinel(0xEF) buffer: rc {rc}, nothing written"),
        }

        // 掩码发现：完全照抄 LHM 的循环
        let mut mask: u32 = 0;
        let mut max_bit = 32usize;
        for bit in 0..32usize {
            let probe_mask = 1u32 << bit;
            let mut s = NvThermalSensors {
                version: v2,
                mask: probe_mask,
                reserved: [0; THERMAL_SENSOR_RESERVED_COUNT],
                temperatures: [0; THERMAL_SENSOR_TEMPERATURE_COUNT],
            };
            let rc = unsafe { f(handle, &mut s) };
            if rc == 0 {
                mask = probe_mask;
                continue;
            }
            mask = probe_mask - 1;
            max_bit = bit;
            break;
        }
        println!(
            "    mask discovery: 第一个失败的位 = bit {max_bit}, 有效掩码 = 0x{mask:08X} ({} 路)",
            mask.count_ones()
        );

        // 正式读一次，打印全部 32 路
        let mut s = NvThermalSensors {
            version: v2,
            mask,
            reserved: [0; THERMAL_SENSOR_RESERVED_COUNT],
            temperatures: [0; THERMAL_SENSOR_TEMPERATURE_COUNT],
        };
        let rc = unsafe { f(handle, &mut s) };
        println!(
            "    read -> rc {rc} ({}), mask echo = 0x{:08X}",
            status_name(rc),
            s.mask
        );
        println!("    reserved = {:?}", s.reserved);
        for i in 0..THERMAL_SENSOR_TEMPERATURE_COUNT {
            let raw_t = s.temperatures[i];
            let valid = (mask >> i) & 1 == 1;
            if raw_t == 0 && !valid {
                continue;
            }
            println!(
                "      [{i:>2}] raw = {raw_t:>10}  /256 = {:>9.2}   mask_bit = {}",
                raw_t as f32 / 256.0,
                if valid { 1 } else { 0 }
            );
        }

        // 1 秒后再读一次，看哪几路是活的遥测
        let first = s.temperatures;
        std::thread::sleep(Duration::from_millis(1000));
        let mut s2 = NvThermalSensors {
            version: v2,
            mask,
            reserved: [0; THERMAL_SENSOR_RESERVED_COUNT],
            temperatures: [0; THERMAL_SENSOR_TEMPERATURE_COUNT],
        };
        let rc2 = unsafe { f(handle, &mut s2) };
        println!("    1s 后复读 -> rc {rc2}");
        for i in 0..THERMAL_SENSOR_TEMPERATURE_COUNT {
            if first[i] != s2.temperatures[i] {
                println!(
                    "      [{i:>2}] {:.2} -> {:.2} C  (变化了，说明是活遥测)",
                    first[i] as f32 / 256.0,
                    s2.temperatures[i] as f32 / 256.0
                );
            }
        }

        // ---- [5] 与 NVML 交叉验证：LHM 说 RTX 50xx 的 [1] 就是核心温度 ----
        println!();
        println!("[5] 与 NVML 对拍（RTX 50xx: Temperatures[1]/256 应当等于核心温度）");
        match cs_core::gpu::NvmlGpu::open() {
            Some(g) => {
                let m = g.poll();
                let nvapi_1 = s2.temperatures[1] as f32 / 256.0;
                let nvapi_2 = s2.temperatures[2] as f32 / 256.0;
                match m.temp_c {
                    Some(nvml_core) => {
                        println!("    NVML   temp_c              = {nvml_core:.2} C");
                        println!("    NVAPI  Temperatures[1]/256 = {nvapi_1:.2} C");
                        println!("    NVAPI  Temperatures[2]/256 = {nvapi_2:.2} C  <- 候选「显存结温」");
                        println!(
                            "    delta([1] vs NVML) = {:.2} C",
                            (nvapi_1 - nvml_core).abs()
                        );
                    }
                    None => {
                        println!("    NVML   temp_c              = n/a");
                        println!("    NVAPI  Temperatures[1]/256 = {nvapi_1:.2} C");
                        println!("    NVAPI  Temperatures[2]/256 = {nvapi_2:.2} C  <- 候选「显存结温」");
                    }
                }
                println!(
                    "    [2] vs 核心 温差 = {:.2} C（显存结温通常高于或接近核心）",
                    nvapi_2 - nvapi_1
                );
            }
            None => println!("    [!] NVML 打不开，无法对拍"),
        }
    }

    // ---- [6] NvAPI_GPU_GetAllClockFrequencies（顺带验证 NVAPI 是通的）----
    println!();
    println!("[6] NvAPI_GPU_GetAllClockFrequencies (id 0xDCB616C3)");
    let clocks_fn = unsafe { query(NVAPI_GPU_GET_ALL_CLOCK_FREQUENCIES) };
    if clocks_fn.is_null() {
        println!("    not exported");
    } else {
        let f: unsafe extern "C" fn(*mut c_void, *mut NvClockFrequencies) -> i32 =
            unsafe { std::mem::transmute(clocks_fn) };
        for ver in 1..=2u32 {
            let mut c = NvClockFrequencies {
                version: make_version(std::mem::size_of::<NvClockFrequencies>(), ver),
                clock_type: 0,
                clocks: [NvClockDomain {
                    is_present: 0,
                    frequency: 0,
                }; MAX_GPU_PUBLIC_CLOCKS],
            };
            let rc = unsafe { f(handle, &mut c) };
            print!(
                "    v{ver} (0x{:08X}) rc {rc} ({}):",
                c.version,
                status_name(rc)
            );
            if rc == 0 {
                for i in 0..MAX_GPU_PUBLIC_CLOCKS {
                    let d = c.clocks[i];
                    if d.is_present & 1 != 0 {
                        print!(
                            " [{i}]{}{}={}MHz",
                            clock_id_name(i),
                            if clock_id_name(i) == "-" { "" } else { " " },
                            d.frequency / 1000
                        );
                    }
                }
            }
            println!();
        }
        if let Some(g) = cs_core::gpu::NvmlGpu::open() {
            let m = g.poll();
            println!(
                "    NVML 对拍: core {} MHz / mem {} MHz",
                m.core_clock_mhz.unwrap_or(-1.0),
                m.mem_clock_mhz.unwrap_or(-1.0)
            );
        }
    }

    println!();
    println!("[7] 结论：见上面各步 rc 与数值；核心结论是 [4] 的 TemperatureSensors 是否可用");
}
