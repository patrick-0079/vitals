//! 内存模组 SPD 直读（DDR5 温度/型号/序列号）。
//!
//! LHM 自己**不实现** SPD 读取，而是把整件事委托给 NuGet 库 RAMSPDToolkit
//! （`LibreHardwareMonitorLib\RAMSPDToolkitDriver.cs` 只实现 `IPawnIODriver`）。
//! 所以这里的对照源码是 `.ref\RAMSPDToolkit\RAMSPDToolkit-master\RAMSPDToolkit\`，
//! 传输层则落在本 crate 的 [`crate::smbus`]（`SmbusPIIX4.bin`）。
//!
//! # 地址空间（本机真机实测确认，是整个 DDR5 通路最容易搞错的地方）
//!
//! SPD5 hub 在**同一个 7 位从机地址**上暴露两套完全不同的空间，靠 bit7 区分：
//!
//! | 空间 | command/offset | 内容 | 读法 |
//! |---|---|---|---|
//! | MR 寄存器 | 原样 `0x00..=0x7F` | 设备类型、页寄存器、温度、状态、能力位 | `read_byte_data` / `read_word_data` |
//! | EEPROM 页 | `(addr & 0x7F) \| 0x80` | SPD 数据本体（2048 B，8 页 × 128 B） | `read_byte_data`（逐字节） |
//!
//! 子 agent 读源码时曾把「`IsAvailable` 不置 bit7 而 `At()` 置 bit7」列为疑点，
//! 实测证明两者本就该不同：检测读的是 MR 空间，读 SPD 数据走的才是 EEPROM 页空间。
//!
//! # 页切换
//!
//! DDR5 没有 DDR4 那种 `0x36+page` 伪从机：页号写在 **MR11（`0x0B`）的低 3 位**，
//! 写入后作用于 `|0x80` 的 EEPROM 空间。`At(addr)` 的换算是
//! `page = addr >> 7`、`offset = (addr & 0x7F) | 0x80`。
//!
//! # 温度
//!
//! 温度在 **MR 空间偏移 `0x31`、16 位、小端、不交换字节**，且必须在页 0
//! （MR11 是页寄存器，与温度同属 volatile 数据）。换算见 [`decode_temperature`]。
//!
//! 本机实测：`0x51` → `0x021C` = 33.75 °C、`0x53` → `0x0200` = 32.00 °C。

#![cfg(windows)]

use crate::schema::DimmMetrics;
use crate::smbus::{
    self, Piix4, SPD_BEGIN, SPD_CFG_RETRIES, SPD_DATA_RETRIES, SPD_END, SPD_TS_RETRIES,
};

// ---------------------------------------------------------------------------
// DDR5 MR 寄存器空间（**不置 bit7**）
// ---------------------------------------------------------------------------

/// 设备类型（高位字节），期望 `0x51`。
pub const MR_DEVICE_TYPE_MOST: u8 = 0x00;
/// 设备类型（低位字节），期望 `0x18`。
pub const MR_DEVICE_TYPE_LEAST: u8 = 0x01;
/// 设备能力位（bit1 = 有温度传感器，本机 `0x03`）。
pub const MR_DEVICE_CAPABILITY: u8 = 0x05;
/// 写恢复时间（决定页切换后要等多久）。
pub const MR_WRITE_RECOVERY_TIME: u8 = 0x06;
/// 虚拟页寄存器（页号在低 3 位）。
pub const MR_PAGE: u8 = 0x0B;
/// 温度传感器使能位（`0` = 使能）。
pub const MR_THERMAL_SENSOR_ENABLED: u8 = 0x1A;
/// 温度高限。
pub const MR_HIGH_LIMIT: u8 = 0x1C;
/// 温度低限。
pub const MR_LOW_LIMIT: u8 = 0x1E;
/// 温度临界高限。
pub const MR_CRITICAL_HIGH: u8 = 0x20;
/// 温度临界低限。
pub const MR_CRITICAL_LOW: u8 = 0x22;
/// 当前温度（16 位，不交换字节）。
pub const MR_TEMPERATURE: u8 = 0x31;
/// 温度传感器状态字。
pub const MR_THERMAL_SENSOR_STATUS: u8 = 0x33;

