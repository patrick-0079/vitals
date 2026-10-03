//! 主板 SuperIO（LPC 硬件监控芯片）—— 风扇转速、主板温度、各路电压。
//!
//! 机制来源：LibreHardwareMonitor（MPL-2.0）
//! - `Hardware/Motherboard/Lpc/LpcIO.cs`        芯片探测（配置端口 0x2E/0x4E、ID 0x20/0x21、运行时基址 0x60/0x64）
//! - `Hardware/Motherboard/Lpc/LpcPort.cs`      进入/退出配置空间的时序
//! - `PawnIo/LpcIO.cs`                          LpcIO.bin 的导出函数
//! - `Hardware/Motherboard/Lpc/Nct677X.cs`      运行时寄存器解码（只移植了 NCT6701D 分支）
//! - `Hardware/Motherboard/SuperIOHardware.cs`  主板型号 → 传感器命名（ASUS AM5 档）
//! - `Hardware/Motherboard/Voltage.cs`          分压还原公式
//!
//! 访问 LPC 端口需要管理员权限（PawnIO 设备只对管理员开放）；读之前按 LHM
//! `Mutexes.cs` 的约定持有 `Global\Access_ISABUS.HTP.Method`，与 LHM/FanControl 串行化。
//!
//! ## 本机实测（ASUS TUF GAMING B850M-PLUS WIFI7 + Nuvoton NCT6701D）
//!
//! 探测结论：id=0xD8 rev=0x06 → NCT6701D，配置端口 0x2E，运行时基址 0x290（非 EC 空间，
//! 走 bank/register 协议），`io_lock` 已是 0 → 无需解锁。运行时全 bank 转储与
//! LHM 的寄存器表逐项对得上：
//!
//! | 传感器 | 寄存器 | 实测 | 交叉验证 |
//! |---|---|---|---|
//! | Vcore | 0x480 | 1.384 V | SMU PM 表 VDDCR 1.376 V（独立通路，偏差 0.6%） |
//! | +12V | 0x484 | 12.08 V | 标称 12 V |
//! | +5V | 0x481 | 5.02 V | 标称 5 V |
//! | +3.3V | 0x483 | 3.39 V | 标称 3.3 V |
//! | Motherboard | 0x490(SYSTIN) | 35 °C | — |
//! | T-Sensor | 0x495(AUXTIN3) | 26 °C | — |
//! | VRM | 0x491(CPUTIN) | 36 °C | — |
//! | CPU | 0x4F4(PECI_0_CAL) | 45 °C | AMD Tctl（SMN 0x59800） |
//!
//! 注意 LHM 的 `TUF_GAMING_B850M_PLUS_II` 档把 "CPU" 放在下标 **22**（`PECI_1_CAL` @0x4F5），
//! 但本机该寄存器恒为 0x00 → LHM 的 `DecodeNct6701Temperature` 判为「无传感器」。
//! 本机可用的是下标 **21**（`PECI_0_CAL` @0x4F4）。我们的板型（…-PLUS WIFI7）LHM 尚未收录，
//! 因此温度命名以**实测对拍**为准，profile 里逐项标注来源。

#![cfg(windows)]

use crate::pawnio::{IsaBusGuard, PawnIo};
use crate::schema::BoardSensor;

const MODULE: &[u8] = include_bytes!("../../../drivers/pawnio/LpcIO.bin");

// ── 配置空间（LpcIO.cs:780-786 / LpcPort.cs:8-14）
const REGISTER_PORTS: [u16; 2] = [0x2E, 0x4E];
const CHIP_ID_REGISTER: u8 = 0x20;
const CHIP_REVISION_REGISTER: u8 = 0x21;
const BASE_ADDRESS_REGISTER: u8 = 0x60;
const ALTERNATE_BASE_ADDRESS_REGISTER: u8 = 0x64;
const DEVICE_SELECT_REGISTER: u8 = 0x07;
const LOGICAL_DEVICE_ACTIVATE_REGISTER: u8 = 0x30;
const NUVOTON_HARDWARE_MONITOR_IO_SPACE_LOCK: u8 = 0x28;
const WINBOND_NUVOTON_HARDWARE_MONITOR_LDN: u8 = 0x0B;

// ── 运行时空间（Nct677X.cs:23-25、244-254）
const ADDRESS_REGISTER_OFFSET: u16 = 0x05;
const DATA_REGISTER_OFFSET: u16 = 0x06;
const BANK_SELECT_REGISTER: u8 = 0x4E;
/// NCT6779D~NCT6701D 一组共用的 16 路电压寄存器（Nct677X.cs:253）
const VOLTAGE_REGISTERS: [u16; 16] = [
    0x480, 0x481, 0x482, 0x483, 0x484, 0x485, 0x486, 0x487, 0x488, 0x489, 0x48A, 0x48B, 0x48C,
    0x48D, 0x48E, 0x48F,
];
/// 电池电压寄存器（Nct677X.cs:254）
const VOLTAGE_VBAT_REGISTER: u16 = 0x488;
/// 电池监测使能位（Nct677X.cs:154）
const VBAT_MONITOR_CONTROL_REGISTER: u16 = 0x005D;
/// 13 位风扇计数寄存器，high/low 各占一个字节（Nct677X.cs:244）
const FAN_COUNT_REGISTERS: [u16; 7] = [0x4B0, 0x4B2, 0x4B4, 0x4B6, 0x4B8, 0x4BA, 0x4CC];
/// 13 位计数上限：>= 此值 = 风扇不转（Nct677X.cs:247）
const MAX_FAN_COUNT: u32 = 0x1FFF;
/// 计数下限：< 此值 = 转太快测不准（Nct677X.cs:250）
const MIN_FAN_COUNT: u32 = 0x15;
/// 风扇计数 → RPM 的常数（Nct677X.cs:1008）
const FAN_RPM_NUMERATOR: f32 = 1.35e6;

