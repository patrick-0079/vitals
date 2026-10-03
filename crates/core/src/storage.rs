//! 存储设备 SMART —— 对照 LibreHardwareMonitor 的 `Hardware/Storage` 模块。
//!
//! 注意：LHM 自己**不实现** SMART 解析，它把读取委托给外部库 DiskInfoToolkit
//! （`StorageDevice.cs:7 using StorageDeviceDIT = DiskInfoToolkit.Devices.StorageDevice;`），
//! 只负责把读到的值挂成传感器。所以这里按其等价机制用 Win32 IOCTL 自行实现：
//!
//! 1. `\\.\PhysicalDriveN` + IOCTL_STORAGE_QUERY_PROPERTY(StorageDeviceProperty)
//!    → STORAGE_DEVICE_DESCRIPTOR，拿到产品名与 BusType（NVMe = 17）
//! 2. NVMe 盘再查 IOCTL_STORAGE_QUERY_PROPERTY(StorageDeviceProtocolSpecificProperty)
//!    + ProtocolTypeNvme/NVMeDataTypeLogPage，请求 NVMe SMART/Health Information Log
//!    （log page 0x02）→ 温度（开尔文）、可用备用块、寿命消耗、读写量、通电小时数
//!
//! 纯解析函数与 IO 分离，便于单测（见文件末尾）。

#![cfg(windows)]

use std::ffi::c_void;

use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{CreateFileW, OPEN_EXISTING};
use windows_sys::Win32::System::IO::DeviceIoControl;

use crate::schema::StorageMetrics;

// ---- Win32 常量（不依赖 windows-sys 的具体导出位置，直接写字面量）----
const GENERIC_READ: u32 = 0x8000_0000;
/// FILE_READ_ATTRIBUTES：非提权时 `\\.\PhysicalDriveN` 只肯给这个级别的访问，
/// 但 IOCTL_STORAGE_QUERY_PROPERTY 是 FILE_ANY_ACCESS，够用。
const FILE_READ_ATTRIBUTES: u32 = 0x0080;
const FILE_SHARE_READ_WRITE: u32 = 0x1 | 0x2;

/// CTL_CODE(IOCTL_STORAGE_BASE(0x2D), 0x0500, METHOD_BUFFERED, FILE_ANY_ACCESS)
const IOCTL_STORAGE_QUERY_PROPERTY: u32 = 0x002D_1400;

const STORAGE_DEVICE_PROPERTY: u32 = 0;
/// 取自 SDK `winioctl.h` 的 `STORAGE_PROPERTY_ID` 枚举：注意该枚举在
/// `StorageAdapterCryptoProperty(17)` 之后**跳到 48**（StorageDeviceIoCapabilityProperty = 48），
/// 因此协议专属属性是 **49/50**，不是 18/19 —— 传 18/19 会得到
/// `ERROR_INVALID_FUNCTION(1)`（属性本身不存在），与访问级别无关。
const STORAGE_ADAPTER_PROTOCOL_SPECIFIC_PROPERTY: u32 = 49;
const STORAGE_DEVICE_PROTOCOL_SPECIFIC_PROPERTY: u32 = 50;
/// 专用温度查询（NVMe 走这条最省事，不必解 512 字节健康日志）
const STORAGE_DEVICE_TEMPERATURE_PROPERTY: u32 = 52;
const PROPERTY_STANDARD_QUERY: u32 = 0;

const PROTOCOL_TYPE_NVME: u32 = 3;
const NVME_DATA_TYPE_LOG_PAGE: u32 = 2;
const NVME_LOG_PAGE_HEALTH_INFO: u32 = 0x02;

/// STORAGE_DEVICE_DESCRIPTOR.BusType 中的 NVMe
pub const BUS_TYPE_NVME: u32 = 17;