// ---------------------------------------------------------------------------
// DDR5 SPD EEPROM 绝对地址（页 = addr >> 7，页内偏移 = (addr & 0x7F) | 0x80）
// ---------------------------------------------------------------------------

/// SPD 字节总数 / SPD 修订版本。
pub const SPD_REVISION: u16 = 0x001;
/// 内存类型（`0x12` = DDR5 SDRAM）。
pub const SPD_MEMORY_TYPE: u16 = 0x002;
/// 模组厂商 JEP106 延续码（bank，bit7 是奇校验位，需清掉）。
pub const SPD_MODULE_MANUFACTURER_CONTINUATION: u16 = 0x200;
/// 模组厂商 JEP106 ID。
pub const SPD_MODULE_MANUFACTURER_ID: u16 = 0x201;
/// 模组制造地点。
pub const SPD_MODULE_MANUFACTURING_LOCATION: u16 = 0x202;
/// 模组制造日期：年（BCD）。
pub const SPD_MODULE_MANUFACTURING_DATE_YEAR: u16 = 0x203;
/// 模组制造日期：周（BCD）。
pub const SPD_MODULE_MANUFACTURING_DATE_WEEK: u16 = 0x204;
/// 模组序列号（4 字节）。
pub const SPD_MODULE_SERIAL_BEGIN: u16 = 0x205;
/// 模组序列号结束。
pub const SPD_MODULE_SERIAL_END: u16 = 0x208;
/// 模组型号（30 字节 ASCII，未用位填充 `0x20`）。
pub const SPD_MODULE_PART_NUMBER_BEGIN: u16 = 0x209;
/// 模组型号结束。
pub const SPD_MODULE_PART_NUMBER_END: u16 = 0x226;
/// 模组修订码。
pub const SPD_MODULE_REVISION_CODE: u16 = 0x227;
/// DRAM 厂商 JEP106 延续码。
pub const SPD_DRAM_MANUFACTURER_CONTINUATION: u16 = 0x228;
/// DRAM 厂商 JEP106 ID。
pub const SPD_DRAM_MANUFACTURER_ID: u16 = 0x229;

/// DDR5 SPD 总长度（字节）。
pub const EEPROM_LENGTH: u16 = 2048;
/// 页大小位移。
pub const PAGE_SHIFT: u16 = 7;
/// 每页字节数。
pub const PAGE_SIZE: usize = 128;
/// 最后一页页号。
pub const PAGE_MAX: u8 = 7;
/// 型号字段的填充字符。
pub const PART_NUMBER_UNUSED: u8 = 0x20;

/// 帧内每次 EEPROM 字节读之后的等待（RAMSPDToolkit 为 1 ms）。
const EEPROM_READ_DELAY_MS: u64 = 1;

// ---------------------------------------------------------------------------
// 纯解码函数
// ---------------------------------------------------------------------------

/// SPD 原始温度 → ℃（`SPDTemperatureConverter.CheckAndConvertTemperature`）。
///
/// bit12 是符号位：置位时清掉该位再乘 0.0625 然后减 256；否则直接乘 0.0625。
pub fn decode_temperature(raw: u16) -> f32 {
    if raw & 0x1000 != 0 {
        ((raw & !0x1000) as f32) * 0.0625 - 256.0
    } else {
        (raw as f32) * 0.0625
    }
}

/// 型号字段：丢掉 `0x20` 填充、在首个 NUL 处截断，得到 ASCII 型号串。
pub fn decode_part_number(bytes: &[u8]) -> String {
    let mut out = String::new();
    for &b in bytes {
        if b == 0 || b == PART_NUMBER_UNUSED {
            continue;
        }
        if b.is_ascii_graphic() || b == b' ' {
            out.push(b as char);
        }
    }
    out.trim().to_string()
}

