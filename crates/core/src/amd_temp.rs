//! AMD Zen（17h/19h/1Ah）CPU 温度与功耗 —— 纯寄存器数学。
//! 算法参照 LibreHardwareMonitor Amd17Cpu.cs（MPL-2.0）：
//! - Tctl/Tdie：SMN 寄存器 THM_TCON_CUR_TMP，(raw >> 21) * 125 毫度；
//!   若 RANGE_SEL/TJ_SEL 置位则再 -49°C
//! - 每 CCD 温度：CCDx_TEMP 寄存器低 12 位，(raw * 125 - 305000) 毫度
//! - 整包功耗：RAPL，MSR_PKG_ENERGY_STAT 的差分 × 能量单位 ÷ 时间
//!
//! 本文件只做纯计算，寄存器 IO 由 PawnIo 提供 —— 方便单测覆盖。

#![cfg(target_arch = "x86_64")]

use crate::pawnio::PawnIo;

// ---- AMD MSR ----
pub const MSR_PWR_UNIT: u32 = 0xC001_0299;
pub const MSR_PKG_ENERGY_STAT: u32 = 0xC001_029B;
/// 每核能耗累计（EAX 低 32 位，会回绕）。LHM `Amd17Cpu.cs:823` 同名常量。
pub const MSR_CORE_ENERGY_STAT: u32 = 0xC001_029A;
/// HW P-state Status：PstateStat[24:22] / CurCpuVid[21:14] / CurCpuDfsId[13:8] / CurCpuFid[11:0]。
pub const MSR_HW_PSTATE_STATUS: u32 = 0xC001_0293;
/// APERF/MPERF（只读计数器），必须在目标核上读 —— 每核一套。
pub const MSR_APERF_RO: u32 = 0xC000_00E8;
pub const MSR_MPERF_RO: u32 = 0xC000_00E7;

// ---- SMN 寄存器（Zen）----
pub const SMN_THM_TCON_CUR_TMP: u32 = 0x0005_9800;
const F17H_M61H_CCD1_TEMP: u32 = 0x0005_9B08; // Raphael(0x61)/GraniteRidge(0x44)
const F17H_M70H_CCD1_TEMP: u32 = 0x0005_9954; // Zen2(0x71)/Zen3(0x21)/TR3000(0x31)

const TEMP_RANGE_SEL_MASK: u32 = 0x0008_0000;
const TEMP_TJ_SEL_MASK: u32 = 0x0003_0000;

#[derive(Clone, Copy, Debug)]
pub struct CpuIdentity {
    pub family: u32,
    pub model: u32,
}

/// CPUID(1) 解析 family/model（AMD：family = base + ext；model = (ext<<4)|base）。
/// 9850X3D 实测：family 0x1A（Zen 5），model 0x44（GraniteRidge）。
pub fn cpuid_family_model() -> CpuIdentity {
    // SAFETY: cpuid 无副作用
    let eax = core::arch::x86_64::__cpuid(1).eax;
    let base_f = (eax >> 8) & 0xF;
    let ext_f = (eax >> 20) & 0xFF;
    let base_m = (eax >> 4) & 0xF;
    let ext_m = (eax >> 16) & 0xF;
    let family = if base_f == 0xF { base_f + ext_f } else { base_f };
    let model = if base_f == 0xF { (ext_m << 4) | base_m } else { base_m };
    CpuIdentity { family, model }
}

/// 是否为 AMD Zen（17h 及以后）——决定 PawnIO 温度通路是否适用。
pub fn is_zen(id: CpuIdentity) -> bool {
    id.family >= 0x17
}

/// Tctl/Tdie ℃。raw = THM_TCON_CUR_TMP 寄存器值。
pub fn tctl_from_raw(raw: u32) -> f32 {
    let offset_flag =
        (raw & TEMP_RANGE_SEL_MASK) != 0 || (raw & TEMP_TJ_SEL_MASK) == TEMP_TJ_SEL_MASK;
    let mut t = (raw >> 21) as f32 * 125.0 * 0.001;
    if offset_flag {
        t -= 49.0;
    }
    t
}

/// 单个 CCD 温度 ℃。raw = CCDx_TEMP 寄存器值。
pub fn ccd_temp_from_raw(raw: u32) -> f32 {
    let raw = raw & 0xFFF;
    (raw as f32 * 125.0 - 305_000.0) * 0.001
}