/// STORAGE_PROTOCOL_SPECIFIC_DATA 大小（10 个 ULONG）
const PROTO_SPECIFIC_DATA_SIZE: usize = 40;
/// `ProtocolDataOffset` 的语义（winioctl.h:2690）："The offset of data buffer is
/// from beginning of this data structure"，即相对 STORAGE_PROTOCOL_SPECIFIC_DATA 自身起点，
/// 所以查询请求里它等于本结构大小 40。
const PROTOCOL_DATA_OFFSET: u32 = PROTO_SPECIFIC_DATA_SIZE as u32;
/// STORAGE_TEMPERATURE_DATA_DESCRIPTOR 头部长度（见下）
const TEMP_DESC_SIZE: usize = 24;
/// 每个 STORAGE_TEMPERATURE_INFO 的长度
const TEMP_INFO_SIZE: usize = 16;
/// NVMe SMART/Health Information Log 长度
const NVME_HEALTH_LOG_SIZE: usize = 512;

/// 待扫描的 PhysicalDrive 编号上限（驱动器号稀疏，扫一遍很快）
const MAX_DRIVES: u32 = 32;

// ---------------------------------------------------------------------------
// 纯解析：STORAGE_DEVICE_DESCRIPTOR

#[derive(Clone, Debug, Default, PartialEq)]
pub struct DeviceDesc {
    pub bus_type: u32,
    pub vendor: String,
    pub product: String,
    pub serial: String,
}

fn read_u32(b: &[u8], off: usize) -> u32 {
    b.get(off..off + 4)
        .map(|s| u32::from_le_bytes(s.try_into().unwrap()))
        .unwrap_or(0)
}

/// 从缓冲区 offset 处读 NUL 结尾的 ASCII 字符串（偏移为 0 表示无此字段）。
fn read_ascii_z(b: &[u8], off: u32) -> String {
    let off = off as usize;
    if off == 0 || off >= b.len() {
        return String::new();
    }
    let end = b[off..].iter().position(|&c| c == 0).map(|p| off + p).unwrap_or(b.len());
    String::from_utf8_lossy(&b[off..end]).trim().to_string()
}

/// 解析 STORAGE_DEVICE_DESCRIPTOR。
/// 布局：Version@0, Size@4, DeviceType@8..11, VendorIdOffset@12, ProductIdOffset@16,
/// ProductRevisionOffset@20, SerialNumberOffset@24, BusType@28, RawPropertiesLength@32。
pub fn parse_device_descriptor(buf: &[u8]) -> DeviceDesc {
    DeviceDesc {
        bus_type: read_u32(buf, 28),
        vendor: read_ascii_z(buf, read_u32(buf, 12)),
        product: read_ascii_z(buf, read_u32(buf, 16)),
        serial: read_ascii_z(buf, read_u32(buf, 24)),
    }
}

// ---------------------------------------------------------------------------
// 纯解析：NVMe SMART/Health Information Log（log page 0x02）

#[derive(Clone, Debug, Default, PartialEq)]
pub struct NvmeHealth {
    pub critical_warning: u8,
    /// 复合温度 ℃（日志里是开尔文）
    pub temp_c: f32,
    pub available_spare_pct: f32,
    pub spare_threshold_pct: f32,
    pub percentage_used_pct: f32,
    pub data_units_read: u128,
    pub data_units_written: u128,
    pub power_cycles: u128,
    pub power_on_hours: u128,
    pub unsafe_shutdowns: u128,
    pub media_errors: u128,
}

fn read_u128(b: &[u8], off: usize) -> u128 {
    match b.get(off..off + 16) {
        Some(s) => u128::from_le_bytes(s.try_into().unwrap()),
        None => 0,
    }
}

/// NVMe 数据单元 = 1000 × 512 字节。
pub const NVME_DATA_UNIT_BYTES: f64 = 1000.0 * 512.0;

/// 解析 512 字节的 NVMe SMART/Health Information Log。
/// 布局（NVMe 规范）：
///  0     Critical Warning
///  1..2  Composite Temperature（开尔文，LE u16）
///  3     Available Spare %
///  4     Available Spare Threshold %
///  5     Percentage Used %
///  32    Data Units Read（128 位）
///  48    Data Units Written
///  64/80 Host Read/Write Commands，96 Controller Busy Time
///  112   Power Cycles，128 Power On Hours，144 Unsafe Shutdowns
///  160   Media and Data Integrity Errors，176 Error Information Log Entries
pub fn parse_nvme_health(log: &[u8]) -> Option<NvmeHealth> {
    if log.len() < NVME_HEALTH_LOG_SIZE {
        return None;
    }
    let kelvin = u16::from_le_bytes([log[1], log[2]]);
    // 0 开尔文 = 驱动没填；开尔文 - 273.15 → ℃
    let temp_c = if kelvin == 0 { 0.0 } else { kelvin as f32 - 273.15 };
    Some(NvmeHealth {
        critical_warning: log[0],
        temp_c,
        available_spare_pct: log[3] as f32,
        spare_threshold_pct: log[4] as f32,
        percentage_used_pct: log[5] as f32,
        data_units_read: read_u128(log, 32),
        data_units_written: read_u128(log, 48),
        power_cycles: read_u128(log, 112),
        power_on_hours: read_u128(log, 128),
        unsafe_shutdowns: read_u128(log, 144),
        media_errors: read_u128(log, 160),
    })
}

