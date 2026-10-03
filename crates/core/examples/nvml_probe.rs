//! NVML 能力探针：列出 nvml.dll 导出的可选用符号，并逐个取真值。
//!
//! 目的：在把新传感器写进 gpu.rs 之前，先在本机确认「这个符号存在吗、返回成功吗、
//! 值的量级合理吗」。NVML 不需要管理员权限，所以这个探针可以直接跑。
//!
//! 输出全 ASCII，便于重定向到文件后阅读。

use std::ffi::{c_char, c_void, CStr};

const NVML_SUCCESS: u32 = 0;

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct NvmlUtilization {
    device: u32,
    memory: u32,
    enc: u32,
    dec: u32,
}

/// nvmlMemory_t（v1）= { total, free, used }，**没有 reserved 字段**。
/// 早期版本的本探针把第 2 个字段当成 reserved、第 3 个当成 free，导致 gpu.rs 里
/// `used = total - free` 实际算出来的是「剩余显存」—— 这个顺序就是那个 bug 的根。
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct NvmlMemory {
    total: u64,
    free: u64,
    used: u64,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct NvmlMemoryV2 {
    version: u32,
    total: u64,
    reserved: u64,
    free: u64,
    used: u64,
}

/// nvmlGpuThermalSettings_t（照 nvml.h 逐字）：
///     unsigned int count;
///     struct { nvmlThermalController_t controller; int defaultMinTemp; int defaultMaxTemp;
///              int currentTemp; nvmlThermalTarget_t target; } sensor[3];
/// 注意**没有 sensorType 字段** —— 早期版本的本探针多写了一个字段，导致读数整体错位
/// （那时把 defaultMaxTemp 当成 currentTemp 读成 57）。结构体总长 = 4 + 3*20 = 64 字节。
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct NvmlThermalSensor {
    controller: i32,
    default_min_temp: i32,
    default_max_temp: i32,
    current_temp: i32,
    target: i32,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct NvmlGpuThermalSettings {
    count: u32,
    sensors: [NvmlThermalSensor; 3],
}

/// nvmlMarginTemperature_v1_t = { version, marginTemperature }
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct NvmlMarginTemperature {
    version: u32,
    margin_temperature: i32,
}

/// nvmlFanSpeedInfo_v1_t = { version, fan, speed(RPM) }
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct NvmlFanSpeedInfo {
    version: u32,
    fan: u32,
    speed: u32,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct NvmlPciInfo {
    bus_id: [c_char; 32],
    domain: u32,
    bus: u32,
    device: u32,
    pci_device_id: u32,
    pci_sub_system_id: u32,
    pci_bus_id: u32,
    pci_sub_system_id_legacy: u32,
}

fn main() {
    let lib_name = if cfg!(windows) { "nvml.dll" } else { "libnvidia-ml.so.1" };
    let lib = match unsafe { libloading::Library::new(lib_name) } {
        Ok(l) => l,
        Err(e) => {
            println!("[!] cannot load {lib_name}: {e}");
            return;
        }
    };
    println!("[1] loaded {lib_name}");

    unsafe {
        // ---- 系统级 ----
        if let Some(f) = sym::<unsafe extern "C" fn(*mut c_char, u32) -> u32>(&lib, b"nvmlSystemGetDriverVersion\0") {
            let mut buf = [0 as c_char; 80];
            if f(buf.as_mut_ptr(), buf.len() as u32) == NVML_SUCCESS {
                println!("    driver version = {}", cstr(&buf));
            }
        }
        if let Some(f) = sym::<unsafe extern "C" fn(*mut c_char, u32) -> u32>(&lib, b"nvmlSystemGetNVMLVersion\0") {
            let mut buf = [0 as c_char; 80];
            if f(buf.as_mut_ptr(), buf.len() as u32) == NVML_SUCCESS {
                println!("    nvml version   = {}", cstr(&buf));
            }
        }
        let init = match sym::<unsafe extern "C" fn() -> u32>(&lib, b"nvmlInit_v2\0") {
            Some(f) => f,
            None => {
                println!("[!] nvmlInit_v2 missing");
                return;
            }
        };
        let rc = init();
        if rc != NVML_SUCCESS {
            println!("[!] nvmlInit_v2 -> {rc}");
            return;
        }
        let get_count = sym::<unsafe extern "C" fn(*mut u32) -> u32>(&lib, b"nvmlDeviceGetCount_v2\0").unwrap();
        let mut count = 0u32;
        get_count(&mut count);
        let get_handle =
            sym::<unsafe extern "C" fn(u32, *mut *mut c_void) -> u32>(&lib, b"nvmlDeviceGetHandleByIndex_v2\0").unwrap();
        let mut dev: *mut c_void = std::ptr::null_mut();
        if get_handle(0, &mut dev) != NVML_SUCCESS {
            println!("[!] no device 0");
            return;
        }
        if let Some(f) = sym::<unsafe extern "C" fn(*mut c_void, *mut c_char, u32) -> u32>(&lib, b"nvmlDeviceGetName\0") {
            let mut buf = [0 as c_char; 96];
            if f(dev, buf.as_mut_ptr(), buf.len() as u32) == NVML_SUCCESS {
                println!("[2] device 0 = {}   (count={})", cstr(&buf), count);
            }
        }

        println!();
        println!("[3] optional symbol availability + live values");
        println!("    {:<52} {}", "symbol", "value / result");

        // ---- 时钟 ----
        let get_clock = sym::<unsafe extern "C" fn(*mut c_void, u32, *mut u32) -> u32>(&lib, b"nvmlDeviceGetClockInfo\0");
        report_bool("nvmlDeviceGetClockInfo", get_clock.is_some());
        if let Some(f) = get_clock {
            for (id, name) in [(0u32, "GRAPHICS"), (1, "SM"), (2, "MEM"), (3, "VIDEO")] {
                let mut v = 0u32;
                let rc = f(dev, id, &mut v);
                println!("      clock[{name:<8}] rc={rc:<3} value={v} MHz");
            }
        }
        let get_max_clock =
            sym::<unsafe extern "C" fn(*mut c_void, u32, *mut u32) -> u32>(&lib, b"nvmlDeviceGetMaxClockInfo\0");
        report_bool("nvmlDeviceGetMaxClockInfo", get_max_clock.is_some());
        if let Some(f) = get_max_clock {
            for (id, name) in [(0u32, "GRAPHICS"), (1, "SM"), (2, "MEM"), (3, "VIDEO")] {
                let mut v = 0u32;
                let rc = f(dev, id, &mut v);
                println!("      max clock[{name:<8}] rc={rc:<3} value={v} MHz");
            }
        }

        // ---- 功耗 ----
        let get_power = sym::<unsafe extern "C" fn(*mut c_void, *mut u32) -> u32>(&lib, b"nvmlDeviceGetPowerUsage\0");
        report_bool("nvmlDeviceGetPowerUsage", get_power.is_some());
        if let Some(f) = get_power {
            let mut v = 0u32;
            println!("      power usage            rc={} value={} mW", f(dev, &mut v), v);
        }
        let get_limit = sym::<unsafe extern "C" fn(*mut c_void, *mut u32) -> u32>(
            &lib,
            b"nvmlDeviceGetPowerManagementLimit\0",
        );
        report_bool("nvmlDeviceGetPowerManagementLimit", get_limit.is_some());
        if let Some(f) = get_limit {
            let mut v = 0u32;
            println!("      power limit            rc={} value={} mW", f(dev, &mut v), v);
        }
        let get_enforced = sym::<unsafe extern "C" fn(*mut c_void, *mut u32) -> u32>(
            &lib,
            b"nvmlDeviceGetEnforcedPowerLimit\0",
        );
        report_bool("nvmlDeviceGetEnforcedPowerLimit", get_enforced.is_some());
        if let Some(f) = get_enforced {
            let mut v = 0u32;
            println!("      enforced power limit   rc={} value={} mW", f(dev, &mut v), v);
        }
        let get_default = sym::<unsafe extern "C" fn(*mut c_void, *mut u32) -> u32>(
            &lib,
            b"nvmlDeviceGetPowerManagementDefaultLimit\0",
        );
        report_bool("nvmlDeviceGetPowerManagementDefaultLimit", get_default.is_some());
        if let Some(f) = get_default {
            let mut v = 0u32;
            println!("      default power limit    rc={} value={} mW", f(dev, &mut v), v);
        }
        let get_range = sym::<unsafe extern "C" fn(*mut c_void, *mut u32, *mut u32) -> u32>(
            &lib,
            b"nvmlDeviceGetPowerManagementLimitConstraints\0",
        );
        report_bool("nvmlDeviceGetPowerManagementLimitConstraints", get_range.is_some());
        if let Some(f) = get_range {
            let (mut lo, mut hi) = (0u32, 0u32);
            let rc = f(dev, &mut lo, &mut hi);
            println!("      power limit range      rc={rc} min={lo} max={hi} mW");
        }
        let get_energy =
            sym::<unsafe extern "C" fn(*mut c_void, *mut u64) -> u32>(&lib, b"nvmlDeviceGetTotalEnergyConsumption\0");
        report_bool("nvmlDeviceGetTotalEnergyConsumption", get_energy.is_some());

        // ---- 温度 ----
        let get_temp = sym::<unsafe extern "C" fn(*mut c_void, u32, *mut u32) -> u32>(&lib, b"nvmlDeviceGetTemperature\0");
        report_bool("nvmlDeviceGetTemperature", get_temp.is_some());
        if let Some(f) = get_temp {
            // nvmlTemperatureSensors_t: 0 = GPU die, 1 = GPU_MAX（die 最热点）
            for (id, name) in [(0u32, "GPU"), (1, "GPU_MAX"), (2, "COUNT")] {
                let mut v = 0u32;
                let rc = f(dev, id, &mut v);
                println!("      temperature[{name:<8}]   rc={rc:<3} value={v} C");
            }
        }
        let get_thresh =
            sym::<unsafe extern "C" fn(*mut c_void, u32, *mut u32) -> u32>(&lib, b"nvmlDeviceGetTemperatureThreshold\0");
        report_bool("nvmlDeviceGetTemperatureThreshold", get_thresh.is_some());
        if let Some(f) = get_thresh {
            for (id, name) in [(0u32, "SHUTDOWN"), (1, "SLOWDOWN"), (2, "MEM_MAX"), (3, "GPU_MAX"), (4, "ACOUSTIC")] {
                let mut v = 0u32;
                let rc = f(dev, id, &mut v);
                println!("      threshold[{name:<8}]    rc={rc:<3} value={v} C");
            }
        }
        let get_thermal =
            sym::<unsafe extern "C" fn(*mut c_void, u32, *mut NvmlGpuThermalSettings) -> u32>(
                &lib,
                b"nvmlDeviceGetThermalSettings\0",
            );
        report_bool("nvmlDeviceGetThermalSettings", get_thermal.is_some());
        if let Some(f) = get_thermal {
            let mut s = NvmlGpuThermalSettings::default();
            let rc = f(dev, 0, &mut s);
            println!("      thermal settings       rc={rc} count={}", s.count);
            for i in 0..(s.count.min(3) as usize) {
                let t = s.sensors[i];
                println!(
                    "        sensor[{i}] controller={} min={} max={} current={} target={}",
                    t.controller, t.default_min_temp, t.default_max_temp, t.current_temp, t.target
                );
            }
        }
        // margin temperature = 「离最近一个降频阈值还差多少度」
        let get_margin = sym::<unsafe extern "C" fn(*mut c_void, *mut NvmlMarginTemperature) -> u32>(
            &lib,
            b"nvmlDeviceGetMarginTemperature\0",
        );
        report_bool("nvmlDeviceGetMarginTemperature", get_margin.is_some());
        if let Some(f) = get_margin {
            let mut m = NvmlMarginTemperature {
                version: std::mem::size_of::<NvmlMarginTemperature>() as u32 | (1 << 24),
                margin_temperature: 0,
            };
            let rc = f(dev, &mut m);
            println!("      margin temperature     rc={rc} value={} C", m.margin_temperature);
        }
        // 显存结温：NVML 只在部分卡上提供（本机 ABSENT，nvidia-smi 的 mtemp 列同样是 "-"）
        report_bool(
            "nvmlDeviceGetMemoryTemp",
            lib.get::<unsafe extern "C" fn(*mut c_void, *mut u32) -> u32>(b"nvmlDeviceGetMemoryTemp\0")
                .is_ok(),
        );

        // ---- 风扇 ----
        let get_fan = sym::<unsafe extern "C" fn(*mut c_void, *mut u32) -> u32>(&lib, b"nvmlDeviceGetFanSpeed\0");
        report_bool("nvmlDeviceGetFanSpeed", get_fan.is_some());
        if let Some(f) = get_fan {
            let mut v = 0u32;
            println!("      fan speed              rc={} value={} %", f(dev, &mut v), v);
        }
        let get_nfans = sym::<unsafe extern "C" fn(*mut c_void, *mut u32) -> u32>(&lib, b"nvmlDeviceGetNumFans\0");
        report_bool("nvmlDeviceGetNumFans", get_nfans.is_some());
        if let Some(f) = get_nfans {
            let mut n = 0u32;
            let rc = f(dev, &mut n);
            println!("      num fans               rc={rc} value={n}");
            if let Some(g) =
                sym::<unsafe extern "C" fn(*mut c_void, u32, *mut u32) -> u32>(&lib, b"nvmlDeviceGetFanSpeed_v2\0")
            {
                report_bool("nvmlDeviceGetFanSpeed_v2", true);
                for i in 0..n.min(4) {
                    let mut v = 0u32;
                    let rc = g(dev, i, &mut v);
                    println!("        fan[{i}] v2            rc={rc} value={v} %");
                }
            } else {
                report_bool("nvmlDeviceGetFanSpeed_v2", false);
            }
            // 真实转速（RPM）
            let get_rpm = sym::<unsafe extern "C" fn(*mut c_void, *mut NvmlFanSpeedInfo) -> u32>(
                &lib,
                b"nvmlDeviceGetFanSpeedRPM\0",
            );
            report_bool("nvmlDeviceGetFanSpeedRPM", get_rpm.is_some());
            if let Some(g) = get_rpm {
                for i in 0..n.min(4) {
                    let mut info = NvmlFanSpeedInfo {
                        version: std::mem::size_of::<NvmlFanSpeedInfo>() as u32 | (1 << 24),
                        fan: i,
                        speed: 0,
                    };
                    let rc = g(dev, &mut info);
                    println!("        fan[{i}] rpm           rc={rc} value={} RPM", info.speed);
                }
            }
        }

        // ---- PCIe ----
        let get_pcie =
            sym::<unsafe extern "C" fn(*mut c_void, u32, *mut u32) -> u32>(&lib, b"nvmlDeviceGetPcieThroughput\0");
        // nvmlPcieUtilCounter_t: 0 = TX_BYTES, 1 = RX_BYTES（顺序容易记反，照 nvml.h 抄）
        report_bool("nvmlDeviceGetPcieThroughput", get_pcie.is_some());
        if let Some(f) = get_pcie {
            for (id, name) in [(0u32, "TX"), (1, "RX")] {
                let mut v = 0u32;
                let rc = f(dev, id, &mut v);
                println!("      pcie throughput[{name}]   rc={rc:<3} value={v} KB/s");
            }
        }
        for name in ["nvmlDeviceGetCurrPcieLinkGeneration", "nvmlDeviceGetCurrPcieLinkWidth"] {
            let mut bytes = name.as_bytes().to_vec();
            bytes.push(0);
            let f = lib.get::<unsafe extern "C" fn(*mut c_void, *mut u32) -> u32>(&bytes).ok().map(|s| *s);
            report_bool(name, f.is_some());
            if let Some(f) = f {
                let mut v = 0u32;
                let rc = f(dev, &mut v);
                println!("        value                rc={rc} value={v}");
            }
        }
        let get_replay = sym::<unsafe extern "C" fn(*mut c_void, *mut u64) -> u32>(&lib, b"nvmlDeviceGetPcieReplayCounter\0");
        report_bool("nvmlDeviceGetPcieReplayCounter", get_replay.is_some());
        if let Some(f) = get_replay {
            let mut v = 0u64;
            let rc = f(dev, &mut v);
            println!("      pcie replay counter    rc={rc} value={v}");
        }

        // ---- 利用率 / 编解码 ----
        let get_util =
            sym::<unsafe extern "C" fn(*mut c_void, *mut NvmlUtilization) -> u32>(&lib, b"nvmlDeviceGetUtilizationRates\0");
        report_bool("nvmlDeviceGetUtilizationRates", get_util.is_some());
        if let Some(f) = get_util {
            let mut u = NvmlUtilization::default();
            let rc = f(dev, &mut u);
            println!("      utilization            rc={rc} gpu={} mem={} enc={} dec={}", u.device, u.memory, u.enc, u.dec);
        }
        let get_enc = sym::<unsafe extern "C" fn(*mut c_void, *mut u32, *mut u32) -> u32>(&lib, b"nvmlDeviceGetEncoderUtilization\0");
        report_bool("nvmlDeviceGetEncoderUtilization", get_enc.is_some());
        if let Some(f) = get_enc {
            let (mut v, mut period) = (0u32, 0u32);
            let rc = f(dev, &mut v, &mut period);
            println!("      encoder                rc={rc} value={v} % period={period} ms");
        }
        let get_dec = sym::<unsafe extern "C" fn(*mut c_void, *mut u32, *mut u32) -> u32>(&lib, b"nvmlDeviceGetDecoderUtilization\0");
        report_bool("nvmlDeviceGetDecoderUtilization", get_dec.is_some());
        if let Some(f) = get_dec {
            let (mut v, mut period) = (0u32, 0u32);
            let rc = f(dev, &mut v, &mut period);
            println!("      decoder                rc={rc} value={v} % period={period} ms");
        }

        // ---- 显存 ----
        let get_mem = sym::<unsafe extern "C" fn(*mut c_void, *mut NvmlMemory) -> u32>(&lib, b"nvmlDeviceGetMemoryInfo\0");
        report_bool("nvmlDeviceGetMemoryInfo", get_mem.is_some());
        let get_mem_v2 =
            sym::<unsafe extern "C" fn(*mut c_void, *mut NvmlMemoryV2) -> u32>(&lib, b"nvmlDeviceGetMemoryInfo_v2\0");
        report_bool("nvmlDeviceGetMemoryInfo_v2", get_mem_v2.is_some());
        let get_bus_w = sym::<unsafe extern "C" fn(*mut c_void, *mut u32) -> u32>(&lib, b"nvmlDeviceGetMemoryBusWidth\0");
        report_bool("nvmlDeviceGetMemoryBusWidth", get_bus_w.is_some());
        if let Some(f) = get_bus_w {
            let mut v = 0u32;
            let rc = f(dev, &mut v);
            println!("      mem bus width          rc={rc} value={v} bit");
        }

        // ---- 状态 ----
        let get_pstate = sym::<unsafe extern "C" fn(*mut c_void, *mut u32) -> u32>(&lib, b"nvmlDeviceGetPerformanceState\0");
        report_bool("nvmlDeviceGetPerformanceState", get_pstate.is_some());
        if let Some(f) = get_pstate {
            let mut v = 0u32;
            let rc = f(dev, &mut v);
            println!("      performance state      rc={rc} value=P{v}");
        }
        let get_reasons =
            sym::<unsafe extern "C" fn(*mut c_void, *mut u64) -> u32>(&lib, b"nvmlDeviceGetCurrentClocksThrottleReasons\0");
        report_bool("nvmlDeviceGetCurrentClocksThrottleReasons", get_reasons.is_some());
        let get_vbios = sym::<unsafe extern "C" fn(*mut c_void, *mut c_char, u32) -> u32>(&lib, b"nvmlDeviceGetVbiosVersion\0");
        report_bool("nvmlDeviceGetVbiosVersion", get_vbios.is_some());
        if let Some(f) = get_vbios {
            let mut buf = [0 as c_char; 64];
            let rc = f(dev, buf.as_mut_ptr(), buf.len() as u32);
            println!("      vbios                  rc={rc} value={}", cstr(&buf));
        }
        let get_pci = sym::<unsafe extern "C" fn(*mut c_void, *mut NvmlPciInfo) -> u32>(&lib, b"nvmlDeviceGetPciInfo_v3\0")
            .or_else(|| sym::<unsafe extern "C" fn(*mut c_void, *mut NvmlPciInfo) -> u32>(&lib, b"nvmlDeviceGetPciInfo_v2\0"));
        report_bool("nvmlDeviceGetPciInfo_v2/v3", get_pci.is_some());
        if let Some(f) = get_pci {
            let mut p = NvmlPciInfo::default();
            let rc = f(dev, &mut p);
            println!(
                "      pci info               rc={rc} bus_id={} domain={:04X} bus={:02X} dev={:02X} devid={:04X} subsys={:08X}",
                cstr(&p.bus_id), p.domain, p.bus, p.device, p.pci_device_id, p.pci_sub_system_id
            );
        }

        // ---- PCIe 吞吐是「速率」还是「累计量」？连续采样看变化 ----
        println!();
        println!("[4] pcie throughput sampling (3 x 1s) - is it a rate?");
        if let Some(f) = get_pcie {
            for round in 0..3 {
                let (mut rx, mut tx) = (0u32, 0u32);
                f(dev, 0, &mut rx);
                f(dev, 1, &mut tx);
                println!("      round {round}: rx={rx} KB/s  tx={tx} KB/s");
                std::thread::sleep(std::time::Duration::from_millis(1000));
            }
        }

        // ---- 显存：v1 / v2 字段语义并列打印，便于与 nvidia-smi 对拍 ----
        println!();
        println!("[5] memory info field semantics");
        if let Some(f) = get_mem {
            let mut m = NvmlMemory::default();
            let rc = f(dev, &mut m);
            // nvmlMemory_t v1 = { total, free, used }（注意顺序，没有 reserved）
            println!(
                "      v1 rc={rc} total={} B ({} MiB)  free={} B ({} MiB)  used={} B ({} MiB)",
                m.total,
                m.total / 1024 / 1024,
                m.free,
                m.free / 1024 / 1024,
                m.used,
                m.used / 1024 / 1024
            );
        }
        if let Some(f) = get_mem_v2 {
            let mut m = NvmlMemoryV2 {
                version: std::mem::size_of::<NvmlMemoryV2>() as u32 | (2 << 24),
                ..Default::default()
            };
            let rc = f(dev, &mut m);
            println!(
                "      v2 rc={rc} total={} MiB reserved={} MiB free={} MiB used={} MiB",
                m.total / 1024 / 1024,
                m.reserved / 1024 / 1024,
                m.free / 1024 / 1024,
                m.used / 1024 / 1024
            );
        }

        // ---- 温度设置结构体的原始布局（SDK 头里的字段顺序在本机对不上，用字节说话）----
        println!();
        println!("[6] thermal settings raw u32 dump + throttle reasons");
        if let Some(f) = get_thermal {
            // 先把整块缓冲填成哨兵值：NVML 只写它真正用到的字节，没被写到的位置就留在哨兵上，
            // 这样「结构体到底多长」是看出来的而不是猜出来的。
            let mut raw = NvmlGpuThermalSettings::default();
            let p = &mut raw as *mut NvmlGpuThermalSettings as *mut u32;
            let words =
                std::slice::from_raw_parts_mut(p, std::mem::size_of::<NvmlGpuThermalSettings>() / 4);
            for w in words.iter_mut() {
                *w = 0xDEAD_BEEF;
            }
            let rc = f(dev, 0, &mut raw);
            let words =
                std::slice::from_raw_parts(p, std::mem::size_of::<NvmlGpuThermalSettings>() / 4);
            let mut line = String::new();
            for (i, w) in words.iter().enumerate() {
                line.push_str(&format!("[{i}]=0x{w:08X}({}) ", *w as i32));
                if i % 5 == 4 {
                    println!("      {line}");
                    line.clear();
                }
            }
            if !line.is_empty() {
                println!("      {line}");
            }
            println!("      (rc={rc}; nvml.h 顺序 = count, 然后每个 sensor 是 controller/defaultMin/defaultMax/current/target)");
        }
        if let Some(f) = get_reasons {
            for round in 0..2 {
                let mut v = 0u64;
                let rc = f(dev, &mut v);
                println!("      throttle reasons round {round}: rc={rc} 0x{v:016X}");
                if round == 0 {
                    std::thread::sleep(std::time::Duration::from_millis(500));
                }
            }
        }
        if let Some(f) = get_energy {
            let (mut a, mut b) = (0u64, 0u64);
            f(dev, &mut a);
            std::thread::sleep(std::time::Duration::from_millis(1000));
            f(dev, &mut b);
            println!("      total energy delta over 1s = {} mJ -> {} W", b - a, (b - a) as f64 / 1000.0);
        }

        println!();
        println!("[7] done");
    }
}

unsafe fn sym<T: Copy>(lib: &libloading::Library, name: &[u8]) -> Option<T> {
    lib.get::<T>(name).ok().map(|s| *s)
}

fn report_bool(name: &str, present: bool) {
    println!("    {:<52} {}", name, if present { "present" } else { "ABSENT" });
}

unsafe fn cstr(buf: &[c_char]) -> String {
    CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned()
}