/// 主板 SuperIO 探测/读取是否可用（非管理员时 false）。
static ACCESS_DENIED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// true = 打开 LpcIO 模块时被拒（通常需要以管理员运行）。
pub fn access_denied() -> bool {
    ACCESS_DENIED.load(std::sync::atomic::Ordering::Relaxed)
}

// ── NCT6701D 温度源表（Nct677X.cs:259-290） ─────────────────────────────────

/// `SourceNct67Xxd` 枚举（Nct677X.cs:1603-1640）
mod src {
    pub const SYSTIN: u8 = 1;
    pub const CPUTIN: u8 = 2;
    pub const AUXTIN0: u8 = 3;
    pub const AUXTIN1: u8 = 4;
    pub const AUXTIN2: u8 = 5;
    pub const AUXTIN3: u8 = 6;
    pub const AUXTIN4: u8 = 7;
    pub const SMBUSMASTER0: u8 = 8;
    pub const SMBUSMASTER1: u8 = 9;
    pub const PECI_0: u8 = 16;
    pub const PECI_1: u8 = 17;
    pub const PCH_CHIP_CPU_MAX_TEMP: u8 = 18;
    pub const PCH_CHIP_TEMP: u8 = 19;
    pub const PCH_CPU_TEMP: u8 = 20;
    pub const PCH_MCH_TEMP: u8 = 21;
    pub const AGENT0_DIMM0: u8 = 22;
    pub const AGENT0_DIMM1: u8 = 23;
    pub const AGENT1_DIMM0: u8 = 24;
    pub const AGENT1_DIMM1: u8 = 25;
    pub const BYTE_TEMP0: u8 = 26;
    pub const BYTE_TEMP1: u8 = 27;
    pub const PECI_0_CAL: u8 = 28;
    pub const PECI_1_CAL: u8 = 29;
    pub const VIRTUAL_TEMP: u8 = 31;
    pub const SPARE_TEMP: u8 = 32;
    pub const SPARE_TEMP2: u8 = 33;
}

#[derive(Clone, Copy)]
struct TempSource {
    /// None = LHM 表里 Source 为 null 的条目，直接按下标赋值
    source: Option<u8>,
    register: u16,
    /// > 0 时实际数据源从该寄存器读出（PECI 通道复用）
    source_register: u16,
}

const fn t(source: Option<u8>, register: u16) -> TempSource {
    TempSource {
        source,
        register,
        source_register: 0,
    }
}

const fn ts(source: u8, register: u16, source_register: u16) -> TempSource {
    TempSource {
        source: Some(source),
        register,
        source_register,
    }
}

/// Nct677X.cs:260-290 —— NCT6701D 的 28 个温度条目（下标即 LHM 的传感器下标）
const NCT6701D_TEMP_SOURCES: [TempSource; 28] = [
    ts(src::PECI_0, 0x073, 0x100),             //  0 PECI_0
    t(Some(src::CPUTIN), 0x491),               //  1 CPUTIN
    t(Some(src::SYSTIN), 0x490),               //  2 SYSTIN      ← 主板温度
    t(Some(src::AUXTIN0), 0x492),              //  3 AUXTIN0
    t(Some(src::AUXTIN1), 0x493),              //  4 AUXTIN1
    t(Some(src::AUXTIN2), 0x494),              //  5 AUXTIN2
    t(Some(src::AUXTIN3), 0x495),              //  6 AUXTIN3     ← T-Sensor
    ts(src::AUXTIN4, 0x027, 0x621),            //  7 AUXTIN4
    ts(src::PECI_1, 0x672, 0xC27),             //  8 PECI_1
    ts(src::PCH_CHIP_CPU_MAX_TEMP, 0x674, 0xC28), //  9
    ts(src::PCH_CHIP_TEMP, 0x676, 0xC29),      // 10
    ts(src::PCH_CPU_TEMP, 0x678, 0xC2A),       // 11
    ts(src::PCH_MCH_TEMP, 0x67A, 0xC2B),       // 12
    t(Some(src::AGENT0_DIMM0), 0x405),         // 13
    t(Some(src::AGENT0_DIMM1), 0x406),         // 14
    t(Some(src::AGENT1_DIMM0), 0x407),         // 15
    t(Some(src::AGENT1_DIMM1), 0x408),         // 16
    ts(src::SMBUSMASTER0, 0x150, 0x622),       // 17
    ts(src::SMBUSMASTER1, 0x670, 0xC26),       // 18
    t(Some(src::BYTE_TEMP0), 0x419),           // 19
    t(Some(src::BYTE_TEMP1), 0x41A),           // 20
    t(Some(src::PECI_0_CAL), 0x4F4),           // 21 PECI_0_CAL ← 本机 CPU 温度
    t(Some(src::PECI_1_CAL), 0x4F5),           // 22 PECI_1_CAL
    t(Some(src::VIRTUAL_TEMP), 0),             // 23 寄存器为 0 → 跳过
    ts(src::SPARE_TEMP, 0x07B, 0x900),         // 24
    t(Some(src::SPARE_TEMP2), 0),              // 25 寄存器为 0 → 跳过
    t(None, 0x409),                            // 26 CPU PACKAGE
    t(None, 0x4A2),                            // 27 TEMP14
];