/// 输出缓冲里定位 NVMe 日志的起始偏移。
/// 输出 = STORAGE_PROTOCOL_DATA_DESCRIPTOR{Version,Size,ProtocolSpecificData}，
/// 数据紧随其后，偏移由返回的 ProtocolSpecificData.ProtocolDataOffset 给出（相对该结构起点）。
/// ProtocolSpecificData 位于缓冲区 offset 8。偏移异常时退回 8 + 40。
pub fn health_log_offset(out: &[u8]) -> usize {
    let declared = read_u32(out, 8 + 24) as usize; // ProtocolDataOffset 是该结构第 6 个字段
    let off = 8 + declared;
    if declared >= PROTO_SPECIFIC_DATA_SIZE && off + NVME_HEALTH_LOG_SIZE <= out.len() {
        off
    } else if 8 + PROTO_SPECIFIC_DATA_SIZE + NVME_HEALTH_LOG_SIZE <= out.len() {
        8 + PROTO_SPECIFIC_DATA_SIZE
    } else {
        out.len()
    }
}

/// 解析 STORAGE_TEMPERATURE_DATA_DESCRIPTOR（winioctl.h:2815）。
/// 布局：Version@0, Size@4, CriticalTemperature(i16)@8, WarningTemperature(i16)@10,
/// InfoCount(u16)@12, Reserved0[2]@14, Reserved1[8]@16 → 头部 24 字节；
/// 随后 InfoCount 个 STORAGE_TEMPERATURE_INFO（winioctl.h:2800）：
/// Index(u16)@0, Temperature(i16)@2, OverThreshold(i16)@4, UnderThreshold(i16)@6,
/// 两个 Changable@8/@9, EventGenerated@10, Reserved0@11, Reserved1(u32)@12 → 16 字节/个。
/// index 0 是复合温度，优先返回；负数或超范围视为无效。
pub fn parse_temperature_descriptor(buf: &[u8]) -> Option<f32> {
    let count = buf
        .get(12..14)
        .map(|s| u16::from_le_bytes(s.try_into().unwrap()) as usize)
        .unwrap_or(0);
    let mut fallback: Option<f32> = None;
    for i in 0..count.min(8) {
        let off = TEMP_DESC_SIZE + i * TEMP_INFO_SIZE;
        let Some(raw) = buf
            .get(off + 2..off + 4)
            .map(|s| i16::from_le_bytes(s.try_into().unwrap()))
        else {
            break;
        };
        if raw <= 0 || raw > 150 {
            continue;
        }
        if i == 0 {
            return Some(raw as f32);
        }
        fallback.get_or_insert(raw as f32);
    }
    fallback
}

/// 专用温度属性查询的输入缓冲（只需 STORAGE_PROPERTY_QUERY 头）。
fn temperature_input() -> Vec<u8> {
    let mut v = vec![0u8; 12];
    v[0..4].copy_from_slice(&STORAGE_DEVICE_TEMPERATURE_PROPERTY.to_le_bytes());
    v[4..8].copy_from_slice(&PROPERTY_STANDARD_QUERY.to_le_bytes());
    v
}

/// 查专用温度属性。比解 512 字节 NVMe 健康日志轻得多，且不要求 NVMe 协议查询能力。
fn query_temperature(handle: HANDLE) -> Result<Option<f32>, u32> {
    let (buf, _n) = query_property(
        handle,
        &temperature_input(),
        TEMP_DESC_SIZE + TEMP_INFO_SIZE * 8,
    )?;
    Ok(parse_temperature_descriptor(&buf))
}

