//! AMD SMU PM 表客户端 —— Zen4/5 的 Core/SoC 电压、电流、分项功耗等传感器。
//!
//! 移植自 LibreHardwareMonitor（MPL-2.0）：
//! - `LibreHardwareMonitorLib/PawnIo/RyzenSmu.cs`：PawnIO 调用封装
//! - `LibreHardwareMonitorLib/Hardware/RyzenSMU.cs`：PM 表版本/尺寸/传感器布局
//! - `LibreHardwareMonitorLib/Hardware/Cpu/Amd17Cpu.cs:405-418`：PM 表 → 传感器
//!
//! 为什么非得走这条路：Zen4/5 上 LHM 主动屏蔽了 SVI2 电压通路
//! （`Amd17Cpu.cs:375-396` 给 model 0x61/0x44 置位 `smuSvi0Tfn`，
//! 使 Core/SoC 电压传感器不激活），所以 Core/SoC 电压只能从 SMU PM 表取。
//!
//! 协议：
//! 1. `ioctl_get_code_name` → CPU code name（枚举序号，GraniteRidge = 17）
//! 2. `ioctl_resolve_pm_table` → (pm_table_version, dram_base)
//! 3. 版本 → 表字节长度 + 传感器布局
//! 4. 每轮 `ioctl_update_pm_table` 让 SMU 刷新，再 `ioctl_read_pm_table`
//!    取回 `(size+7)/8` 个 i64，按**小端**重解释为 f32 数组
//! 5. 传感器值 = `floats[index] * scale`
//!
//! 除 `ioctl_get_code_name` 外都必须持有 `Global\Access_PCI` 互斥锁
//! （与 LHM/FanControl 等工具串行化）。

#![cfg(windows)]

use crate::pawnio::{PawnIo, PciBusGuard};
use crate::schema::SmuSensor;

/// LHM 官方签发的 PawnIO SMU 模块（MPL-2.0，来源见 drivers/pawnio/COPYING）。
pub const MODULE: &[u8] = include_bytes!("../../../drivers/pawnio/RyzenSMU.bin");

const PCI_MUTEX_TIMEOUT_MS: u32 = 5000;

// CPU code name（`RyzenSMU.cs:396-438` 枚举序号，`ioctl_get_code_name` 的返回值）。
pub const CODE_NAME_RAPHAEL: u32 = 16;
pub const CODE_NAME_GRANITE_RIDGE: u32 = 17;

/// PM 表版本 → 表字节长度（`RyzenSMU.cs:268-386` `SetupPmTableSize`）。
///
/// 0x0062_0105 是 LHM master / ryzen_smu master **都没有**的 Zen5 固件版本
/// （本机 Ryzen 9850X3D，SMU 版本 0x00625200 实测）。0x948 是按 Zen4 尺寸试读
/// 成功得到的（见 `examples/smu_probe.rs`），且我们只用到下标 47 为止，够用。
pub fn pm_table_size(code_name: u32, version: u32) -> Option<u32> {
    match code_name {
        CODE_NAME_RAPHAEL | CODE_NAME_GRANITE_RIDGE => match version {
            0x0054_0004 => Some(0x948),
            0x0054_0104 => Some(0x950),
            // Zen5 新固件：LHM 未收录，实测可读
            0x0062_0105 => Some(0x948),
            _ => None,
        },
        _ => None,
    }
}

/// PM 表里的一个传感器定义。
/// `index` 是 **f32 数组下标**（字节偏移 = index * 4）。
#[derive(Clone, Copy, Debug)]
pub struct SmuSensorDef {
    pub index: u32,
    pub name: &'static str,
    /// "voltage" | "current" | "power" | "temperature" | "clock"
    pub kind: &'static str,
    /// "V" | "A" | "W" | "°C" | "MHz"
    pub unit: &'static str,
    pub scale: f32,
}

