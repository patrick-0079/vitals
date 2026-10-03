//! SMU PM 表探针 —— 分步打点，定位是驱动/权限/表版本/布局哪一环出问题。
//!
//! 跑法（需管理员，PawnIO 设备只对管理员开放）：
//!   cargo run -p cs-core --example smu_probe --release
//!
//! 已知（Ryzen 9850X3D / GraniteRidge）：code_name = 17，SMU 版本 0x00625200，
//! 但 **PM 表版本 0x00620105** 是 LHM master 与 ryzen_smu master 都没有的新值。
//! 所以第 [3]/[4] 步不依赖"已知布局"——直接用 Zen4 的布局当假设去对拍真值：
//!   floats[3]  CPU PPT  ≈ RAPL 整包功耗（面板上的 W）
//!   floats[11] Package  ≈ Tctl（面板上的 °C）
//!   floats[47] VDDCR    ≈ 0.9~1.4 V
//! 若这些都对得上，说明表结构没变、只是版本号变了。

#![cfg(windows)]

use cs_core::smu::{self, ZEN4_PM_TABLE};

fn main() {
    println!("[1] 打开 RyzenSMU 模块 + resolve PM 表");
    let Some(c) = smu::SmuClient::open() else {
        println!("    失败：PawnIO 设备打不开（需管理员）或模块加载失败");
        return;
    };
    println!("    code_name        = {} (16=Raphael, 17=GraniteRidge)", c.code_name);
    println!("    pm_table_version = 0x{:08X}", c.pm_table_version);
    println!("    table_base       = 0x{:08X}", c.table_base);
    println!(
        "    table_size       = {}",
        c.table_size
            .map(|s| format!("0x{:X} ({} 个 f32)", s, s / 4))
            .unwrap_or_else(|| "无已知布局".into())
    );
    println!(
        "    smu_version      = {}",
        c.smu_version()
            .map(|v| format!("0x{:08X}", v))
            .unwrap_or_else(|| "n/a".into())
    );

    // 未知表版本时按 Zen4 的尺寸试读：表结构很可能没变，只是版本号变了
    let size = c.table_size.unwrap_or(0x948) as usize;

    println!("[2] 按 {} 字节读 PM 表原始 f32", size);
    let Some(floats) = c.read_floats_sized(size) else {
        println!("    失败：拿不到 PCI 互斥锁或 IOCTL 报错");
        return;
    };
    println!("    长度 = {} 个 f32", floats.len());
    let head = floats
        .iter()
        .take(8)
        .enumerate()
        .map(|(i, v)| format!("[{}]={:.6}", i, v))
        .collect::<Vec<_>>()
        .join(" ");
    println!("    前 8 个: {head}");
    let nonzero = floats.iter().filter(|v| **v != 0.0).count();
    println!("    非 0 项 = {nonzero} / {}", floats.len());

    println!("[3] Zen4 布局下标处的实际值（对拍真值用）");
    for d in ZEN4_PM_TABLE {
        let raw = floats.get(d.index as usize).copied().unwrap_or(f32::NAN);
        println!("    [{:>3}] {:<12} raw = {:>12.6}", d.index, d.name, raw);
    }

    println!("[4] 全表启发式扫描：疑似电压（0.5 ~ 1.6）");
    let volts: Vec<String> = floats
        .iter()
        .enumerate()
        .filter(|(_, v)| (0.5..1.6).contains(*v))
        .map(|(i, v)| format!("[{}]={:.4}", i, v))
        .collect();
    if volts.is_empty() {
        println!("    无");
    } else {
        for chunk in volts.chunks(8) {
            println!("    {}", chunk.join("  "));
        }
    }

    println!("[5] 全表启发式扫描：疑似温度（20 ~ 110）");
    let temps: Vec<String> = floats
        .iter()
        .enumerate()
        .filter(|(_, v)| (20.0..110.0).contains(*v))
        .map(|(i, v)| format!("[{}]={:.2}", i, v))
        .collect();
    if temps.is_empty() {
        println!("    无");
    } else {
        for chunk in temps.chunks(8) {
            println!("    {}", chunk.join("  "));
        }
    }

    println!("[6] 解码为传感器（仅在布局已知时才有输出）");
    match c.read_sensors() {
        Some(list) if !list.is_empty() => {
            for s in &list {
                println!("    {:<12} {:>10.4} {}", s.name, s.value, s.unit);
            }
        }
        Some(_) => println!("    解出 0 个传感器（首值仍为 0？SMU 未更新）"),
        None => println!("    无已知布局，跳过（先看 [3] 能否对拍成功）"),
    }
}