/// 序列号：`At(0x205, 4)` 的 4 个字节按顺序大写十六进制拼接（如 `EB7F58B8`）。
pub fn decode_serial(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push_str(&format!("{b:02X}"));
    }
    out
}

/// BCD 字节 → 十进制（`BinaryHandler.NormalizeBcd`）。
pub fn decode_bcd(value: u8) -> u8 {
    (value >> 4) * 10 + (value & 0x0F)
}

/// 制造日期：`2024-W05` 形式；年为 0 时表示未编码。
pub fn format_manufacture_date(year_bcd: u8, week_bcd: u8) -> Option<String> {
    if year_bcd == 0 {
        return None;
    }
    Some(format!(
        "{}-W{:02}",
        2000 + decode_bcd(year_bcd) as u32,
        decode_bcd(week_bcd)
    ))
}

/// 温度状态字 → 名字。判定优先级照抄 `DDR5Accessor.Update`（bit2/3 最优先）。
pub fn thermal_status_name(status: u8) -> &'static str {
    if status & 0x04 != 0 {
        "AboveCriticalHighLimit"
    } else if status & 0x08 != 0 {
        "BelowCriticalLowLimit"
    } else if status & 0x01 != 0 {
        "AboveHighLimit"
    } else if status & 0x02 != 0 {
        "BelowLowLimit"
    } else {
        "Good"
    }
}

/// 设备类型是否为 DDR5 SPD5 hub（`0x51` / `0x18`）。
pub fn is_ddr5(mr0: u8, mr1: u8) -> bool {
    mr0 == 0x51 && mr1 == 0x18
}

/// 能力位是否有温度传感器（bit1）。
pub fn has_thermal_sensor(capability: u8) -> bool {
    capability & 0x02 != 0
}

/// 写恢复时间（毫秒）。`timeUnit` 取 bit0..1、`recUnit` 取 bit4..7。
pub fn write_recovery_ms(raw: u8) -> u32 {
    let time_unit = raw & 0x03;
    let rec_unit = (raw >> 4) & 0x0F;
    let base: u32 = match rec_unit {
        0..=10 => rec_unit as u32,
        0x0B => 50,
        0x0C => 100,
        0x0D => 200,
        0x0E => 500,
        _ => 0,
    };
    // timeUnit: 0 = ns、1 = µs、2 = ms、3 = 保留（按 ms 处理）
    let ms = match time_unit {
        0 => (base as f64) / 1_000_000.0,
        1 => (base as f64) / 1_000.0,
        _ => base as f64,
    };
    ms.ceil() as u32
}

/// SPD 的厂商延续码 → JEP106 表 bank 号。
///
/// bit7 是奇校验位（Micron 实测读出 `0x80`），清掉后**再加 1** 才是表里的 bank
/// （`SPDAccessor.cs:344` 的 `TryGetValue((byte)(continuation + 1))`）。
pub fn manufacturer_bank(continuation: u8) -> u8 {
    ((continuation & 0x7F) as u16 + 1).min(u8::MAX as u16) as u8
}

/// JEP106 厂商名查表（只收模组/DRAM 常见厂，其余回退成十六进制）。
///
/// `bank` 是**表里的 bank 号**，等于 SPD 读到的延续码（清掉 bit7 奇校验位）+ 1：
/// `SPDAccessor.cs:344` 正是用 `ManufacturerBanks.TryGetValue((byte)(continuation + 1))`。
/// 本机 Micron 的延续码读出 `0x80`（清 bit7 → `0x00`），故 bank = 1，命中 `0x2C`。
///
/// 完整表在 `RAMSPDToolkit\SPD\Mappings\ManufacturerMapping.cs`（17 个 bank、
/// 2270 行，JEP106BN Jan 2026）；这里保持精简，未命中时由调用方格式化
/// `0x{bank:02X}/0x{id:02X}`，不猜名字。
pub fn jedec_manufacturer(bank: u8, id: u8) -> Option<&'static str> {
    match (bank, id) {
        (1, 0x01) => Some("AMD"),
        (1, 0x02) => Some("AMI"),
        (1, 0x04) => Some("RAMXEED Limited"),
        (1, 0x2C) => Some("Micron Technology"),
        (1, 0xAD) => Some("SK Hynix"),
        (1, 0xAE) => Some("OKI Semiconductor"),
        (1, 0xCE) => Some("Samsung"),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// DDR5 访问器
// ---------------------------------------------------------------------------

/// 一条 DDR5 模组的 SPD 访问器：负责页切换与两种地址空间的读写。
///
/// 每次 [`Ddr5::at`] 会读 1 ms 延时（与 RAMSPDToolkit 的 `Thread.Sleep(1)` 一致），
/// 批量读字段（型号 30 字节）因此有约 30 ms 量级开销 —— 身份信息只在启动时读一次。
pub struct Ddr5<'a> {
    bus: &'a Piix4,
    address: u8,
    page: u8,
    write_recovery_ms: u32,
}