// ── 主板档案（SuperIOHardware.cs 的型号 → 传感器命名） ───────────────────────

#[derive(Clone, Copy)]
struct VoltageDef {
    name: &'static str,
    index: usize,
    /// 分压电阻比（Nct677X 内部值 ×0.008 V 之后再做还原）
    ri: f32,
    rf: f32,
    vf: f32,
    hidden: bool,
}

#[derive(Clone, Copy)]
struct NamedSensor {
    name: &'static str,
    index: usize,
}

struct BoardProfile {
    /// 档案标识，会进 JSON，便于核对用的哪一套命名
    id: &'static str,
    voltages: &'static [VoltageDef],
    temperatures: &'static [NamedSensor],
    fans: &'static [NamedSensor],
}

/// ASUS AM5（B850M/X870 一档）—— 数值取自 LHM `TUF_GAMING_B850M_PLUS_II`（SuperIOHardware.cs:5616-5651）
const ASUS_AM5_VOLTAGES: [VoltageDef; 16] = [
    VoltageDef { name: "Vcore", index: 0, ri: 0.0, rf: 1.0, vf: 0.0, hidden: false },
    VoltageDef { name: "+5V", index: 1, ri: 4.02, rf: 1.0, vf: 0.0, hidden: false },
    VoltageDef { name: "AVSB", index: 2, ri: 34.0, rf: 34.0, vf: 0.0, hidden: false },
    VoltageDef { name: "+3.3V", index: 3, ri: 34.0, rf: 34.0, vf: 0.0, hidden: false },
    VoltageDef { name: "+12V", index: 4, ri: 10.98, rf: 1.0, vf: 0.0, hidden: false },
    VoltageDef { name: "Voltage #6", index: 5, ri: 0.0, rf: 1.0, vf: 0.0, hidden: true },
    VoltageDef { name: "Voltage #7", index: 6, ri: 0.0, rf: 1.0, vf: 0.0, hidden: true },
    VoltageDef { name: "+3V Standby", index: 7, ri: 34.0, rf: 34.0, vf: 0.0, hidden: false },
    VoltageDef { name: "CMOS Battery", index: 8, ri: 34.0, rf: 34.0, vf: 0.0, hidden: false },
    VoltageDef { name: "VTT", index: 9, ri: 34.0, rf: 34.0, vf: 0.0, hidden: false },
    VoltageDef { name: "CPU VDDIO Memory", index: 10, ri: 34.0, rf: 34.0, vf: 0.0, hidden: false },
    VoltageDef { name: "VMISC", index: 11, ri: 34.0, rf: 34.0, vf: 0.0, hidden: false },
    VoltageDef { name: "1.8V Standby", index: 12, ri: 7.66, rf: 10.0, vf: 0.0, hidden: false },
    VoltageDef { name: "Voltage #14", index: 13, ri: 0.0, rf: 1.0, vf: 0.0, hidden: true },
    VoltageDef { name: "Voltage #15", index: 14, ri: 0.0, rf: 1.0, vf: 0.0, hidden: true },
    VoltageDef { name: "Voltage #16", index: 15, ri: 0.0, rf: 1.0, vf: 0.0, hidden: true },
];

/// 温度命名：前三条来自 LHM `TUF_GAMING_B850M_PLUS_II`；"VRM"@1 来自 `ROG_STRIX_B850_A`；
/// "CPU"@21 是本机实测对拍后改的（LHM 的 @22 在本机恒为哨兵值 0x00）。
const ASUS_AM5_TEMPERATURES: [NamedSensor; 4] = [
    NamedSensor { name: "CPU", index: 21 },
    NamedSensor { name: "VRM", index: 1 },
    NamedSensor { name: "Motherboard", index: 2 },
    NamedSensor { name: "T-Sensor", index: 6 },
];

const ASUS_AM5_FANS: [NamedSensor; 6] = [
    NamedSensor { name: "Chassis Fan #1", index: 0 },
    NamedSensor { name: "CPU Fan", index: 1 },
    NamedSensor { name: "Chassis Fan #2", index: 2 },
    NamedSensor { name: "Chassis Fan #3", index: 3 },
    NamedSensor { name: "CPU Optional Fan", index: 4 },
    NamedSensor { name: "AIO Pump", index: 5 },
];