// ---------------------------------------------------------------------------
// IO

/// 打开 `\\.\PhysicalDriveN`，返回 (句柄, GetLastError 快照)。
/// 先要 GENERIC_READ；被拒（非提权常见）再退回 FILE_READ_ATTRIBUTES —— 属性查询
/// 级访问就足以执行 IOCTL_STORAGE_QUERY_PROPERTY（该 IOCTL 是 FILE_ANY_ACCESS）。
fn open_drive(index: u32) -> (HANDLE, u32, u32) {
    let path: Vec<u16> = format!("\\\\.\\PhysicalDrive{}", index)
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let mut last_err = 0;
    for access in [GENERIC_READ, FILE_READ_ATTRIBUTES] {
        unsafe {
            let h = CreateFileW(
                path.as_ptr(),
                access,
                FILE_SHARE_READ_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            );
            if h != INVALID_HANDLE_VALUE {
                return (h, 0, access);
            }
            last_err = GetLastError();
        }
    }
    (INVALID_HANDLE_VALUE, last_err, 0)
}

/// 一次 IOCTL_STORAGE_QUERY_PROPERTY。成功返回 (输出缓冲, 实际字节数)。
fn query_property(handle: HANDLE, input: &[u8], out_len: usize) -> Result<(Vec<u8>, u32), u32> {
    let mut out = vec![0u8; out_len];
    let mut read: u32 = 0;
    unsafe {
        let ok = DeviceIoControl(
            handle,
            IOCTL_STORAGE_QUERY_PROPERTY,
            input.as_ptr().cast::<c_void>(),
            input.len() as u32,
            out.as_mut_ptr().cast::<c_void>(),
            out.len() as u32,
            &mut read,
            std::ptr::null_mut(),
        );
        if ok == 0 {
            return Err(GetLastError());
        }
    }
    Ok((out, read))
}

/// 通用属性查询（StorageDeviceProperty）输入缓冲。
fn device_property_input() -> Vec<u8> {
    let mut v = vec![0u8; 8 + PROTO_SPECIFIC_DATA_SIZE];
    v[0..4].copy_from_slice(&STORAGE_DEVICE_PROPERTY.to_le_bytes());
    v[4..8].copy_from_slice(&PROPERTY_STANDARD_QUERY.to_le_bytes());
    v.truncate(12); // STORAGE_PROPERTY_QUERY 实际大小（含 AdditionalParameters 填充）
    v
}

/// NVMe 协议专属查询输入缓冲。
/// `property_id`：18 = StorageAdapterProtocolSpecificProperty，
///                19 = StorageDeviceProtocolSpecificProperty；
/// `proto_off`：STORAGE_PROTOCOL_SPECIFIC_DATA 在输入缓冲中的偏移 —— 厂商实现
///              对它的位置有歧义（编译器把 STORAGE_PROPERTY_QUERY 视为 12 字节
///              含填充，而文档说数据紧跟 AdditionalParameters），故两种都试。
fn nvme_health_input_variant(property_id: u32, proto_off: usize) -> Vec<u8> {
    let mut v = vec![0u8; proto_off + PROTO_SPECIFIC_DATA_SIZE];
    v[0..4].copy_from_slice(&property_id.to_le_bytes());
    v[4..8].copy_from_slice(&PROPERTY_STANDARD_QUERY.to_le_bytes());
    let p = proto_off;
    v[p..p + 4].copy_from_slice(&PROTOCOL_TYPE_NVME.to_le_bytes());
    v[p + 4..p + 8].copy_from_slice(&NVME_DATA_TYPE_LOG_PAGE.to_le_bytes());
    v[p + 8..p + 12].copy_from_slice(&NVME_LOG_PAGE_HEALTH_INFO.to_le_bytes());
    v[p + 12..p + 16].copy_from_slice(&0u32.to_le_bytes()); // ProtocolDataRequestSubValue
    v[p + 16..p + 20].copy_from_slice(&PROTOCOL_DATA_OFFSET.to_le_bytes()); // ProtocolDataOffset
    v[p + 20..p + 24].copy_from_slice(&(NVME_HEALTH_LOG_SIZE as u32).to_le_bytes()); // ProtocolDataLength
    v
}