impl<'a> Ddr5<'a> {
    /// 在 `address`（0x50..=0x57）上探测 DDR5 SPD5 hub。
    ///
    /// 照抄 `DDR5Accessor.IsAvailable`：先把页归零（页非 0 时 EEPROM 空间读到的是
    /// 别的页，检测仍会过但后续字段全错），再读 MR0/MR1 必须等于 `0x51`/`0x18`。
    pub fn detect(bus: &'a Piix4, address: u8) -> Option<Ddr5<'a>> {
        let page = bus.read_byte_data(address, MR_PAGE).map(|v| v & 0x07).unwrap_or(0);
        if page != 0 {
            let _ = bus.write_byte_data(address, MR_PAGE, 0);
        }
        let mr0 = bus.read_byte_data(address, MR_DEVICE_TYPE_MOST).ok()?;
        let mr1 = bus.read_byte_data(address, MR_DEVICE_TYPE_LEAST).ok()?;
        if !is_ddr5(mr0, mr1) {
            return None;
        }
        Some(Ddr5 {
            bus,
            address,
            page: 0,
            write_recovery_ms: write_recovery_ms_opt(bus, address),
        })
    }

    /// SPD 从机地址。
    pub fn address(&self) -> u8 {
        self.address
    }

    /// 相对 [`crate::smbus::SPD_BEGIN`] 的槽位序号。
    pub fn index(&self) -> u8 {
        self.address.wrapping_sub(smbus::SPD_BEGIN)
    }

    /// 当前页号。
    pub fn page(&self) -> u8 {
        self.page
    }

    /// 切换 EEPROM 页（写 MR11 低 3 位），并按写恢复时间等待。
    pub fn set_page(&mut self, page: u8) -> Result<(), i32> {
        let page = page & PAGE_MAX;
        if page == self.page {
            return Ok(());
        }
        self.bus.write_byte_data(self.address, MR_PAGE, page)?;
        let delay = self.write_recovery_ms.max(1) as u64;
        std::thread::sleep(std::time::Duration::from_millis(delay));
        self.page = page;
        Ok(())
    }

    /// 读 EEPROM 绝对地址（`At()`）。越界返回 `0xFF`（RAMSPDToolkit 同）。
    pub fn at(&mut self, address: u16) -> Result<u8, i32> {
        if address >= EEPROM_LENGTH {
            return Ok(0xFF);
        }
        self.set_page((address >> PAGE_SHIFT) as u8)?;
        let offset = ((address & 0x7F) as u8) | 0x80;
        let value = self.bus.read_byte_data(self.address, offset)?;
        std::thread::sleep(std::time::Duration::from_millis(EEPROM_READ_DELAY_MS));
        Ok(value)
    }

    /// 连续读 EEPROM（含起止两端），**逐字节**读。任何一字节失败即整体失败。
    pub fn at_range(&mut self, begin: u16, end: u16) -> Result<Vec<u8>, i32> {
        let mut out = Vec::with_capacity((end - begin + 1) as usize);
        for addr in begin..=end {
            out.push(self.at(addr)?);
        }
        Ok(out)
    }

    /// 读 MR 寄存器空间（**不置 bit7**）。
    pub fn mr(&self, reg: u8) -> Result<u8, i32> {
        self.bus.read_byte_data(self.address, reg)
    }

    /// 读 MR 寄存器空间 16 位小端（温度用）。
    pub fn mr_word(&self, reg: u8) -> Result<u16, i32> {
        self.bus.read_word_data(self.address, reg)
    }

    /// 读当前温度：先归零页（volatile 数据），再带重试读 MR `0x31`。
    pub fn temperature(&mut self) -> Result<f32, i32> {
        self.set_page(0)?;
        let raw = self
            .bus
            .read_word_data_retry(self.address, MR_TEMPERATURE, SPD_TS_RETRIES)?;
        Ok(decode_temperature(raw))
    }

    /// 读温度状态字（须先归零页）。
    pub fn thermal_status(&mut self) -> Result<&'static str, i32> {
        self.set_page(0)?;
        let raw = self
            .bus
            .read_byte_data_retry(self.address, MR_THERMAL_SENSOR_STATUS, SPD_DATA_RETRIES)?;
        Ok(thermal_status_name(raw))
    }