/// 未知主板时的兜底命名（LHM `SuperIOHardware.cs:5653-5680` 的 default 分支）
const DEFAULT_VOLTAGES: [VoltageDef; 15] = [
    VoltageDef { name: "Vcore", index: 0, ri: 0.0, rf: 1.0, vf: 0.0, hidden: false },
    VoltageDef { name: "Voltage #2", index: 1, ri: 0.0, rf: 1.0, vf: 0.0, hidden: true },
    VoltageDef { name: "AVCC", index: 2, ri: 34.0, rf: 34.0, vf: 0.0, hidden: false },
    VoltageDef { name: "+3.3V", index: 3, ri: 34.0, rf: 34.0, vf: 0.0, hidden: false },
    VoltageDef { name: "Voltage #5", index: 4, ri: 0.0, rf: 1.0, vf: 0.0, hidden: true },
    VoltageDef { name: "Voltage #6", index: 5, ri: 0.0, rf: 1.0, vf: 0.0, hidden: true },
    VoltageDef { name: "Voltage #7", index: 6, ri: 0.0, rf: 1.0, vf: 0.0, hidden: true },
    VoltageDef { name: "+3V Standby", index: 7, ri: 34.0, rf: 34.0, vf: 0.0, hidden: false },
    VoltageDef { name: "CMOS Battery", index: 8, ri: 34.0, rf: 34.0, vf: 0.0, hidden: false },
    VoltageDef { name: "CPU Termination", index: 9, ri: 0.0, rf: 1.0, vf: 0.0, hidden: false },
    VoltageDef { name: "Voltage #11", index: 10, ri: 0.0, rf: 1.0, vf: 0.0, hidden: true },
    VoltageDef { name: "Voltage #12", index: 11, ri: 0.0, rf: 1.0, vf: 0.0, hidden: true },
    VoltageDef { name: "Voltage #13", index: 12, ri: 0.0, rf: 1.0, vf: 0.0, hidden: true },
    VoltageDef { name: "Voltage #14", index: 13, ri: 0.0, rf: 1.0, vf: 0.0, hidden: true },
    VoltageDef { name: "Voltage #15", index: 14, ri: 0.0, rf: 1.0, vf: 0.0, hidden: true },
];

const DEFAULT_TEMPERATURES: [NamedSensor; 7] = [
    NamedSensor { name: "CPU Core", index: 0 },
    NamedSensor { name: "Temperature #1", index: 1 },
    NamedSensor { name: "Temperature #2", index: 2 },
    NamedSensor { name: "Temperature #3", index: 3 },
    NamedSensor { name: "Temperature #4", index: 4 },
    NamedSensor { name: "Temperature #5", index: 5 },
    NamedSensor { name: "Temperature #6", index: 6 },
];

const DEFAULT_FANS: [NamedSensor; 7] = [
    NamedSensor { name: "Fan #1", index: 0 },
    NamedSensor { name: "Fan #2", index: 1 },
    NamedSensor { name: "Fan #3", index: 2 },
    NamedSensor { name: "Fan #4", index: 3 },
    NamedSensor { name: "Fan #5", index: 4 },
    NamedSensor { name: "Fan #6", index: 5 },
    NamedSensor { name: "Fan #7", index: 6 },
];

const PROFILE_ASUS_AM5: BoardProfile = BoardProfile {
    id: "asus-am5-b850m",
    voltages: &ASUS_AM5_VOLTAGES,
    temperatures: &ASUS_AM5_TEMPERATURES,
    fans: &ASUS_AM5_FANS,
};

const PROFILE_DEFAULT: BoardProfile = BoardProfile {
    id: "nct6701d-default",
    voltages: &DEFAULT_VOLTAGES,
    temperatures: &DEFAULT_TEMPERATURES,
    fans: &DEFAULT_FANS,
};

// ── 纯函数解码（可单测） ─────────────────────────────────────────────────────

/// `Nct677X.cs:1561-1566` —— NCT6701D 温度解码。
/// 0x00 / 0xA0 / [0x7E,0x80] 是「无传感器」哨兵，其余按有符号字节（**不做 0.5 细分**，
/// 这一点和 NCT679x 的 `(sbyte)<<1 | halfbit` 路径不同）。
pub fn decode_nct6701_temperature(raw: u8) -> Option<f32> {
    match raw {
        0x00 | 0xA0 => None,
        0x7E..=0x80 => None,
        _ => Some(raw as i8 as f32),
    }
}

/// `Nct677X.cs:1003-1018` —— 13 位风扇计数 → RPM。
/// `None` = 测不准（计数太小），`Some(0.0)` = 风扇停转。
pub fn fan_rpm(high: u8, low: u8) -> Option<f32> {
    let count = ((high as u32) << 5) | ((low as u32) & 0x1F);
    if count >= MAX_FAN_COUNT {
        Some(0.0)
    } else if count >= MIN_FAN_COUNT {
        Some(FAN_RPM_NUMERATOR / count as f32)
    } else {
        None
    }
}

/// `Voltage.cs:15` + `SuperIOHardware.cs:6262` ——
/// `Vout = value + (value - Vf) * Ri / Rf`（value 已是 `0.008 × 寄存器值`）。
pub fn apply_divider(value: f32, ri: f32, rf: f32, vf: f32) -> f32 {
    value + (value - vf) * ri / rf
}