/// 默认组合（实测有效的会记录在此）：prop=50 即 StorageDeviceProtocolSpecificProperty，
/// 协议结构放在 AdditionalParameters 起点（缓冲 offset 8）。
fn nvme_health_input() -> Vec<u8> {
    nvme_health_input_variant(STORAGE_DEVICE_PROTOCOL_SPECIFIC_PROPERTY, 8)
}

/// 一次变体尝试的结果（诊断用）。
#[derive(Clone, Debug)]
pub struct NvmeAttempt {
    pub label: String,
    pub error: u32,
    pub bytes_returned: u32,
    pub temp_c: Option<f32>,
}

/// 对同一块盘穷举 (属性 ID × 协议数据偏移) 组合，找出这台机器上真正可用的那个。
/// Windows 存储栈对这两个参数的解释随版本/驱动而异，实测比猜可靠。
pub fn diagnose_nvme(index: u32) -> Vec<NvmeAttempt> {
    let mut out = Vec::new();
    let (h, err, _) = open_drive(index);
    if h == INVALID_HANDLE_VALUE {
        out.push(NvmeAttempt {
            label: format!("open 失败 err={}", err),
            error: err,
            bytes_returned: 0,
            temp_c: None,
        });
        return out;
    }
    const COMBOS: [(u32, usize); 4] = [
        (STORAGE_DEVICE_PROTOCOL_SPECIFIC_PROPERTY, 8),
        (STORAGE_DEVICE_PROTOCOL_SPECIFIC_PROPERTY, 12),
        (STORAGE_ADAPTER_PROTOCOL_SPECIFIC_PROPERTY, 8),
        (STORAGE_ADAPTER_PROTOCOL_SPECIFIC_PROPERTY, 12),
    ];
    match query_temperature(h) {
        Ok(t) => out.push(NvmeAttempt {
            label: format!("prop={} 温度属性", STORAGE_DEVICE_TEMPERATURE_PROPERTY),
            error: 0,
            bytes_returned: 0,
            temp_c: t,
        }),
        Err(e) => out.push(NvmeAttempt {
            label: format!("prop={} 温度属性", STORAGE_DEVICE_TEMPERATURE_PROPERTY),
            error: e,
            bytes_returned: 0,
            temp_c: None,
        }),
    }
    for (pid, off) in COMBOS {
        let label = format!("prop={} proto_off={}", pid, off);
        let inbuf = nvme_health_input_variant(pid, off);
        match query_property(h, &inbuf, 8 + PROTO_SPECIFIC_DATA_SIZE + NVME_HEALTH_LOG_SIZE) {
            Ok((buf, n)) => {
                let lo = health_log_offset(&buf);
                let health = buf.get(lo..lo + NVME_HEALTH_LOG_SIZE).and_then(parse_nvme_health);
                out.push(NvmeAttempt {
                    label,
                    error: 0,
                    bytes_returned: n,
                    temp_c: health.map(|h| h.temp_c),
                });
            }
            Err(e) => out.push(NvmeAttempt {
                label,
                error: e,
                bytes_returned: 0,
                temp_c: None,
            }),
        }
    }
    unsafe { CloseHandle(h) };
    out
}

/// 单盘探测结果（诊断用，storage_probe 示例打印）。
#[derive(Clone, Debug)]
pub struct DriveProbe {
    pub index: u32,
    pub open_error: u32,
    /// 实际拿到的访问权限（0x80000000 = GENERIC_READ，0x80 = FILE_READ_ATTRIBUTES）
    pub access: u32,
    pub desc: DeviceDesc,
    pub health: Option<NvmeHealth>,
    pub health_error: u32,
    /// 专用温度属性（prop=52）读到的复合温度
    pub temp_prop: Option<f32>,
    pub bytes_returned: u32,
}

