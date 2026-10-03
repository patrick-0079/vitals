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
}