    /// 读能力位，判断是否带温度传感器（须先归零页）。
    pub fn thermal_sensor_present(&mut self) -> Result<bool, i32> {
        self.set_page(0)?;
        let raw = self
            .bus
            .read_byte_data_retry(self.address, MR_DEVICE_CAPABILITY, SPD_CFG_RETRIES)?;
        Ok(has_thermal_sensor(raw))
    }

    /// 读整页 EEPROM（128 字节）——排障/对拍用。
    pub fn dump_page(&mut self, page: u8) -> Result<Vec<u8>, i32> {
        self.set_page(page)?;
        let mut out = Vec::with_capacity(PAGE_SIZE);
        for i in 0..PAGE_SIZE as u8 {
            out.push(self.bus.read_byte_data(self.address, i | 0x80)?);
            std::thread::sleep(std::time::Duration::from_millis(EEPROM_READ_DELAY_MS));
        }
        Ok(out)
    }

    /// 组装一条模组的完整读数（身份 + 温度）。字段读失败按缺失处理，不中断。
    pub fn read_metrics(&mut self) -> DimmMetrics {
        let mut m = DimmMetrics {
            index: self.index(),
            address: self.address,
            source: "ddr5-spd".to_string(),
            ..Default::default()
        };

        // 温度与状态属于 volatile MR 空间，先读它们并保持页 0。
        match self.thermal_sensor_present() {
            Ok(true) => match self.temperature() {
                Ok(t) => m.temp_c = Some(t),
                Err(_) => m.source = "none".to_string(),
            },
            Ok(false) => m.source = "none".to_string(),
            Err(_) => m.source = "none".to_string(),
        }
        m.thermal_status = self
            .thermal_status()
            .map(|s| s.to_string())
            .unwrap_or_else(|_| "Unknown".to_string());

        // 身份信息在 EEPROM 空间（型号/序列号在第 4 页）。
        if let Ok(bytes) = self.at_range(SPD_MODULE_PART_NUMBER_BEGIN, SPD_MODULE_PART_NUMBER_END) {
            m.part_number = decode_part_number(&bytes);
        }
        if let Ok(bytes) = self.at_range(SPD_MODULE_SERIAL_BEGIN, SPD_MODULE_SERIAL_END) {
            m.serial_number = decode_serial(&bytes);
        }
        if let (Ok(raw_bank), Ok(id)) = (
            self.at(SPD_MODULE_MANUFACTURER_CONTINUATION),
            self.at(SPD_MODULE_MANUFACTURER_ID),
        ) {
            // 表里 bank = 延续码 + 1（`SPDAccessor.cs:344`），bit7 是奇校验位要清掉
            let bank = manufacturer_bank(raw_bank);
            m.manufacturer = jedec_manufacturer(bank, id)
                .map(|s| s.to_string())
                .unwrap_or_else(|| format!("0x{bank:02X}/0x{id:02X}"));
        }
        if let (Ok(year), Ok(week)) = (
            self.at(SPD_MODULE_MANUFACTURING_DATE_YEAR),
            self.at(SPD_MODULE_MANUFACTURING_DATE_WEEK),
        ) {
            m.manufacture_date = format_manufacture_date(year, week);
        }
        if m.part_number.is_empty() && m.serial_number.is_empty() && m.temp_c.is_none() {
            m.source = "none".to_string();
        }
        m
    }
}