/// 该型号是否支持每 CCD 温度，以及 CCD1 寄存器基址。
pub fn ccd_layout(id: CpuIdentity) -> Option<u32> {
    if !is_zen(id) {
        return None;
    }
    match id.model {
        0x61 | 0x44 => Some(F17H_M61H_CCD1_TEMP),
        0x71 | 0x21 | 0x31 => Some(F17H_M70H_CCD1_TEMP),
        _ => None, // Zen/Zen+ 无每 CCD 温度
    }
}

pub const CCD_COUNT: u32 = 8; // 与 LHM 一致：最多 8 个 CCD 槽位

/// 整包能耗（EAX 低 32 位，会回绕）。
pub fn read_pkg_energy(p: &PawnIo) -> Option<u32> {
    p.read_msr(MSR_PKG_ENERGY_STAT).map(|v| (v & 0xFFFF_FFFF) as u32)
}

/// ESU → 每个增量代表的能量（**焦耳**）。
/// AMD PPR 与 Intel SDM 一致：MSR_PWR_UNIT[12:8] = ESU，能量单位 = 1/2^ESU **焦耳**。
/// 注意 LHM `Amd17Cpu.cs:253` 把这句注释写成 "micro Joule" 是**误导**：它 266-267 行
/// 直接 `energy = unit * delta; energy /= dt` 当瓦特用，只有按焦耳理解才量纲自洽
/// （µJ 理解会差 1e6 倍）。
pub fn esu_to_unit_j(esu: u32) -> f64 {
    2f64.powi(-(esu as i32))
}

/// RAPL 能量单位（焦耳/增量）：ESU = MSR_PWR_UNIT[12:8]。
pub fn read_energy_unit_j(p: &PawnIo) -> Option<f64> {
    p.read_msr(MSR_PWR_UNIT)
        .map(|v| esu_to_unit_j(((v >> 8) & 0x1F) as u32))
}

/// 差分功耗（W）。dt_s <= 0 或首采样（last=None）返回 None。
pub fn calc_power_w(
    last: Option<(u32, std::time::Instant)>,
    energy: u32,
    unit_j: f64,
) -> Option<f32> {
    let (last_e, t0) = last?;
    let dt = t0.elapsed().as_secs_f64();
    if dt <= 0.0 {
        return None;
    }
    let delta = energy.wrapping_sub(last_e);
    // 增量 × 焦耳/增量 = 焦耳；焦耳 ÷ 秒 = 瓦特。此处**不做** 1e6 缩放。
    Some((delta as f64 * unit_j / dt) as f32)
}

// ---- 每核时钟 / 每核功耗（LHM `Amd17Cpu.Core` + `CpuThread`）----
//
// LHM 的模型是「瞬时 P-state 频率」与「平均有效频率」两个传感器并存，本项目照搬：
//   Core #N (Clock)           = CurCpuFid 推出的 CoreCOF（瞬时，来自 0xC0010293）
//   Core #N (Effective)       = APERF 增量 ÷ 采样窗口（平均值，含被 halt 掉的时间）
// 本机实测（Zen5 / 9850X3D）满载单核时 CoreCOF=5480MHz、APERF 实测=5510MHz 吻合；
// 空载时 CoreCOF 仍报 ~5.6GHz 而 APERF 只有几百 MHz —— 两者语义不同，不是 bug。

/// HW P-state Status 的 PstateStat[24:22]。
pub fn pstate_index(eax: u32) -> u32 {
    (eax >> 22) & 0x7
}

/// Zen5（family 0x1A）CoreCOF = CpuFid[11:0] × 5 MHz。
/// 依据 AMD PPR 57896-B0-PUB_3.00（LHM `Amd17Cpu.cs:737-748` 同）。
pub fn core_clock_mhz_zen5(eax: u32) -> f64 {
    (eax & 0xFFF) as f64 * 5.0
}

/// Zen1..4 CoreCOF = (CpuFid[7:0] / CpuDfsId[13:8]) × 2 × 总线频率（LHM `Amd17Cpu.cs:750-763`）。
/// DfsId 为 0 时无法计算，返回 0（调用方按 0 视为无数据）。
pub fn core_clock_mhz_legacy(eax: u32, bus_mhz: f64) -> f64 {
    let dfs = ((eax >> 8) & 0x3F) as f64;
    let fid = (eax & 0xFF) as f64;
    if dfs <= 0.0 {
        return 0.0;
    }
    fid / dfs * bus_mhz * 2.0
}

