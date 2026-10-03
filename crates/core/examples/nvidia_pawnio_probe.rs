#![cfg(windows)]
//! PawnIO `Nvidia.bin` 探针：给 NVAPI 的热点/显存结温找一条**完全独立**的对拍通路。
//!
//! LHM 的 `NvidiaGpu` 有两条互不相干的温度通路：
//!   1. NVAPI `NvAPI_GPU_GetThermalSensors`（进程内 API，不需要管理员）
//!   2. PawnIO `Nvidia.bin` 模块直接读 GPU 的 MMIO 热通道寄存器（需要管理员 + `Global\Access_PCI`）
//! 本项目已实现第 1 条（`crate::nvapi`），本探针用来跑第 2 条，看两条能不能互相印证。
//!
//! LHM `PawnIo\Nvidia.cs` 的用法逐字照抄：
//!   - `TryReadThermalChannels(bus, device, 0, [6])`：`ioctl_read_thermal_registers`，输出 6 路，
//!     每路 `bit30 = 有效`、`value = raw & 0xFFFF`、温度 = `value / 256.0f`；
//!   - `TryReadMemoryTemperatures(bus, device, 0, [48])`：`ioctl_read_memory_temperatures`，
//!     输出 `48 + 2` 路，要求 `out[1] == 48`，然后 `out[i + 2]` 就是第 i 路温度（整数），
//!     `i32::MIN` 表示无效；
//!   - bus/device 取自 NVAPI 的 `GetBusId` / `GetBusSlotId`，function 恒为 0
//!     （LHM: `_pciDeviceId = busSlotId`，`TryReadThermalChannels(_pciBusId, _pciDeviceId, 0, ...)`）；
//!   - 调用前后要持有 PCI 总线互斥锁（`Mutexes.WaitPciBus(10)` → `Global\Access_PCI`）。
//!
//! 输出全 ASCII，需要管理员权限。

fn main() {
    println!("[1] NVAPI 侧：拿到 bus/device 与 32 路原始值");
    let nvapi = cs_core::nvapi::NvapiGpu::open();
    let (bus, device) = match &nvapi {
        Some(g) => {
            let (b, d) = g.pci_bus_device();
            println!(
                "    name = {}, mask = 0x{:08X}, bus = {:?}, device = {:?}",
                g.name(),
                g.mask(),
                b,
                d
            );
            match (b, d) {
                (Some(b), Some(d)) => (b, d),
                _ => {
                    println!("    [!] NVAPI 没给出 bus/device，后面无法调用 PawnIO 模块");
                    return;
                }
            }
        }
        None => {
            println!("    [!] NVAPI 打不开（没有 NVIDIA 卡 / 没有 nvapi64.dll）");
            return;
        }
    };

    if let Some(g) = &nvapi {
        match g.read_raw() {
            Some(raw) => {
                for (i, v) in raw.iter().enumerate() {
                    if *v != cs_core::nvapi::INVALID_RAW && *v > 0 {
                        println!(
                            "    NVAPI [{i:>2}] raw = {v:>7}  = {:>7.2} C",
                            *v as f32 / 256.0
                        );
                    }
                }
            }
            None => println!("    [!] read_raw 失败"),
        }
    }

    println!();
    println!("[2] PawnIO Nvidia.bin 模块");
    let module: &[u8] = include_bytes!("../../../drivers/pawnio/Nvidia.bin");
    println!("    模块 {} 字节", module.len());
    let pawn = match cs_core::pawnio::PawnIo::open(module) {
        Some(p) => p,
        None => {
            println!("    [!] 模块加载失败（需要管理员权限？）");
            return;
        }
    };
    println!("    加载成功");

    let guard = cs_core::pawnio::PciBusGuard::wait(10_000);
    println!(
        "    PCI 总线锁：{}",
        if guard.is_some() { "已持有" } else { "拿不到（继续试）" }
    );

    // ---- 6 路热通道 ----
    println!();
    println!("[3] ioctl_read_thermal_registers(bus={bus}, device={device}, function=0) -> 6 路");
    match pawn.execute_hr("ioctl_read_thermal_registers", &[bus as i64, device as i64, 0], 6) {
        Ok(out) => {
            println!("    返回 {} 路", out.len());
            let mut max_valid: Option<f32> = None;
            for (i, cell) in out.iter().enumerate() {
                let raw = *cell as u32;
                let valid = (raw & (1 << 30)) != 0;
                let value = (raw & 0xFFFF) as f32 / 256.0;
                if valid {
                    max_valid = Some(max_valid.map_or(value, |m: f32| m.max(value)));
                }
                println!(
                    "      [{i}] raw = 0x{raw:08X}  valid_bit30 = {:<5}  /256 = {:>7.2} C",
                    valid, value
                );
            }
            println!("    有效通道最大值 = {:?} C（LHM 拿它当 \"GPU Hot Spot\"）", max_valid);
        }
        Err(code) => println!("    [!] 调用失败，NTSTATUS/Win32 = 0x{code:08X} ({code})"),
    }

    // ---- 48 路显存温度 ----
    println!();
    println!("[4] ioctl_read_memory_temperatures(bus={bus}, device={device}, function=0) -> 48 路");
    match pawn.execute_hr("ioctl_read_memory_temperatures", &[bus as i64, device as i64, 0], 50) {
        Ok(out) => {
            println!("    返回 {} 路（期望 50）", out.len());
            if out.len() >= 2 {
                println!("    out[0] = {}, out[1] = {}（LHM 要求 out[1] == 48）", out[0], out[1]);
            }
            let mut valid: Vec<(usize, i64)> = Vec::new();
            for i in 0..48usize {
                if i + 2 >= out.len() {
                    break;
                }
                let v = out[i + 2];
                if v != i32::MIN as i64 {
                    valid.push((i, v));
                }
            }
            println!("    有效路数 = {} / 48", valid.len());
            for (i, v) in valid.iter().take(64) {
                println!("      memory[{i:>2}] = {v} C");
            }
            if let (Some(min), Some(max)) = (
                valid.iter().map(|(_, v)| *v).min(),
                valid.iter().map(|(_, v)| *v).max(),
            ) {
                println!("    范围 {min} .. {max} C");
            }
        }
        Err(code) => println!("    [!] 调用失败，NTSTATUS/Win32 = 0x{code:08X} ({code})"),
    }

    drop(guard);

    // ---- 对拍 ----
    println!();
    println!("[5] 对拍：NVAPI 的 [1]/[2] 与 PawnIO 的两组读数");
    if let Some(g) = &nvapi {
        match g.read_raw() {
            Some(raw) => {
                println!(
                    "    NVAPI [1] (RTX 50 系 = 核心温度) = {:.2} C",
                    raw[1] as f32 / 256.0
                );
                println!(
                    "    NVAPI [2] (RTX 50 系 = 显存结温) = {:.2} C",
                    raw[2] as f32 / 256.0
                );
            }
            None => println!("    NVAPI 复读失败"),
        }
    }
    println!("    直接比较上面的 PawnIO 通道值 / 显存传感器值，看是否落在同一量级");
}