/// `Nct677X.cs:821-862` —— NCT6701D 的温度源复用解析。
///
/// 与其它 NCT67xx 不同，NCT6701D 的温度**按 source 归属**而不是按下标：
/// 带 `source_register` 的条目先读出真实 source，再把该寄存器读到的温度写进
/// **所有** source 相同的下标，并用掩码保证「先到的赢」；source 未被任何条目
/// 声明时整条跳过。
pub fn resolve_temperatures(read: &mut dyn FnMut(u16) -> u8) -> Vec<Option<f32>> {
    let n = NCT6701D_TEMP_SOURCES.len();
    let mut out: Vec<Option<f32>> = vec![None; n];
    let mut mask: u64 = 0;

    for i in 0..n {
        let entry = NCT6701D_TEMP_SOURCES[i];
        let Some(configured) = entry.source else {
            // Source 为 null 的条目：直接按下标赋值，不参与源掩码
            out[i] = if entry.register == 0 {
                None
            } else {
                decode_nct6701_temperature(read(entry.register))
            };
            continue;
        };

        let source = if entry.source_register > 0 {
            let s = read(entry.source_register);
            if !NCT6701D_TEMP_SOURCES
                .iter()
                .any(|e| e.source == Some(s))
            {
                continue; // 该 source 没有任何条目声明 → 丢弃
            }
            s
        } else {
            configured
        };

        let bit = 1u64 << source;
        if mask & bit != 0 || entry.register == 0 {
            continue;
        }
        let Some(value) = decode_nct6701_temperature(read(entry.register)) else {
            continue;
        };
        mask |= bit;
        for j in 0..n {
            if NCT6701D_TEMP_SOURCES[j].source == Some(source) {
                out[j] = Some(value);
            }
        }
    }
    out
}

// ── 芯片识别 ─────────────────────────────────────────────────────────────────

/// `LpcIO.cs:109-460` 的 Nuvoton/Winbond/Fintek 识别表（只保留能在本机/常见主板上
/// 出现的条目）；返回 (名字, 是否为 EC 空间芯片)。
pub fn identify_winbond(id: u8, revision: u8) -> Option<(&'static str, bool)> {
    let hi = revision & 0xF0;
    let (name, ec) = match (id, revision, hi) {
        (0x52, _, _) => ("Winbond W83627HF", false),
        (0x82, _, 0x80) => ("Winbond W83627THF", false),
        (0x85, 0x41, _) => ("Winbond W83687THF", false),
        (0x88, _, 0x50) | (0x88, _, 0x60) => ("Winbond W83627EHF", false),
        (0xA0, _, 0x20) => ("Winbond W83627DHG", false),
        (0xA5, _, 0x10) => ("Winbond W83667HG", false),
        (0xB0, _, 0x70) => ("Winbond W83627DHGP", false),
        (0xB3, _, 0x50) => ("Winbond W83667HGB", false),
        (0xB4, _, 0x70) => ("Nuvoton NCT6771F", false),
        (0xC3, _, 0x30) => ("Nuvoton NCT6776F", false),
        (0xC4, _, 0x50) => ("Nuvoton NCT610XD", false),
        (0xC5, _, 0x60) => ("Nuvoton NCT6779D", false),
        (0xC7, 0x32, _) => ("Nuvoton NCT6683D", true),
        (0xC8, 0x03, _) => ("Nuvoton NCT6791D", false),
        (0xC9, 0x11, _) => ("Nuvoton NCT6792D", false),
        (0xC9, 0x13, _) => ("Nuvoton NCT6792DA", false),
        (0xD1, 0x21, _) => ("Nuvoton NCT6793D", false),
        (0xD3, 0x52, _) => ("Nuvoton NCT6795D", false),
        (0xD4, 0x23, _) => ("Nuvoton NCT6796D", false),
        (0xD4, 0x2A, _) => ("Nuvoton NCT6796DR / NCT5585D", false),
        (0xD4, 0x51, _) => ("Nuvoton NCT6797D", false),
        (0xD4, 0x2B, _) => ("Nuvoton NCT6798D", false),
        (0xD4, 0x40) | (0xD4, 0x41) => ("Nuvoton NCT6686D", true),
        (0xD5, 0x92) => ("Nuvoton NCT6687D / NCT6687DR", true),
        (0xD8, 0x02) => ("Nuvoton NCT6799D / NCT6796DS", false),
        (0xD8, 0x06) => ("Nuvoton NCT6701D", false),
        _ => return None,
    };
    Some((name, ec))
}

// ── DMI 主板名（注册表 HKLM\HARDWARE\DESCRIPTION\System\BIOS） ───────────────

fn read_dmi_string(value_name: &str) -> Option<String> {
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ};

    let sub: Vec<u16> = "HARDWARE\\DESCRIPTION\\System\\BIOS\0".encode_utf16().collect();
    let key: Vec<u16> = value_name.encode_utf16().chain(std::iter::once(0)).collect();
    let mut buf = [0u16; 256];
    let mut len = (buf.len() * 2) as u32;
    unsafe {
        let rc = RegGetValueW(
            HKEY_LOCAL_MACHINE,
            sub.as_ptr(),
            key.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            buf.as_mut_ptr().cast(),
            &mut len,
        );
        if rc != ERROR_SUCCESS {
            return None;
        }
    }
    let chars = (len as usize / 2).saturating_sub(1);
    Some(String::from_utf16_lossy(&buf[..chars]))
}

/// 主板型号 → 命名档案。本机 `TUF GAMING B850M-PLUS WIFI7`（LHM 尚未收录）按 ASUS AM5 档处理。
fn board_profile() -> &'static BoardProfile {
    let manufacturer = read_dmi_string("BaseBoardManufacturer").unwrap_or_default();
    let product = read_dmi_string("BaseBoardProduct").unwrap_or_default();
    let m = manufacturer.to_uppercase();
    let p = product.to_uppercase();
    if (m.contains("ASUS") || m.contains("ASUSTEK"))
        && (p.contains("B850M") || p.contains("X870") || p.contains("B650M"))
    {
        &PROFILE_ASUS_AM5
    } else {
        &PROFILE_DEFAULT
    }
}