/// 采集一块盘的静态描述（成功则返回描述，失败返回 open_error）。
pub fn probe_drive(index: u32) -> DriveProbe {
    let mut r = DriveProbe {
        index,
        open_error: 0,
        access: 0,
        desc: DeviceDesc::default(),
        health: None,
        health_error: 0,
        temp_prop: None,
        bytes_returned: 0,
    };
    let (h, err, access) = open_drive(index);
    if h == INVALID_HANDLE_VALUE {
        r.open_error = err;
        return r;
    }
    r.access = access;
    if let Ok((out, n)) = query_property(h, &device_property_input(), 1024) {
        let end = (n as usize).min(out.len());
        r.desc = parse_device_descriptor(&out[..end]);
    }
    if r.desc.bus_type == BUS_TYPE_NVME {
        // 温度优先走专用属性：轻量、且不依赖 NVMe 协议查询是否被驱动放行
        r.temp_prop = query_temperature(h).ok().flatten();
        match query_property(h, &nvme_health_input(), 8 + PROTO_SPECIFIC_DATA_SIZE + NVME_HEALTH_LOG_SIZE) {
            Ok((out, n)) => {
                r.bytes_returned = n;
                let off = health_log_offset(&out);
                r.health = out.get(off..off + NVME_HEALTH_LOG_SIZE).and_then(parse_nvme_health);
            }
            Err(e) => r.health_error = e,
        }
    }
    unsafe { CloseHandle(h) };
    r
}