/// Zen 4 布局（PM 表版本 0x00540004），照抄 `RyzenSMU.cs:104-143`。
/// 注：0x00540104 在 LHM 里**没有**布局定义（`GetPmTableStructure()` 会抛
/// KeyNotFound），所以这里也只有 0x00540004 有布局 —— 与 LHM 行为一致。
pub const ZEN4_PM_TABLE: &[SmuSensorDef] = &[
    SmuSensorDef { index: 3, name: "CPU PPT", kind: "power", unit: "W", scale: 1.0 },
    SmuSensorDef { index: 11, name: "Package", kind: "temperature", unit: "°C", scale: 1.0 },
    SmuSensorDef { index: 20, name: "Core Power", kind: "power", unit: "W", scale: 1.0 },
    SmuSensorDef { index: 21, name: "SOC Power", kind: "power", unit: "W", scale: 1.0 },
    SmuSensorDef { index: 22, name: "Misc Power", kind: "power", unit: "W", scale: 1.0 },
    SmuSensorDef { index: 26, name: "Total Power", kind: "power", unit: "W", scale: 1.0 },
    SmuSensorDef { index: 47, name: "VDDCR", kind: "voltage", unit: "V", scale: 1.0 },
    SmuSensorDef { index: 48, name: "TDC", kind: "current", unit: "A", scale: 1.0 },
    SmuSensorDef { index: 49, name: "EDC", kind: "current", unit: "A", scale: 1.0 },
    SmuSensorDef { index: 52, name: "VDDCR SoC", kind: "voltage", unit: "V", scale: 1.0 },
    SmuSensorDef { index: 57, name: "VDD Misc", kind: "voltage", unit: "V", scale: 1.0 },
    SmuSensorDef { index: 70, name: "Fabric", kind: "clock", unit: "MHz", scale: 1.0 },
    SmuSensorDef { index: 74, name: "Uncore", kind: "clock", unit: "MHz", scale: 1.0 },
    SmuSensorDef { index: 78, name: "Memory", kind: "clock", unit: "MHz", scale: 1.0 },
    SmuSensorDef { index: 211, name: "IOD Hotspot", kind: "temperature", unit: "°C", scale: 1.0 },
    SmuSensorDef { index: 268, name: "LDO VDD", kind: "voltage", unit: "V", scale: 1.0 },
    SmuSensorDef { index: 539, name: "L3 (CCD1)", kind: "temperature", unit: "°C", scale: 1.0 },
    SmuSensorDef { index: 540, name: "L3 (CCD2)", kind: "temperature", unit: "°C", scale: 1.0 },
];

/// Zen5 新固件布局（PM 表版本 0x00620105）—— **LHM 没有这份表，是我们实测逆向的**。
///
/// 判定依据（`examples/smu_probe.rs` 的原始 dump + 两个独立真值源交叉验证）：
/// - `[3] CPU PPT`    74.65 W ←→ RAPL 整包功耗同量级（MSR 0xC001029B，完全独立）
/// - `[11] Package`   74.63 °C ←→ SMN Tctl（0x59800，完全独立）
/// - `[20/21/22]`     51.50 / 4.66 / 9.33 W，CPU 满载下的合理分项
/// - `[26] Total`     75.17 W ≈ PPT，两者吻合
/// - `[47] VDDCR`     1.1661 V，正是 Vcore 量级；且 `[309..316]` 是 8 个相同值
///   （逐核电压数组），紧邻 `[317..324]` 是 8 个温度 —— 结构自洽
///
/// **故意不收编的字段**（在 0x620105 上已被证伪，收了就是造假数据）：
/// - `[48/49]` 在 Zen4 是 TDC/EDC 电流，这里与 `[47]` 完全相同 → 不是电流
/// - `[52/57]` 在 Zen4 是 VDDCR SoC / VDD Misc 电压，这里读出 37.31 → 是温度量级
/// - `[211]` IOD Hotspot 读出 3000、`[268]` LDO VDD 读出 120 → 都不是温度/电压
/// - `[539/540]` L3 CCD 温度读出 0
///
/// 所以本机上 `soc_voltage_v` 会是 null —— 宁可空着，也不报一个错的 SoC 电压。
pub const ZEN5_PM_TABLE: &[SmuSensorDef] = &[
    SmuSensorDef { index: 3, name: "CPU PPT", kind: "power", unit: "W", scale: 1.0 },
    SmuSensorDef { index: 11, name: "Package", kind: "temperature", unit: "°C", scale: 1.0 },
    SmuSensorDef { index: 20, name: "Core Power", kind: "power", unit: "W", scale: 1.0 },
    SmuSensorDef { index: 21, name: "SOC Power", kind: "power", unit: "W", scale: 1.0 },
    SmuSensorDef { index: 22, name: "Misc Power", kind: "power", unit: "W", scale: 1.0 },
    SmuSensorDef { index: 26, name: "Total Power", kind: "power", unit: "W", scale: 1.0 },
    SmuSensorDef { index: 47, name: "VDDCR", kind: "voltage", unit: "V", scale: 1.0 },
];