// ── SuperIO 实例 ─────────────────────────────────────────────────────────────

pub struct SuperIo {
    pawn: PawnIo,
    base: u16,
    chip_name: &'static str,
    /// 只有 NCT6701D 这一支是完整移植并实测过的
    supported: bool,
    profile: &'static BoardProfile,
}

impl SuperIo {
    /// 探测主板上的 SuperIO 芯片并打开运行时通道。需要管理员权限。
    pub fn open() -> Option<SuperIo> {
        let pawn = PawnIo::open(MODULE);
        let Some(pawn) = pawn else {
            ACCESS_DENIED.store(true, std::sync::atomic::Ordering::Relaxed);
            return None;
        };
        let (chip_name, reg_port, base) = detect(&pawn)?;
        let supported = chip_name == "Nuvoton NCT6701D";
        Some(SuperIo {
            pawn,
            base,
            chip_name,
            supported,
            profile: board_profile(),
        })
    }

    pub fn chip_name(&self) -> &'static str {
        self.chip_name
    }

    pub fn profile_id(&self) -> &'static str {
        self.profile.id
    }

    /// 该芯片是否在已移植范围内。
    pub fn is_supported(&self) -> bool {
        self.supported
    }

    fn pio_inb(&self, port: u16) -> u8 {
        self.pawn
            .execute("ioctl_pio_inb", &[port as i64], 1)
            .map_or(0xFF, |v| v[0] as u8)
    }

    fn pio_outb(&self, port: u16, value: u8) {
        self.pawn
            .execute("ioctl_pio_outb", &[port as i64, value as i64], 0);
    }

    /// `Nct677X.cs:1250-1260` —— bank/register 协议的运行时读。
    fn read_byte(&self, address: u16) -> u8 {
        let bank = (address >> 8) as u8;
        let register = (address & 0xFF) as u8;
        self.pio_outb(self.base + ADDRESS_REGISTER_OFFSET, BANK_SELECT_REGISTER);
        self.pio_outb(self.base + DATA_REGISTER_OFFSET, bank);
        self.pio_outb(self.base + ADDRESS_REGISTER_OFFSET, register);
        self.pio_inb(self.base + DATA_REGISTER_OFFSET)
    }

    fn read_voltage(&self, index: usize) -> Option<f32> {
        let register = *VOLTAGE_REGISTERS.get(index)?;
        let raw = self.read_byte(register);
        let value = 0.008 * raw as f32;
        if value <= 0.0 {
            return None;
        }
        // 电池监测：除数值有效外还要看 0x005D 的使能位（Nct677X.cs:787-789）
        if register == VOLTAGE_VBAT_REGISTER
            && (self.read_byte(VBAT_MONITOR_CONTROL_REGISTER) & 0x01) == 0
        {
            return None;
        }
        Some(value)
    }

    fn read_fan(&self, index: usize) -> Option<f32> {
        let register = *FAN_COUNT_REGISTERS.get(index)?;
        let high = self.read_byte(register);
        let low = self.read_byte(register + 1);
        fan_rpm(high, low)
    }

    /// 读一轮全部传感器（按档案命名，空值跳过，与 LHM `ActivateSensor` 行为一致）。
    pub fn read_sensors(&self) -> Option<Vec<BoardSensor>> {
        if !self.supported {
            return None;
        }
        // LHM `Nct677X.cs:775` 只等 10ms，拿不到就整轮跳过 —— 宁缺毋脏
        let _guard = IsaBusGuard::wait(10)?;

        let mut out = Vec::new();

        for def in self.profile.voltages {
            if def.hidden {
                continue;
            }
            if let Some(raw) = self.read_voltage(def.index) {
                out.push(BoardSensor {
                    name: def.name.to_string(),
                    kind: "voltage".to_string(),
                    value: apply_divider(raw, def.ri, def.rf, def.vf),
                    unit: "V".to_string(),
                });
            }
        }

        let temps = resolve_temperatures(&mut |address| self.read_byte(address));
        for def in self.profile.temperatures {
            if let Some(Some(value)) = temps.get(def.index) {
                out.push(BoardSensor {
                    name: def.name.to_string(),
                    kind: "temperature".to_string(),
                    value: *value,
                    unit: "°C".to_string(),
                });
            }
        }

        for def in self.profile.fans {
            if let Some(value) = self.read_fan(def.index) {
                out.push(BoardSensor {
                    name: def.name.to_string(),
                    kind: "fan".to_string(),
                    value,
                    unit: "RPM".to_string(),
                });
            }
        }

        Some(out)
    }

    /// 诊断用：把全部 28 个温度下标解码结果打出来（探针用）
    pub fn dump_temperature_indices(&self) -> Vec<Option<f32>> {
        resolve_temperatures(&mut |address| self.read_byte(address))
    }
}