/// 扫描所有 PhysicalDrive，返回可直接进 Metrics 的存储指标。
pub fn poll() -> Vec<StorageMetrics> {
    let mut v = Vec::new();
    for i in 0..MAX_DRIVES {
        let p = probe_drive(i);
        if p.open_error != 0 {
            continue;
        }
        let name = [p.desc.vendor.trim(), p.desc.product.trim()]
            .iter()
            .filter(|s| !s.is_empty())
            .cloned()
            .collect::<Vec<_>>()
            .join(" ");
        let bus = match p.desc.bus_type {
            BUS_TYPE_NVME => "nvme",
            _ => "other",
        }
        .to_string();
        // 温度：健康日志优先，退到专用温度属性；磨损/写入量只有健康日志有
        let (temp, used, spare, hours, written, read) = match p.health.as_ref() {
            Some(h) => (
                Some(h.temp_c),
                Some(h.percentage_used_pct),
                Some(h.available_spare_pct),
                Some(h.power_on_hours as f64),
                Some((h.data_units_written as f64 * NVME_DATA_UNIT_BYTES) / 1e9),
                Some((h.data_units_read as f64 * NVME_DATA_UNIT_BYTES) / 1e9),
            ),
            None => (p.temp_prop, None, None, None, None, None),
        };
        let source = if p.health.is_some() {
            "nvme-smart"
        } else if p.temp_prop.is_some() {
            "nvme-temp-prop"
        } else {
            "none"
        };
        v.push(StorageMetrics {
            index: p.index,
            name: if name.is_empty() { format!("PhysicalDrive{}", p.index) } else { name },
            bus,
            temp_c: temp,
            percentage_used_pct: used,
            available_spare_pct: spare,
            power_on_hours: hours,
            data_written_gb: written,
            data_read_gb: read,
            source: source.into(),
        });
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_parse() {
        // 构造一个最小 STORAGE_DEVICE_DESCRIPTOR：BusType=NVMe，产品名 "Samsung SSD 990 EVO 2TB"
        let mut b = vec![0u8; 128];
        b[0..4].copy_from_slice(&0x0000_0001u32.to_le_bytes()); // Version
        b[4..8].copy_from_slice(&128u32.to_le_bytes()); // Size
        b[28..32].copy_from_slice(&BUS_TYPE_NVME.to_le_bytes()); // BusType
        // ProductIdOffset = 64
        b[16..20].copy_from_slice(&64u32.to_le_bytes());
        let prod = b"Samsung SSD 990 EVO 2TB";
        b[64..64 + prod.len()].copy_from_slice(prod);
        let d = parse_device_descriptor(&b);
        assert_eq!(d.bus_type, BUS_TYPE_NVME);
        assert_eq!(d.product, "Samsung SSD 990 EVO 2TB");
        assert_eq!(d.vendor, "");
    }

    #[test]
    fn nvme_health_parse() {
        let mut log = vec![0u8; NVME_HEALTH_LOG_SIZE];
        log[0] = 0; // critical warning
        let kelvin: u16 = (47.0f32 + 273.15) as u16; // 320
        log[1..3].copy_from_slice(&kelvin.to_le_bytes());
        log[3] = 100; // available spare
        log[4] = 10; // threshold
        log[5] = 3; // percentage used
        log[32..48].copy_from_slice(&1_000_000u128.to_le_bytes()); // data units read
        log[48..64].copy_from_slice(&2_000_000u128.to_le_bytes()); // data units written
        log[112..128].copy_from_slice(&42u128.to_le_bytes()); // power cycles
        log[128..144].copy_from_slice(&1234u128.to_le_bytes()); // power on hours
        log[144..160].copy_from_slice(&7u128.to_le_bytes()); // unsafe shutdowns
        log[160..176].copy_from_slice(&0u128.to_le_bytes()); // media errors

        let h = parse_nvme_health(&log).unwrap();
        assert!((h.temp_c - 46.85).abs() < 0.1, "temp={}", h.temp_c);
        assert_eq!(h.available_spare_pct, 100.0);
        assert_eq!(h.spare_threshold_pct, 10.0);
        assert_eq!(h.percentage_used_pct, 3.0);
        assert_eq!(h.power_cycles, 42);
        assert_eq!(h.power_on_hours, 1234);
        assert_eq!(h.unsafe_shutdowns, 7);
        assert_eq!(h.data_units_written, 2_000_000);
    }

    #[test]
    fn temperature_descriptor_parse() {
        // 头部 24 字节 + 2 个 16 字节条目
        let mut buf = vec![0u8; TEMP_DESC_SIZE + TEMP_INFO_SIZE * 2];
        buf[4..8].copy_from_slice(&((TEMP_DESC_SIZE + TEMP_INFO_SIZE * 2) as u32).to_le_bytes());
        buf[8..10].copy_from_slice(&85i16.to_le_bytes()); // CriticalTemperature
        buf[10..12].copy_from_slice(&80i16.to_le_bytes()); // WarningTemperature
        buf[12..14].copy_from_slice(&2u16.to_le_bytes()); // InfoCount
        // 条目 0：复合温度 41℃
        buf[TEMP_DESC_SIZE + 2..TEMP_DESC_SIZE + 4].copy_from_slice(&41i16.to_le_bytes());
        // 条目 1：传感器 2，温度 39℃
        let e1 = TEMP_DESC_SIZE + TEMP_INFO_SIZE;
        buf[e1..e1 + 2].copy_from_slice(&1u16.to_le_bytes());
        buf[e1 + 2..e1 + 4].copy_from_slice(&39i16.to_le_bytes());
        assert_eq!(parse_temperature_descriptor(&buf), Some(41.0));
    }

    #[test]
    fn temperature_descriptor_rejects_invalid() {
        // InfoCount = 0 → 无温度
        let mut buf = vec![0u8; TEMP_DESC_SIZE + TEMP_INFO_SIZE];
        buf[12..14].copy_from_slice(&1u16.to_le_bytes());
        // 条目温度为负（未提供）
        buf[TEMP_DESC_SIZE + 2..TEMP_DESC_SIZE + 4].copy_from_slice(&(-1i16).to_le_bytes());
        assert_eq!(parse_temperature_descriptor(&buf), None);
        assert_eq!(parse_temperature_descriptor(&[0u8; 8]), None);
    }

    #[test]
    fn nvme_health_rejects_short_buffer() {
        assert!(parse_nvme_health(&[0u8; 64]).is_none());
    }

    #[test]
    fn health_offset_prefers_declared() {
        // 缓冲区：8 字节描述符头 + 40 字节协议数据 + 512 日志
        let out = vec![0u8; 8 + PROTO_SPECIFIC_DATA_SIZE + NVME_HEALTH_LOG_SIZE];
        assert_eq!(health_log_offset(&out), 8 + PROTO_SPECIFIC_DATA_SIZE);
        // 声明了更大的偏移且放得下 → 采用声明值
        let mut out2 = vec![0u8; 8 + 64 + NVME_HEALTH_LOG_SIZE];
        out2[8 + 24..8 + 28].copy_from_slice(&64u32.to_le_bytes());
        assert_eq!(health_log_offset(&out2), 8 + 64);
    }

    #[test]
    fn data_unit_math() {
        // 2e6 数据单元 × 512000 B = 1.024e12 B = 1024 GB
        let gb = 2_000_000f64 * NVME_DATA_UNIT_BYTES / 1e9;
        assert!((gb - 1024.0).abs() < 0.01);
    }
}