/// PM 表版本 → 传感器布局（`RyzenSMU.cs:19-145` `_supportedPmTableVersions`）。
pub fn sensor_layout(version: u32) -> Option<&'static [SmuSensorDef]> {
    match version {
        0x0054_0004 => Some(ZEN4_PM_TABLE),
        0x0062_0105 => Some(ZEN5_PM_TABLE),
        _ => None,
    }
}

/// 把 `ioctl_read_pm_table` 取回的原始字节按小端重解释成 f32 数组。
/// 对应 `RyzenSMU.cs:263` 的 `Buffer.BlockCopy(read, 0, table, 0, _pmTableSize)`。
pub fn bytes_to_floats(raw: &[u8], table_bytes: usize) -> Vec<f32> {
    let n = (table_bytes / 4).min(raw.len() / 4);
    raw[..n * 4]
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

/// 解码传感器：值 = `floats[index] * scale`；
/// 与 LHM 一致（`Amd17Cpu.cs:411-416`），读回 0 或下标越界的不输出。
pub fn decode(layout: &[SmuSensorDef], floats: &[f32]) -> Vec<SmuSensor> {
    layout
        .iter()
        .filter_map(|d| {
            let v = *floats.get(d.index as usize)? * d.scale;
            if v == 0.0 {
                return None;
            }
            Some(SmuSensor {
                name: d.name.to_string(),
                kind: d.kind.to_string(),
                value: v,
                unit: d.unit.to_string(),
            })
        })
        .collect()
}

/// 按 (名称, 类型) 取值 —— 给合约里的 Core/SoC 电压这类头等指标用。
pub fn find(sensors: &[SmuSensor], name: &str, kind: &str) -> Option<f32> {
    sensors
        .iter()
        .find(|s| s.name == name && s.kind == kind)
        .map(|s| s.value)
}

/// 已打开的 SMU 客户端：持有 PawnIO 句柄 + PM 表元数据。
pub struct SmuClient {
    pawn: PawnIo,
    pub code_name: u32,
    pub pm_table_version: u32,
    pub table_base: u32,
    /// None = 该 code name/版本组合没有已知布局（无传感器可解）
    pub table_size: Option<u32>,
}

impl SmuClient {
    /// 打开 RyzenSMU 模块并解析 PM 表元数据。驱动不在 / 无权限返回 None。
    pub fn open() -> Option<SmuClient> {
        let pawn = PawnIo::open(MODULE)?;

        // LHM 的 GetCodeName() 不进 PCI 互斥锁（RyzenSmu.cs:30-34），照搬。
        let code_name = pawn.execute("ioctl_get_code_name", &[], 1)?.first().copied()? as u32;

        let (pm_table_version, table_base) = {
            let _pci = PciBusGuard::wait(PCI_MUTEX_TIMEOUT_MS)?;
            let v = pawn.execute("ioctl_resolve_pm_table", &[], 2)?;
            if v.len() < 2 {
                return None;
            }
            (v[0] as u32, v[1] as u32)
        };

        let table_size = pm_table_size(code_name, pm_table_version);
        Some(SmuClient {
            pawn,
            code_name,
            pm_table_version,
            table_base,
            table_size,
        })
    }

    /// SMU 固件版本（诊断用；LHM 的传感器路径不依赖它）。
    pub fn smu_version(&self) -> Option<u32> {
        let _pci = PciBusGuard::wait(PCI_MUTEX_TIMEOUT_MS)?;
        self.pawn
            .execute("ioctl_get_smu_version", &[], 1)
            .map(|v| v[0] as u32)
    }

    pub fn layout(&self) -> Option<&'static [SmuSensorDef]> {
        sensor_layout(self.pm_table_version)
    }

    /// 刷新 + 读取 PM 表并解码。
    /// 照搬 LHM `GetPmTable()`（`RyzenSMU.cs:230-255`）：首值读回 0 说明 SMU
    /// 还没更新完，重试一次。
    pub fn read_sensors(&self) -> Option<Vec<SmuSensor>> {
        let layout = self.layout()?;
        let mut last: Option<Vec<f32>> = None;
        for _ in 0..2 {
            let floats = match self.read_floats() {
                Some(f) => f,
                None => break,
            };
            if floats.first().copied().unwrap_or(0.0) != 0.0 {
                return Some(decode(layout, &floats));
            }
            last = Some(floats);
        }
        last.map(|f| decode(layout, &f))
    }

    /// `ioctl_update_pm_table` + `ioctl_read_pm_table` → f32 数组。
    /// 两步共用同一把 PCI 互斥锁（LHM 是各自加解锁，这里更严格）。
    pub fn read_floats(&self) -> Option<Vec<f32>> {
        self.read_floats_sized(self.table_size? as usize)
    }

    /// 按指定字节数读 PM 表 —— **不依赖已知布局**。
    /// 用途：碰到 LHM/ryzen_smu 都还没有的 PM 表版本时（本机 0x00620105 就是），
    /// 先按候选尺寸把原始表捞出来做逆向比对。
    pub fn read_floats_sized(&self, table_bytes: usize) -> Option<Vec<f32>> {
        let longs = table_bytes.div_ceil(8);

        let _pci = PciBusGuard::wait(PCI_MUTEX_TIMEOUT_MS)?;

        // 让 SMU 把最新数据写回 DRAM 里的 PM 表；失败也继续试着读
        let _ = self.pawn.execute("ioctl_update_pm_table", &[], 0);

        let vals = self.pawn.execute("ioctl_read_pm_table", &[], longs)?;

        // execute() 已经把 i64 按小端还原过，这里再摊平成字节流做 f32 重解释
        let mut bytes = Vec::with_capacity(vals.len() * 8);
        for v in vals {
            bytes.extend_from_slice(&v.to_le_bytes());
        }
        Some(bytes_to_floats(&bytes, table_bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pm_table_size_zen4_zen5() {
        assert_eq!(pm_table_size(CODE_NAME_RAPHAEL, 0x0054_0004), Some(0x948));
        assert_eq!(pm_table_size(CODE_NAME_GRANITE_RIDGE, 0x0054_0004), Some(0x948));
        assert_eq!(pm_table_size(CODE_NAME_GRANITE_RIDGE, 0x0054_0104), Some(0x950));
        // 本机 Zen5 固件（LHM / ryzen_smu 都没收录，尺寸是实测出来的）
        assert_eq!(pm_table_size(CODE_NAME_GRANITE_RIDGE, 0x0062_0105), Some(0x948));
        // 未知版本 / 未知族没有尺寸
        assert_eq!(pm_table_size(CODE_NAME_GRANITE_RIDGE, 0x0011_2233), None);
        assert_eq!(pm_table_size(0xFFFF, 0x0054_0004), None);
    }

    #[test]
    fn layout_only_for_known_version() {
        assert_eq!(sensor_layout(0x0054_0004).map(|l| l.len()), Some(18));
        // LHM 对 0x00540104 也没有布局 → 我们同样不定义
        assert!(sensor_layout(0x0054_0104).is_none());
        // Zen5 实测逆向的表
        assert_eq!(sensor_layout(0x0062_0105).map(|l| l.len()), Some(7));
    }

    /// Zen5 表里的每一项都必须在可读范围内，且下标不重复。
    /// VDDCR 是 `core_voltage_v` 的来源，改名会让电压静默变 null。
    #[test]
    fn zen5_layout_is_sane() {
        let mut seen = std::collections::HashSet::new();
        for d in ZEN5_PM_TABLE {
            assert!(seen.insert(d.index), "Zen5 PM 表下标重复: {}", d.index);
            assert!((d.index as usize) < 0x948 / 4, "下标越界: {}", d.index);
            assert!(d.scale > 0.0, "scale 必须为正: {}", d.name);
        }
        assert_eq!(ZEN5_PM_TABLE.iter().find(|d| d.name == "VDDCR").map(|d| d.kind), Some("voltage"));
        // 已被实测证伪的下标绝不能出现在 Zen5 表里（[48] TDC、[49] EDC、[52] VDDCR SoC、
        // [57] VDD Misc、[211] IOD Hotspot、[268] LDO VDD、[539]/[540] L3 CCD）
        for bogus in [48u32, 49, 52, 57, 211, 268, 539, 540] {
            assert!(
                !ZEN5_PM_TABLE.iter().any(|d| d.index == bogus),
                "Zen5 表混入了在这版固件上已证伪的下标 {bogus}"
            );
        }
    }

    #[test]
    fn layout_indices_unique() {
        let mut seen = std::collections::HashSet::new();
        for d in ZEN4_PM_TABLE {
            assert!(seen.insert(d.index), "PM 表下标重复: {}", d.index);
        }
    }

    #[test]
    fn bytes_to_floats_is_little_endian() {
        let mut raw = Vec::new();
        raw.extend_from_slice(&1.5f32.to_le_bytes());
        raw.extend_from_slice(&(-2.25f32).to_le_bytes());
        // 尾部多出的字节（表长不是 4 的倍数时的补零）不能混进来
        raw.extend_from_slice(&[0xAA, 0xBB]);
        let f = bytes_to_floats(&raw, 8);
        assert_eq!(f, vec![1.5, -2.25]);
    }

    #[test]
    fn decode_reads_indexed_values() {
        // 0x948 字节 = 594 个 f32
        let mut floats = vec![0f32; 594];
        floats[47] = 1.3125; // VDDCR
        floats[52] = 1.05; // VDDCR SoC
        floats[3] = 88.0; // CPU PPT

        let sensors = decode(ZEN4_PM_TABLE, &floats);
        assert_eq!(find(&sensors, "VDDCR", "voltage"), Some(1.3125));
        assert_eq!(find(&sensors, "VDDCR SoC", "voltage"), Some(1.05));
        assert_eq!(find(&sensors, "CPU PPT", "power"), Some(88.0));
        // 全 0 的项被丢掉（LHM 只在非 0 时激活传感器）
        assert_eq!(find(&sensors, "TDC", "current"), None);
        assert_eq!(sensors.len(), 3);
    }

    #[test]
    fn decode_tolerates_short_table() {
        // 表比布局短：越界项直接跳过，不 panic
        let floats = vec![0f32; 10];
        assert!(decode(ZEN4_PM_TABLE, &floats).is_empty());
    }

    #[test]
    fn find_is_name_and_kind_sensitive() {
        let sensors = vec![SmuSensor {
            name: "Package".into(),
            kind: "temperature".into(),
            value: 61.0,
            unit: "°C".into(),
        }];
        assert_eq!(find(&sensors, "Package", "temperature"), Some(61.0));
        // 同名不同类型不应命中
        assert_eq!(find(&sensors, "Package", "power"), None);
    }
}