/// 探测流程（`LpcIO.cs:471-500`）：
/// 依次在 0x2E/0x4E 上进入配置空间 → 读芯片 ID/版本 → 选 LDN 0x0B → 读运行时基址 → 退出。
fn detect(pawn: &PawnIo) -> Option<(&'static str, u16, u16)> {
    struct Lpc<'a>(&'a PawnIo);
    impl Lpc<'_> {
        fn inb(&self, port: u16) -> u8 {
            self.0
                .execute("ioctl_pio_inb", &[port as i64], 1)
                .map_or(0xFF, |v| v[0] as u8)
        }
        fn outb(&self, port: u16, value: u8) {
            self.0
                .execute("ioctl_pio_outb", &[port as i64, value as i64], 0);
        }
        fn superio_inb(&self, register: u8) -> u8 {
            self.0
                .execute("ioctl_superio_inb", &[register as i64], 1)
                .map_or(0xFF, |v| v[0] as u8)
        }
        fn superio_inw(&self, register: u8) -> u16 {
            self.0
                .execute("ioctl_superio_inw", &[register as i64], 1)
                .map_or(0xFFFF, |v| v[0] as u16)
        }
        fn superio_outb(&self, register: u8, value: u8) {
            self.0
                .execute("ioctl_superio_outb", &[register as i64, value as i64], 0);
        }
        /// `LpcPort.cs` WinbondNuvotonFintekEnter
        fn winbond_enter(&self, reg_port: u16) {
            self.outb(reg_port, 0x87);
            self.outb(reg_port, 0x87);
        }
        /// `LpcPort.cs` WinbondNuvotonFintekExit
        fn winbond_exit(&self, reg_port: u16) {
            self.outb(reg_port, 0xAA);
        }
    }

    let lpc = Lpc(pawn);
    for &reg_port in REGISTER_PORTS.iter() {
        let slot: i64 = if reg_port == 0x2E { 0 } else { 1 };
        pawn.execute("ioctl_select_slot", &[slot], 0);

        lpc.winbond_enter(reg_port);
        let id = lpc.superio_inb(CHIP_ID_REGISTER);
        let revision = lpc.superio_inb(CHIP_REVISION_REGISTER);
        let Some((name, ec)) = identify_winbond(id, revision) else {
            lpc.winbond_exit(reg_port);
            continue;
        };
        // 目前只有非 EC 空间的 bank/register 协议实现了读取
        if ec {
            lpc.winbond_exit(reg_port);
            continue;
        }

        pawn.execute("ioctl_find_bars", &[], 0);
        lpc.superio_outb(DEVICE_SELECT_REGISTER, WINBOND_NUVOTON_HARDWARE_MONITOR_LDN);
        // 部分 NCT6701D 固件把硬件监控逻辑设备关着（LpcIO.cs:471-473）
        if lpc.superio_inb(LOGICAL_DEVICE_ACTIVATE_REGISTER) == 0 {
            lpc.superio_outb(LOGICAL_DEVICE_ACTIVATE_REGISTER, 0x01);
        }
        let mut address = lpc.superio_inw(BASE_ADDRESS_REGISTER);
        let verify = lpc.superio_inw(BASE_ADDRESS_REGISTER);
        if address != verify || is_invalid_runtime_base(address) {
            let alternate = lpc.superio_inw(ALTERNATE_BASE_ADDRESS_REGISTER);
            if alternate != 0xFFFF && alternate != address {
                address = alternate;
            }
        }
        let lock = lpc.superio_inb(NUVOTON_HARDWARE_MONITOR_IO_SPACE_LOCK);
        if (lock & 0x10) != 0 {
            lpc.superio_outb(NUVOTON_HARDWARE_MONITOR_IO_SPACE_LOCK, lock & !0x10);
        }
        lpc.winbond_exit(reg_port);

        if is_invalid_runtime_base(address) {
            continue;
        }
        return Some((name, reg_port, address));
    }
    None
}