/// LHM 只在 `AperfDelta < MperfDelta`（比值 < 1）时按比例折算 Core Clock。
/// 比值 > 1 说明核跑在 MPERF 参考频率之上，LHM 选择不折算，这里保持一致。
pub fn ratio_adjusted_clock_mhz(core_clock_mhz: f64, aperf_delta: u64, mperf_delta: u64) -> f64 {
    if mperf_delta > 0 && aperf_delta < mperf_delta {
        (aperf_delta as f64 / mperf_delta as f64) * core_clock_mhz
    } else {
        core_clock_mhz
    }
}

/// 有效频率（MHz）= APERF 增量 ÷ 采样窗口（µs）。
/// APERF 在核被 halt/clock-gate 时不计数，所以这个值是「摊到墙钟时间的平均频率」。
pub fn effective_clock_mhz(aperf_delta: u64, window_us: f64) -> f64 {
    if window_us <= 0.0 {
        return 0.0;
    }
    aperf_delta as f64 / window_us
}

/// 计数器差分。倒挂（回绕）或超出 20000e6 视为无效 → None，调用方重置基线
/// （LHM `Amd17Cpu.cs:579-607`：回绕或 delta 过大时整轮丢弃而不是算出一个负频率）。
pub fn counter_delta(now: u64, last: u64) -> Option<u64> {
    if now < last {
        return None;
    }
    let d = now - last;
    if d > 20_000_000_000 {
        return None;
    }
    Some(d)
}

/// 每核功耗（W）。能量单位与整包 RAPL 共用同一条 `MSR_PWR_UNIT`。
pub fn calc_core_power_w(delta_energy: u64, unit_j: f64, dt_s: f64) -> Option<f32> {
    if dt_s <= 0.0 {
        return None;
    }
    Some((delta_energy as f64 * unit_j / dt_s) as f32)
}

/// 读每核能耗计数器（EAX 低 32 位）。**必须在目标核上调用。**
pub fn read_core_energy(p: &PawnIo) -> Option<u32> {
    p.read_msr(MSR_CORE_ENERGY_STAT)
        .map(|v| (v & 0xFFFF_FFFF) as u32)
}