fn write_recovery_ms_opt(bus: &Piix4, address: u8) -> u32 {
    bus.read_byte_data(address, MR_WRITE_RECOVERY_TIME)
        .map(write_recovery_ms)
        .unwrap_or(1)
        .max(1)
}

// ---------------------------------------------------------------------------
// 对外入口
// ---------------------------------------------------------------------------

/// 扫描 SMBus 上的 0x50..=0x57，返回识别到的 DDR5 模组（含身份与温度）。
///
/// 无权限 / 没有 PIIX4 控制器 / 模块加载失败时返回空 Vec —— 与其它数据源一致，
/// 由宿主读 `SourcesStatus` 决定怎么提示。
pub fn poll() -> Vec<DimmMetrics> {
    let Some(bus) = Piix4::open(Some(0)) else {
        return Vec::new();
    };
    enumerate(&bus)
}

/// 用既有总线枚举模组。
pub fn enumerate(bus: &Piix4) -> Vec<DimmMetrics> {
    let mut out = Vec::new();
    for address in SPD_BEGIN..=SPD_END {
        if let Some(mut dimm) = Ddr5::detect(bus, address) {
            out.push(dimm.read_metrics());
        }
    }
    out
}

/// 只刷新温度与状态字（身份信息是静态的，不必重读 EEPROM）。
///
/// 每帧 2 次字读 + 1 次字节读，适合放进采样循环；失败时保持上一帧的值。
pub fn refresh_temperatures(bus: &Piix4, dimms: &mut [DimmMetrics]) {
    for dimm in dimms.iter_mut() {
        let mut acc = Ddr5 {
            bus,
            address: dimm.address,
            page: 0,
            write_recovery_ms: 1,
        };
        if let Ok(t) = acc.temperature() {
            dimm.temp_c = Some(t);
        }
        if let Ok(s) = acc.thermal_status() {
            dimm.thermal_status = s.to_string();
        }
    }
}