/// `LpcIO.cs` IsInvalidRuntimeBase：低于 0x100 或不合法的间距
fn is_invalid_runtime_base(addr: u16) -> bool {
    addr < 0x100 || (addr & 0xF007) != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nct6701_temperature_sentinels() {
        assert_eq!(decode_nct6701_temperature(0x00), None);
        assert_eq!(decode_nct6701_temperature(0xA0), None);
        assert_eq!(decode_nct6701_temperature(0x7E), None);
        assert_eq!(decode_nct6701_temperature(0x7F), None);
        assert_eq!(decode_nct6701_temperature(0x80), None);
        assert_eq!(decode_nct6701_temperature(0x2D), Some(45.0));
        // 有符号：0xC7 = -57
        assert_eq!(decode_nct6701_temperature(0xC7), Some(-57.0));
    }

    #[test]
    fn fan_count_decoding() {
        // 0x4B0 = 0x34, 0x4B1 = 0x01 → count = (0x34<<5)|0x01 = 1665 → 811 RPM
        let rpm = fan_rpm(0x34, 0x01).unwrap();
        assert!((rpm - 810.8).abs() < 1.0, "got {rpm}");
        // 0x4B8 = 0x1F, 0x4B9 = 0x0B → count = 1003 → 1346 RPM
        let rpm = fan_rpm(0x1F, 0x0B).unwrap();
        assert!((rpm - 1345.9).abs() < 1.0, "got {rpm}");
        // 0xFF/0x1F = 0x1FFF → 停转
        assert_eq!(fan_rpm(0xFF, 0x1F), Some(0.0));
        // 计数过小 → 测不准
        assert_eq!(fan_rpm(0x00, 0x05), None);
    }

    #[test]
    fn voltage_divider_matches_lhm() {
        // +5V: raw 0x7D=125 → 0.008*125 = 1.0 → Ri=4.02/Rf=1 → 5.02
        let v = apply_divider(0.008 * 125.0, 4.02, 1.0, 0.0);
        assert!((v - 5.02).abs() < 1e-3, "got {v}");
        // +12V: raw 0x7E=126 → 1.008 → Ri=10.98/Rf=1 → 12.077
        let v = apply_divider(0.008 * 126.0, 10.98, 1.0, 0.0);
        assert!((v - 12.077).abs() < 1e-2, "got {v}");
        // +3.3V: raw 0xD4=212 → 1.696 → Ri=Rf=34 → 3.392
        let v = apply_divider(0.008 * 212.0, 34.0, 34.0, 0.0);
        assert!((v - 3.392).abs() < 1e-3, "got {v}");
        // Vcore: Ri=0 → 原值
        let v = apply_divider(0.008 * 173.0, 0.0, 1.0, 0.0);
        assert!((v - 1.384).abs() < 1e-3, "got {v}");
    }

    /// 用本机提权探针抓到的真实寄存器值（bank 全转储）驱动解析器，
    /// 断言它给出与探针人工分析一致的结果。
    #[test]
    fn resolve_temperatures_on_real_dump() {
        // 只填温度解析真正会读到的寄存器；其余按 0xFF 兜底（解码后为 -1）
        let pairs: &[(u16, u8)] = &[
            (0x100, 0x1C), // → source = PECI_0_CAL(28)
            (0x073, 0x2D), // 45 → 归给下标 21（PECI_0_CAL）
            (0x491, 0x24), // 36 → 下标 1
            (0x490, 0x23), // 35 → 下标 2
            (0x492, 0x1A), // 26 → 下标 3
            (0x493, 0x13), // 19 → 下标 4
            (0x494, 0x12), // 18 → 下标 5
            (0x495, 0x1A), // 26 → 下标 6
            (0x027, 0x23),
            (0x621, 0x01), // AUXTIN4 复用 SYSTIN
            (0x672, 0x13),
            (0xC27, 0x04),
            (0x674, 0x12),
            (0xC28, 0x05),
            (0x676, 0x1A),
            (0xC29, 0x06),
            (0x678, 0x18), // 24 → 下标 7 与 11（都声明 AUXTIN4）
            (0xC2A, 0x07),
            (0x67A, 0x24),
            (0xC2B, 0x02),
            (0x405, 0x00),
            (0x406, 0x00),
            (0x407, 0x00),
            (0x408, 0x00),
            (0x150, 0x24),
            (0x622, 0x02),
            (0x670, 0x1A),
            (0xC26, 0x03),
            (0x419, 0x00),
            (0x41A, 0x00),
            (0x4F4, 0x2D), // 45 → 下标 21
            (0x4F5, 0x00), // 哨兵 → 下标 22 为 None
            (0x07B, 0x1A),
            (0x900, 0x03),
            (0x409, 0x00), // 哨兵 → 下标 26 为 None
            (0x4A2, 0x1E), // 30 → 下标 27
        ];
        let mut read = |address: u16| -> u8 {
            pairs
                .iter()
                .find(|(a, _)| *a == address)
                .map_or(0xFF, |(_, v)| *v)
        };
        let temps = resolve_temperatures(&mut read);

        assert_eq!(temps[21], Some(45.0), "PECI_0_CAL 应从 0x073 取到 45°C");
        assert_eq!(temps[1], Some(36.0));
        assert_eq!(temps[2], Some(35.0));
        assert_eq!(temps[3], Some(26.0));
        assert_eq!(temps[4], Some(19.0));
        assert_eq!(temps[5], Some(18.0));
        assert_eq!(temps[6], Some(26.0));
        // AUXTIN4(7) 的 source_register(0x621) 指向 SYSTIN，已被下标 2 占用 → 下标 7 保持空
        // 但下标 11 的 source_register(0xC2A) 指向 AUXTIN4，会把 0x678 的值写进下标 7 与 11
        assert_eq!(temps[7], Some(24.0));
        assert_eq!(temps[11], Some(24.0));
        assert_eq!(temps[22], None, "0x4F5 = 0x00 是哨兵");
        assert_eq!(temps[0], None, "下标 0 的 source 解析成 PECI_0_CAL，值只写给下标 21");
        assert_eq!(temps[27], Some(30.0));
        // 被掩码吃掉的重复源
        assert_eq!(temps[8], None);
        assert_eq!(temps[17], None);
        assert_eq!(temps[18], None);
        assert_eq!(temps[24], None);
    }

    #[test]
    fn runtime_base_validation() {
        assert!(is_invalid_runtime_base(0x00FF));
        assert!(!is_invalid_runtime_base(0x0290));
        // (addr & 0xF007) != 0 → 非法
        assert!(is_invalid_runtime_base(0x0204));
    }

    #[test]
    fn chip_identification() {
        assert_eq!(
            identify_winbond(0xD8, 0x06),
            Some(("Nuvoton NCT6701D", false))
        );
        assert_eq!(
            identify_winbond(0xD8, 0x02),
            Some(("Nuvoton NCT6799D / NCT6796DS", false))
        );
        assert!(identify_winbond(0xD5, 0x92).unwrap().1, "NCT6687D 是 EC 空间");
        assert_eq!(identify_winbond(0x00, 0x00), None);
    }
}