/// 读 HW P-state Status 的 EAX。
pub fn read_pstate_status(p: &PawnIo) -> Option<u32> {
    p.read_msr(MSR_HW_PSTATE_STATUS)
        .map(|v| (v & 0xFFFF_FFFF) as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tctl_plain() {
        // (raw >> 21) = 500 → 500 * 125 / 1000 = 62.5℃，无偏移标志
        let raw = 500u32 << 21;
        assert!((tctl_from_raw(raw) - 62.5).abs() < 0.01);
    }

    #[test]
    fn tctl_with_range_sel() {
        // 置位 bit19 (RANGE_SEL) → 100℃ - 49 = 51℃
        let raw = (800u32 << 21) | 0x8_0000;
        assert!((tctl_from_raw(raw) - 51.0).abs() < 0.01);
    }

    #[test]
    fn tctl_tj_sel() {
        // TJ_SEL[17:16] = 0b11 → 视为需 -49
        let raw = (800u32 << 21) | 0x3_0000;
        assert!((tctl_from_raw(raw) - 51.0).abs() < 0.01);
    }

    #[test]
    fn ccd_temp() {
        // 45℃ 对应 raw = (45 + 305) * 8 = 2800
        assert!((ccd_temp_from_raw(2800) - 45.0).abs() < 0.01);
        // 无效小值会被算出极端温度，由调用方按范围过滤
    }

    #[test]
    fn esu_to_unit() {
        assert!((esu_to_unit_j(0) - 1.0).abs() < 1e-12);
        assert!((esu_to_unit_j(20) - 9.536_743_164_062_5e-7).abs() < 1e-18);
    }

    #[test]
    fn power_math() {
        // ESU=20 → 2^-20 J/增量；1 秒内 52_428_800 个增量 = 50 J → 50 W
        let unit = esu_to_unit_j(20);
        let past = std::time::Instant::now() - std::time::Duration::from_secs(1);
        let last = Some((0u32, past));
        let p = calc_power_w(last, 52_428_800, unit).unwrap();
        assert!((p - 50.0).abs() < 2.5, "power={p}");
    }

    #[test]
    fn power_wraps_32bit() {
        // 计数器低 32 位回绕：0xFFFF_FFF0 → 0x20 应算作 +0x30 个增量而非负数
        let unit = 1.0;
        let past = std::time::Instant::now() - std::time::Duration::from_secs(1);
        let p = calc_power_w(Some((0xFFFF_FFF0u32, past)), 0x20, unit).unwrap();
        assert!((p - 48.0).abs() < 2.5, "power={p}");
    }

    #[test]
    fn power_needs_baseline() {
        assert!(calc_power_w(None, 1234, 1.0).is_none());
    }

    #[test]
    fn identity() {
        let id = cpuid_family_model();
        // 本机 9850X3D：family 0x1A, model 0x44。其他机器跑测试也不 fail，
        // 只验证字段自洽。
        assert!(id.family >= 0x0F);
        assert!(id.model <= 0xFF);
    }

    #[test]
    fn zen5_core_clock_from_real_probe_value() {
        // 本机实测（满载 core 7）：0xC0010293 EAX = 0x00350448
        // fid[11:0] = 0x448 = 1096 → 5480 MHz，同窗口 APERF 实测 5510 MHz（差 0.5%）
        let eax = 0x0035_0448u32;
        assert_eq!(pstate_index(eax), 0);
        assert!((core_clock_mhz_zen5(eax) - 5480.0).abs() < 0.01);
        assert_eq!((eax >> 8) & 0x3F, 0x04); // dfsid，仅记录
    }

    #[test]
    fn zen5_core_clock_idle_probe_value() {
        // 空载实测 EAX = 0x0036445A → fid 0x45A = 1114 → 5570 MHz
        assert!((core_clock_mhz_zen5(0x0036_445A) - 5570.0).abs() < 0.01);
    }

    #[test]
    fn legacy_core_clock_math() {
        // fid=0x30(48) dfs=1 → 48/1*200 = 9600 MHz（纯公式验证，非本机读数）
        let eax = (1u32 << 8) | 0x30;
        assert!((core_clock_mhz_legacy(eax, 100.0) - 9600.0).abs() < 0.01);
        // dfs=0 无法计算
        assert_eq!(core_clock_mhz_legacy(0x30, 100.0), 0.0);
    }

    #[test]
    fn ratio_adjustment_matches_lhm() {
        // ratio < 1 → 折算；ratio >= 1 → 原样
        assert!((ratio_adjusted_clock_mhz(5000.0, 400, 800) - 2500.0).abs() < 0.01);
        assert!((ratio_adjusted_clock_mhz(5000.0, 900, 800) - 5000.0).abs() < 0.01);
        assert!((ratio_adjusted_clock_mhz(5000.0, 900, 0) - 5000.0).abs() < 0.01);
    }

    #[test]
    fn effective_clock_math() {
        // 500_000 µs 内 2_750_000_000 个 APERF 增量 → 5500 MHz
        assert!((effective_clock_mhz(2_750_000_000, 500_000.0) - 5500.0).abs() < 0.01);
        assert_eq!(effective_clock_mhz(123, 0.0), 0.0);
    }

    #[test]
    fn counter_delta_rejects_wrap_and_bogus_jumps() {
        assert_eq!(counter_delta(1000, 400), Some(600));
        assert_eq!(counter_delta(400, 1000), None); // 回绕/倒挂
        assert_eq!(counter_delta(20_000_000_001, 0), None); // 超界
        assert_eq!(counter_delta(20_000_000_000, 0), Some(20_000_000_000)); // 边界内
        assert_eq!(counter_delta(5, 5), Some(0));
    }

    #[test]
    fn core_power_math() {
        // ESU=16 → 2^-16 J/增量；1 秒内 458_752 个增量 ≈ 7.0 W（本机探针实测量级）
        let unit = esu_to_unit_j(16);
        let w = calc_core_power_w(458_752, unit, 1.0).unwrap();
        assert!((w - 7.0).abs() < 0.05, "power={w}");
        assert!(calc_core_power_w(100, unit, 0.0).is_none());
    }
}