// ---------------------------------------------------------------------------
// 单测
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temperature_sign_and_scale() {
        // 本机实测 0x51 → 0x021C = 540 → 33.75 °C
        assert!((decode_temperature(0x021C) - 33.75).abs() < 1e-4);
        assert!((decode_temperature(0x0200) - 32.0).abs() < 1e-4);
        assert!((decode_temperature(0x0000) - 0.0).abs() < 1e-4);
        // bit12 置位 = 负温：0x1100 → (0x0100)*0.0625 - 256 = -240.0
        assert!((decode_temperature(0x1100) + 240.0).abs() < 1e-4);
    }

    #[test]
    fn part_number_strips_padding() {
        let raw = b"CP32G60C40U5W.M8B1             ";
        assert_eq!(decode_part_number(raw), "CP32G60C40U5W.M8B1");
    }

    #[test]
    fn part_number_stops_at_nul() {
        let mut raw = [0u8; 30];
        raw[..4].copy_from_slice(b"ABCD");
        raw[4] = 0x20;
        raw[5] = 0x20;
        assert_eq!(decode_part_number(&raw), "ABCD");
    }

    #[test]
    fn serial_is_uppercase_hex() {
        assert_eq!(decode_serial(&[0xEB, 0x7F, 0x58, 0xB8]), "EB7F58B8");
        assert_eq!(decode_serial(&[0x00, 0x0A]), "000A");
    }

    #[test]
    fn bcd_normalization() {
        assert_eq!(decode_bcd(0x24), 24);
        assert_eq!(decode_bcd(0x05), 5);
        assert_eq!(decode_bcd(0x99), 99);
    }

    #[test]
    fn manufacture_date_format() {
        assert_eq!(format_manufacture_date(0x24, 0x05).as_deref(), Some("2024-W05"));
        assert_eq!(format_manufacture_date(0x00, 0x00), None);
    }

    #[test]
    fn thermal_status_priority() {
        assert_eq!(thermal_status_name(0x00), "Good");
        assert_eq!(thermal_status_name(0x01), "AboveHighLimit");
        assert_eq!(thermal_status_name(0x02), "BelowLowLimit");
        assert_eq!(thermal_status_name(0x04), "AboveCriticalHighLimit");
        assert_eq!(thermal_status_name(0x08), "BelowCriticalLowLimit");
        // bit2 优先于 bit0/bit3 优先于 bit1
        assert_eq!(thermal_status_name(0x07), "AboveCriticalHighLimit");
        assert_eq!(thermal_status_name(0x0A), "BelowCriticalLowLimit");
    }

    #[test]
    fn ddr5_device_type_check() {
        assert!(is_ddr5(0x51, 0x18));
        assert!(!is_ddr5(0x51, 0x00));
        assert!(!is_ddr5(0x00, 0x18));
    }

    #[test]
    fn capability_bit1_means_thermal_sensor() {
        assert!(has_thermal_sensor(0x03));
        assert!(!has_thermal_sensor(0x01));
    }

    #[test]
    fn write_recovery_decoding() {
        // 本机实测 0x32：recUnit=3、timeUnit=2(ms) → 3 ms
        assert_eq!(write_recovery_ms(0x32), 3);
        // recUnit=0x0B → 50，timeUnit=2 → 50 ms
        assert_eq!(write_recovery_ms(0xB2), 50);
        // timeUnit=1(µs)：recUnit=0x0D → 200 µs → 向 1 ms 取整
        assert_eq!(write_recovery_ms(0xD1), 1);
        // timeUnit=0(ns)：recUnit=5 → 5 ns → 取整 1 ms
        assert_eq!(write_recovery_ms(0x50), 1);
    }

    #[test]
    fn jedec_lookup_and_fallback() {
        // 本机实测：延续码 0x80（清 bit7 = 0）→ bank 1 → Micron 0x2C
        assert_eq!(manufacturer_bank(0x80), 1);
        assert_eq!(manufacturer_bank(0x00), 1);
        assert_eq!(manufacturer_bank(0x01), 2);
        assert_eq!(manufacturer_bank(0x83), 4);
        assert_eq!(jedec_manufacturer(manufacturer_bank(0x80), 0x2C), Some("Micron Technology"));
        assert_eq!(jedec_manufacturer(1, 0x2C), Some("Micron Technology"));
        assert_eq!(jedec_manufacturer(1, 0xAD), Some("SK Hynix"));
        assert_eq!(jedec_manufacturer(1, 0xCE), Some("Samsung"));
        assert_eq!(jedec_manufacturer(1, 0x7F), None);
        assert_eq!(jedec_manufacturer(2, 0x2C), None);
    }

    #[test]
    fn addresses_match_ramspdtoolkit_constants() {
        // 页换算核对：型号字段跨 0x209..0x226 → 第 4 页、偏移 0x89..0xA6
        assert_eq!(SPD_MODULE_PART_NUMBER_BEGIN >> PAGE_SHIFT, 4);
        assert_eq!(
            ((SPD_MODULE_PART_NUMBER_BEGIN & 0x7F) as u8) | 0x80,
            0x89
        );
        assert_eq!(SPD_MODULE_PART_NUMBER_END >> PAGE_SHIFT, 4);
        assert_eq!(((SPD_MODULE_PART_NUMBER_END & 0x7F) as u8) | 0x80, 0xA6);
        // 序列号 0x205..0x208 → 第 4 页
        assert_eq!(SPD_MODULE_SERIAL_BEGIN >> PAGE_SHIFT, 4);
        assert_eq!(SPD_MODULE_SERIAL_END >> PAGE_SHIFT, 4);
    }
}
