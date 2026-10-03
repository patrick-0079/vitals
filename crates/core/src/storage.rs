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
//!    同一日志的 offset 200 起是 8 路温度传感器（Temperature Sensor 1..8，开尔文），
//!    对应 LHM 的 `Temperature #1..#8`
//! 3. NVMe Identify Controller（CNS=01h）→ WCTEMP/CCTEMP，即 LHM 的
//!    `Warning Temperature` / `Critical Temperature`；STORAGE_DEVICE_TEMPERATURE_PROPERTY
//!    的 `WarningTemperature`/`CriticalTemperature` 字段是同一信息的另一条通路，两者互证
//! 4. `\\.\PhysicalDriveN` + IOCTL_DISK_PERFORMANCE → DISK_PERFORMANCE 计数器，
//!    差分出 Read/Write/Total Activity（%）与读写吞吐（MiB/s），对应 LHM
//!    `StorageDevice.cs:396-447` 的 `_perfRead/_perfWrite/_perfTotal`。
//!    两个坑（本机实测）：① 该 IOCTL 的**码值**必须用 winioctl.h 现在写的
//!    `FILE_ANY_ACCESS` 编码 `0x00070020`，按旧文档的 `FILE_READ_ACCESS`
//!    （`0x00074020`）会被驱动当成不认识的请求，一律 `ERROR_INVALID_FUNCTION(1)`；
//!    ② 但**不需要管理员** —— 句柄用 `FILE_READ_ATTRIBUTES` 打开即可（与 SMART 同级）。
//!    另外 `QueryTime` 只在驱动刷新计数器时才走（空闲时 2 s 只走 ~50 ms），
//!    所以速率必须用**墙钟**差分而不是 ΔQueryTime（LHM 用的是后者，空闲会失真）。
//!    吞吐另有第二条通路：NVMe 设备计数器（`ThroughputState`），见文件下半部分。
//!
//! 纯解析函数与 IO 分离，便于单测（见文件末尾）。

#![cfg(windows)]

use std::ffi::c_void;
use std::time::Instant;

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
/// NVMe Health Log 里 8 路温度传感器的起始偏移（每路 u16 LE，开尔文）
const NVME_TEMP_SENSOR_OFFSET: usize = 200;
const NVME_TEMP_SENSOR_COUNT: usize = 8;

/// NVMe Identify Controller 数据结构长度（CNS=01h）
const NVME_ID_CTRL_SIZE: usize = 4096;
/// `NVME_DATA_TYPE_IDENTIFY`（winioctl.h 的 NVME_DATA_TYPES）
const NVME_DATA_TYPE_IDENTIFY: u32 = 1;
/// Identify Controller 里的警告复合温度 WCTEMP（开尔文，LE u16，byte 266）
const ID_CTRL_WCTEMP_OFF: usize = 266;
/// Identify Controller 里的临界复合温度 CCTEMP（开尔文，LE u16，byte 268）
const ID_CTRL_CCTEMP_OFF: usize = 268;

/// IOCTL_DISK_PERFORMANCE 的**两个可能码值**，按顺序都试一遍，取先成功的。
///
/// winioctl.h（本机 SDK 10.0.26100）现在写作
/// `CTL_CODE(IOCTL_DISK_BASE, 0x0008, METHOD_BUFFERED, FILE_ANY_ACCESS)`
/// = (0x07 << 16) | (0 << 14) | (0x08 << 2) | 0 = **0x00070020**；
/// 历史版本与多数文档写 FILE_READ_ACCESS（访问级别位 14..15 = 1）→ 0x00074020。
/// **实测（提权与非提权各跑一遍）只有 0x70020 被驱动接受**，0x74020 一律
/// `ERROR_INVALID_FUNCTION(1)`（STATUS_INVALID_DEVICE_REQUEST，驱动不认这个请求）——
/// 驱动是按编译时的宏做全值比较的。留两个码值是为了兼容老驱动。
const IOCTL_DISK_PERFORMANCE_CODES: [u32; 2] = [0x0007_0020, 0x0007_4020];
/// DISK_PERFORMANCE 长度（x64：5×8 + 4×4 + 8 + 4 + 16 = 88，末尾按 8 字节对齐）
const DISK_PERFORMANCE_SIZE: usize = 88;

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
    /// 8 路温度传感器里有值的那些（℃）。传感器 1 通常等于复合温度，驱动未填的为 0 开尔文，
    /// 这里直接跳过 —— 与 LHM 只在 `attr.RawValue > 0` 时才挂传感器一致。
    pub temp_sensors_c: Vec<f32>,
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
///  200   Temperature Sensor 1..8（每路 u16 LE 开尔文，0 = 未填）
pub fn parse_nvme_health(log: &[u8]) -> Option<NvmeHealth> {
    if log.len() < NVME_HEALTH_LOG_SIZE {
        return None;
    }
    let kelvin = u16::from_le_bytes([log[1], log[2]]);
    // 0 开尔文 = 驱动没填；开尔文 - 273.15 → ℃
    let temp_c = if kelvin == 0 { 0.0 } else { kelvin as f32 - 273.15 };
    let mut temp_sensors_c = Vec::new();
    for i in 0..NVME_TEMP_SENSOR_COUNT {
        let off = NVME_TEMP_SENSOR_OFFSET + i * 2;
        let Some(s) = log.get(off..off + 2) else { break };
        let k = u16::from_le_bytes(s.try_into().unwrap());
        if k > 0 {
            temp_sensors_c.push(k as f32 - 273.15);
        }
    }
    Some(NvmeHealth {
        critical_warning: log[0],
        temp_c,
        temp_sensors_c,
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

/// 输出缓冲里定位协议数据的起始偏移。
/// 输出 = STORAGE_PROTOCOL_DATA_DESCRIPTOR{Version,Size,ProtocolSpecificData}，
/// 数据紧随其后，偏移由返回的 ProtocolSpecificData.ProtocolDataOffset 给出（相对该结构起点）。
/// ProtocolSpecificData 位于缓冲区 offset 8。偏移异常时退回 8 + 40。
fn data_offset(out: &[u8], needed: usize) -> usize {
    let declared = read_u32(out, 8 + 24) as usize; // ProtocolDataOffset 是该结构第 6 个字段
    let off = 8 + declared;
    if declared >= PROTO_SPECIFIC_DATA_SIZE && off + needed <= out.len() {
        off
    } else if 8 + PROTO_SPECIFIC_DATA_SIZE + needed <= out.len() {
        8 + PROTO_SPECIFIC_DATA_SIZE
    } else {
        out.len()
    }
}

/// NVMe SMART 日志在输出缓冲里的偏移（见 `data_offset`）。
pub fn health_log_offset(out: &[u8]) -> usize {
    data_offset(out, NVME_HEALTH_LOG_SIZE)
}

/// `STORAGE_TEMPERATURE_DATA_DESCRIPTOR` 的解析结果。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TemperatureDescriptor {
    /// index 0 的复合温度（℃）；0 或超范围视为未提供
    pub composite_c: Option<f32>,
    /// 头部 `WarningTemperature`（℃，i16；0 视为未提供）
    pub warning_c: Option<i16>,
    /// 头部 `CriticalTemperature`（℃，i16；0 视为未提供）
    pub critical_c: Option<i16>,
    /// 各 `STORAGE_TEMPERATURE_INFO` 的温度（℃），按条目顺序，跳过无效值
    pub sensors: Vec<f32>,
}

/// 解析 STORAGE_TEMPERATURE_DATA_DESCRIPTOR（winioctl.h:2815）。
/// 布局：Version@0, Size@4, CriticalTemperature(i16)@8, WarningTemperature(i16)@10,
/// InfoCount(u16)@12, Reserved0[2]@14, Reserved1[8]@16 → 头部 24 字节；
/// 随后 InfoCount 个 STORAGE_TEMPERATURE_INFO（winioctl.h:2800）：
/// Index(u16)@0, Temperature(i16)@2, OverThreshold(i16)@4, UnderThreshold(i16)@6,
/// 两个 Changable@8/@9, EventGenerated@10, Reserved0@11, Reserved1(u32)@12 → 16 字节/个。
/// index 0 是复合温度；负数或超范围（>150℃）视为无效。
pub fn parse_temperature_descriptor(buf: &[u8]) -> TemperatureDescriptor {
    let i16_at = |off: usize| -> Option<i16> {
        buf.get(off..off + 2)
            .map(|s| i16::from_le_bytes(s.try_into().unwrap()))
    };
    let count = buf
        .get(12..14)
        .map(|s| u16::from_le_bytes(s.try_into().unwrap()) as usize)
        .unwrap_or(0);
    let valid = |raw: i16| raw > 0 && raw <= 150;
    let mut r = TemperatureDescriptor {
        critical_c: i16_at(8).filter(|&v| valid(v)),
        warning_c: i16_at(10).filter(|&v| valid(v)),
        ..Default::default()
    };
    for i in 0..count.min(8) {
        let off = TEMP_DESC_SIZE + i * TEMP_INFO_SIZE;
        let Some(raw) = i16_at(off + 2) else { break };
        if !valid(raw) {
            continue;
        }
        if i == 0 {
            r.composite_c = Some(raw as f32);
        } else {
            r.sensors.push(raw as f32);
        }
    }
    if r.composite_c.is_none() {
        // index 0 没填时退回第一个有效条目（有些驱动只填传感器 2）
        r.composite_c = r.sensors.first().copied();
    }
    r
}

/// 专用温度属性查询的输入缓冲（只需 STORAGE_PROPERTY_QUERY 头）。
fn temperature_input() -> Vec<u8> {
    let mut v = vec![0u8; 12];
    v[0..4].copy_from_slice(&STORAGE_DEVICE_TEMPERATURE_PROPERTY.to_le_bytes());
    v[4..8].copy_from_slice(&PROPERTY_STANDARD_QUERY.to_le_bytes());
    v
}

/// 查专用温度属性。比解 512 字节 NVMe 健康日志轻得多，且不要求 NVMe 协议查询能力。
fn query_temperature(handle: HANDLE) -> Result<TemperatureDescriptor, u32> {
    let (buf, _n) = query_property(
        handle,
        &temperature_input(),
        TEMP_DESC_SIZE + TEMP_INFO_SIZE * 8,
    )?;
    Ok(parse_temperature_descriptor(&buf))
}

/// NVMe Identify Controller 查询输入缓冲（CNS=01h）。
/// 与 SMART 日志查询的唯一区别是 DataType=1（Identify）而不是 2（Log Page），
/// 且 `ProtocolDataRequestValue` = 1（CNS 01h）。
fn nvme_identify_input() -> Vec<u8> {
    let mut v = nvme_health_input_variant(STORAGE_DEVICE_PROTOCOL_SPECIFIC_PROPERTY, 8);
    let p = 8;
    v[p + 4..p + 8].copy_from_slice(&NVME_DATA_TYPE_IDENTIFY.to_le_bytes());
    v[p + 8..p + 12].copy_from_slice(&1u32.to_le_bytes()); // CNS = 01h（Identify Controller）
    v[p + 20..p + 24].copy_from_slice(&(NVME_ID_CTRL_SIZE as u32).to_le_bytes());
    v
}

/// 从 Identify Controller 数据结构里取 (WCTEMP, CCTEMP)，开尔文 → ℃。
/// 0 表示控制器没有报这个字段（NVMe 允许），返回 None 而不是 0℃。
pub fn parse_identify_controller(buf: &[u8]) -> (Option<f32>, Option<f32>) {
    let k = |off: usize| -> Option<f32> {
        buf.get(off..off + 2)
            .map(|s| u16::from_le_bytes(s.try_into().unwrap()))
            .filter(|&v| v > 0)
            .map(|v| v as f32 - 273.15)
    };
    (k(ID_CTRL_WCTEMP_OFF), k(ID_CTRL_CCTEMP_OFF))
}

/// 查 Identify Controller，返回 (警告温度, 临界温度)。
fn query_identify_controller(handle: HANDLE) -> Result<(Option<f32>, Option<f32>), u32> {
    let (buf, _n) = query_property(
        handle,
        &nvme_identify_input(),
        8 + PROTO_SPECIFIC_DATA_SIZE + NVME_ID_CTRL_SIZE,
    )?;
    let off = data_offset(&buf, NVME_ID_CTRL_SIZE);
    Ok(buf
        .get(off..off + NVME_ID_CTRL_SIZE)
        .map(parse_identify_controller)
        .unwrap_or((None, None)))
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
            temp_c: t.composite_c,
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
    /// 专用温度属性（prop=52）读到的完整描述符
    pub temp_desc: Option<TemperatureDescriptor>,
    /// NVMe Identify Controller 的 WCTEMP / CCTEMP（℃）
    pub id_warn_c: Option<f32>,
    pub id_crit_c: Option<f32>,
    pub bytes_returned: u32,
    /// 磁盘性能计数器（IOCTL_DISK_PERFORMANCE；码值不对/计数器被系统关闭时见 perf_error）
    pub perf: Option<DiskPerformance>,
    /// 生效的 IOCTL 码值（0 表示全部候选码值都失败）
    pub perf_code: u32,
    pub perf_error: u32,
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
        temp_desc: None,
        id_warn_c: None,
        id_crit_c: None,
        bytes_returned: 0,
        perf: None,
        perf_code: 0,
        perf_error: 0,
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
    match query_disk_performance(h) {
        Ok((p, code)) => {
            r.perf = Some(p);
            r.perf_code = code;
        }
        Err(e) => r.perf_error = e,
    }
    if r.desc.bus_type == BUS_TYPE_NVME {
        // 温度优先走专用属性：轻量、且不依赖 NVMe 协议查询是否被驱动放行
        r.temp_desc = query_temperature(h).ok();
        if let Ok((w, c)) = query_identify_controller(h) {
            r.id_warn_c = w;
            r.id_crit_c = c;
        }
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

/// 把一轮性能采样的结果并进存储指标（按 PhysicalDrive 编号配对）。
/// 返回成功配上速率的盘数。
pub fn merge_perf(metrics: &mut [StorageMetrics], samples: &[PerfSample]) -> usize {
    let mut n = 0;
    for s in samples {
        let Some(r) = s.rates else { continue };
        if let Some(m) = metrics.iter_mut().find(|m| m.index == s.index) {
            m.activity_read_pct = Some(r.read_pct);
            m.activity_write_pct = Some(r.write_pct);
            m.activity_total_pct = Some(r.total_pct);
            m.read_mib_s = Some(r.read_mib_s);
            m.write_mib_s = Some(r.write_mib_s);
            n += 1;
        }
    }
    n
}

// ---------------------------------------------------------------------------
// 磁盘活动率与吞吐：IOCTL_DISK_PERFORMANCE（对应 LHM `StorageDevice.cs:396-447`）

/// DISK_PERFORMANCE 计数器（winioctl.h）。
/// 三个时间字段单位是 100ns；`query_time` 是驱动维护的时基，它自己也在涨。
/// 活动率 = Δ忙时 / Δ时基 —— 这就是 LHM `PerformanceValue.Update(val, valBase)` 的算法。
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct DiskPerformance {
    pub bytes_read: u64,
    pub bytes_written: u64,
    pub read_time: u64,
    pub write_time: u64,
    pub idle_time: u64,
    pub read_count: u32,
    pub write_count: u32,
    pub queue_depth: u32,
    pub split_count: u32,
    pub query_time: u64,
    pub storage_device_number: u32,
}

fn read_u64(b: &[u8], off: usize) -> u64 {
    b.get(off..off + 8)
        .map(|s| u64::from_le_bytes(s.try_into().unwrap()))
        .unwrap_or(0)
}

/// 布局：BytesRead@0, BytesWritten@8, ReadTime@16, WriteTime@24, IdleTime@32,
/// ReadCount@40, WriteCount@44, QueueDepth@48, SplitCount@52, QueryTime@56,
/// StorageDeviceNumber@64, StorageManagerName[8]@68（16 字节 WCHAR）→ 88 字节。
pub fn parse_disk_performance(buf: &[u8]) -> Option<DiskPerformance> {
    if buf.len() < DISK_PERFORMANCE_SIZE {
        return None;
    }
    Some(DiskPerformance {
        bytes_read: read_u64(buf, 0),
        bytes_written: read_u64(buf, 8),
        read_time: read_u64(buf, 16),
        write_time: read_u64(buf, 24),
        idle_time: read_u64(buf, 32),
        read_count: read_u32(buf, 40),
        write_count: read_u32(buf, 44),
        queue_depth: read_u32(buf, 48),
        split_count: read_u32(buf, 52),
        query_time: read_u64(buf, 56),
        storage_device_number: read_u32(buf, 64),
    })
}

/// LHM `PerformanceValue.Update` 的等价实现：`100 / Δbase × Δvalue`，结果钳到 [0,100]。
pub fn activity_pct(delta_value: u64, delta_base: u64) -> f32 {
    if delta_base == 0 {
        return 0.0;
    }
    ((100.0 * delta_value as f64 / delta_base as f64) as f32).clamp(0.0, 100.0)
}

/// 总活动率 = 100 − 空闲率（LHM：`100 - _perfTotal.Result`）。
pub fn total_activity_pct(idle_pct: f32) -> f32 {
    (100.0 - idle_pct).clamp(0.0, 100.0)
}

/// 字节差 ÷ 秒 → MiB/s。
pub fn rate_mib_s(delta_bytes: u64, dt_s: f64) -> f32 {
    if dt_s <= 0.0 {
        return 0.0;
    }
    (delta_bytes as f64 / dt_s / (1024.0 * 1024.0)) as f32
}

/// 两次计数器快照之间算出的速率。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PerfRates {
    pub read_pct: f32,
    pub write_pct: f32,
    pub total_pct: f32,
    pub read_mib_s: f32,
    pub write_mib_s: f32,
}

/// 由前后两个计数器快照算速率；`dt_s` 是两次采样的真实间隔（吞吐分母用它而不是
/// ΔQueryTime —— 后者是驱动侧时基，与墙钟可能有偏差）。
pub fn compute_rates(prev: &DiskPerformance, cur: &DiskPerformance, dt_s: f64) -> Option<PerfRates> {
    if dt_s <= 0.0 {
        return None;
    }
    let dq = cur.query_time.saturating_sub(prev.query_time);
    let read_pct = activity_pct(cur.read_time.saturating_sub(prev.read_time), dq);
    let write_pct = activity_pct(cur.write_time.saturating_sub(prev.write_time), dq);
    let idle_pct = activity_pct(cur.idle_time.saturating_sub(prev.idle_time), dq);
    Some(PerfRates {
        read_pct,
        write_pct,
        total_pct: total_activity_pct(idle_pct),
        read_mib_s: rate_mib_s(cur.bytes_read.saturating_sub(prev.bytes_read), dt_s),
        write_mib_s: rate_mib_s(cur.bytes_written.saturating_sub(prev.bytes_written), dt_s),
    })
}

/// 单盘一轮采样结果。`error` 非 0 表示这次没读到，两个典型值：
/// `1 = ERROR_INVALID_FUNCTION`（遗留计数器被系统关闭，`diskperf -Y` 可开）、
/// `5 = ERROR_ACCESS_DENIED`（非提权时 `\\.\PhysicalDriveN` 拿不到 GENERIC_READ）。
#[derive(Clone, Copy, Debug)]
pub struct PerfSample {
    pub index: u32,
    pub rates: Option<PerfRates>,
    /// 生效的 IOCTL 码值（失败时为 0）
    pub code: u32,
    pub error: u32,
}

/// 磁盘性能采样器：持有上一轮计数器，`sample()` 差分出速率。
/// 计数器是设备级累加值，句柄不必常驻（每次采样开-查-关即可）。
#[derive(Default)]
pub struct PerfState {
    entries: Vec<(u32, DiskPerformance, Instant)>,
}

impl PerfState {
    pub fn new() -> Self {
        Self::default()
    }

    /// 扫一遍所有 PhysicalDrive；首轮只建基线，`rates` 为 None。
    pub fn sample(&mut self) -> Vec<PerfSample> {
        let mut out = Vec::new();
        for i in 0..MAX_DRIVES {
            let (h, _err, _access) = open_drive(i);
            if h == INVALID_HANDLE_VALUE {
                continue; // 该编号没有盘
            }
            let cur = query_disk_performance(h);
            unsafe { CloseHandle(h) };
            let now = Instant::now();
            match cur {
                Err(e) => out.push(PerfSample {
                    index: i,
                    rates: None,
                    code: 0,
                    error: e,
                }),
                Ok((p, code)) => {
                    let rates = self
                        .entries
                        .iter()
                        .find(|(idx, _, _)| *idx == i)
                        .and_then(|(_, prev, at)| {
                            compute_rates(prev, &p, now.duration_since(*at).as_secs_f64())
                        });
                    match self.entries.iter_mut().find(|(idx, _, _)| *idx == i) {
                        Some(slot) => {
                            slot.1 = p;
                            slot.2 = now;
                        }
                        None => self.entries.push((i, p, now)),
                    }
                    out.push(PerfSample {
                        index: i,
                        rates,
                        code,
                        error: 0,
                    });
                }
            }
        }
        out
    }
}

/// 一次 IOCTL_DISK_PERFORMANCE（两个候选码值都试，取先成功的）。
///
/// 返回 `(计数器, 生效的码值)`；全部失败时返回**最后一个** Win32 错误码，
/// 该错误码能区分两种失败：`ERROR_INVALID_FUNCTION(1)` = 计数器被系统关闭
/// （`diskperf -Y` 可开），`ERROR_ACCESS_DENIED(5)` = 句柄/权限不够。
fn query_disk_performance(handle: HANDLE) -> Result<(DiskPerformance, u32), u32> {
    const ERROR_INVALID_DATA: u32 = 13;
    let mut last_err = 0u32;
    for &code in IOCTL_DISK_PERFORMANCE_CODES.iter() {
        let mut out = vec![0u8; DISK_PERFORMANCE_SIZE];
        let mut read: u32 = 0;
        unsafe {
            let ok = DeviceIoControl(
                handle,
                code,
                std::ptr::null(),
                0,
                out.as_mut_ptr().cast::<c_void>(),
                out.len() as u32,
                &mut read,
                std::ptr::null_mut(),
            );
            if ok == 0 {
                last_err = GetLastError();
                continue;
            }
        }
        last_err = ERROR_INVALID_DATA;
        if let Some(perf) = parse_disk_performance(&out) {
            return Ok((perf, code));
        }
    }
    Err(last_err)
}

// ---------------------------------------------------------------------------
// 第二条吞吐通路：NVMe 设备计数器（Health log 的 Data Units Read/Written）
//
// 与 IOCTL 通路互补：IOCTL 给的是「自上次采样以来」的精细速率（本项目每帧 0.5 s），
// 设备计数器只能给健康日志刷新间隔（本项目 5 s）上的平均，但有两个好处 ——
// ① 不依赖 IOCTL 是否被驱动接受（老驱动/非 Windows 方案不同）；
// ② 与 SMART 同一次查询取回，零额外 IOCTL。
// 单位：1 data unit = 512000 字节（NVMe 规范，见 NVME_DATA_UNIT_BYTES）。

/// 由设备计数器差分算吞吐：1 data unit = 512000 字节（NVMe 规范）。
pub fn units_rate_mib_s(delta_units: u64, dt_s: f64) -> f32 {
    if dt_s <= 0.0 {
        return 0.0;
    }
    (delta_units as f64 * NVME_DATA_UNIT_BYTES / dt_s / (1024.0 * 1024.0)) as f32
}

/// NVMe 设备计数器推出的吞吐采样器。
#[derive(Default)]
pub struct ThroughputState {
    entries: Vec<(u32, u64, u64, Instant)>,
}

impl ThroughputState {
    pub fn new() -> Self {
        Self::default()
    }

    /// 用 `metrics` 里的原始计数器推进一次；首轮只建基线。
    /// 只填**还是 None** 的 `read_mib_s`/`write_mib_s` —— IOCTL 通路（粒度 0.5 s）优先。
    /// 返回配上速率的盘数。
    pub fn apply(&mut self, metrics: &mut [StorageMetrics]) -> usize {
        let now = Instant::now();
        let mut paired = 0;
        for m in metrics.iter_mut() {
            let cur = (m.data_units_read, m.data_units_written);
            match self.entries.iter_mut().find(|e| e.0 == m.index) {
                Some(e) => {
                    let dt = now.duration_since(e.3).as_secs_f64();
                    if dt > 0.0 {
                        let r = units_rate_mib_s(cur.0.saturating_sub(e.1), dt);
                        let w = units_rate_mib_s(cur.1.saturating_sub(e.2), dt);
                        if m.read_mib_s.is_none() {
                            m.read_mib_s = Some(r);
                        }
                        if m.write_mib_s.is_none() {
                            m.write_mib_s = Some(w);
                        }
                        paired += 1;
                    }
                    *e = (m.index, cur.0, cur.1, now);
                }
                None => self.entries.push((m.index, cur.0, cur.1, now)),
            }
        }
        // 掉线的盘不再占位，避免编号复用后拿旧计数器差分
        self.entries
            .retain(|e| metrics.iter().any(|m| m.index == e.0));
        paired
    }
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
            None => (
                p.temp_desc.as_ref().and_then(|d| d.composite_c),
                None,
                None,
                None,
                None,
                None,
            ),
        };
        let temp_sensors_c = p
            .health
            .as_ref()
            .map(|h| h.temp_sensors_c.clone())
            .unwrap_or_default();
        // 警告/临界温度：先看 STORAGE_TEMPERATURE_DATA_DESCRIPTOR 的头部字段，
        // 再看 NVMe Identify Controller 的 WCTEMP/CCTEMP（同一信息的独立通路）
        let warning_temp_c = p
            .temp_desc
            .as_ref()
            .and_then(|d| d.warning_c)
            .map(f32::from)
            .or(p.id_warn_c);
        let critical_temp_c = p
            .temp_desc
            .as_ref()
            .and_then(|d| d.critical_c)
            .map(f32::from)
            .or(p.id_crit_c);
        let source = if p.health.is_some() {
            "nvme-smart"
        } else if p.temp_desc.is_some() {
            "nvme-temp-prop"
        } else {
            "none"
        };
        // 原始设备计数器（512000 字节/单位）：吞吐兜底路径要靠它做差分
        let (units_read, units_written) = match p.health.as_ref() {
            Some(h) => (
                h.data_units_read.min(u64::MAX as u128) as u64,
                h.data_units_written.min(u64::MAX as u128) as u64,
            ),
            None => (0, 0),
        };
        v.push(StorageMetrics {
            index: p.index,
            name: if name.is_empty() { format!("PhysicalDrive{}", p.index) } else { name },
            bus,
            temp_c: temp,
            temp_sensors_c,
            warning_temp_c,
            critical_temp_c,
            percentage_used_pct: used,
            available_spare_pct: spare,
            power_on_hours: hours,
            data_written_gb: written,
            data_read_gb: read,
            data_units_read: units_read,
            data_units_written: units_written,
            activity_read_pct: None,
            activity_write_pct: None,
            activity_total_pct: None,
            read_mib_s: None,
            write_mib_s: None,
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
        let d = parse_temperature_descriptor(&buf);
        assert_eq!(d.composite_c, Some(41.0));
        assert_eq!(d.sensors, vec![39.0]);
        assert_eq!(d.warning_c, Some(80));
        assert_eq!(d.critical_c, Some(85));
    }

    #[test]
    fn temperature_descriptor_rejects_invalid() {
        // InfoCount = 0 → 无温度
        let mut buf = vec![0u8; TEMP_DESC_SIZE + TEMP_INFO_SIZE];
        buf[12..14].copy_from_slice(&1u16.to_le_bytes());
        // 条目温度为负（未提供）
        buf[TEMP_DESC_SIZE + 2..TEMP_DESC_SIZE + 4].copy_from_slice(&(-1i16).to_le_bytes());
        assert_eq!(parse_temperature_descriptor(&buf), TemperatureDescriptor::default());
        assert_eq!(parse_temperature_descriptor(&[0u8; 8]), TemperatureDescriptor::default());
    }

    #[test]
    fn nvme_temp_sensors_skip_unfilled() {
        let mut log = vec![0u8; NVME_HEALTH_LOG_SIZE];
        log[1..3].copy_from_slice(&320u16.to_le_bytes()); // 复合 46.85℃
        let s = NVME_TEMP_SENSOR_OFFSET;
        log[s..s + 2].copy_from_slice(&320u16.to_le_bytes()); // 46.85
        log[s + 2..s + 4].copy_from_slice(&312u16.to_le_bytes()); // 38.85
        // s+4 留 0 = 驱动未填，必须跳过
        log[s + 10..s + 12].copy_from_slice(&300u16.to_le_bytes()); // 26.85
        let h = parse_nvme_health(&log).unwrap();
        assert_eq!(h.temp_sensors_c.len(), 3);
        assert!((h.temp_sensors_c[0] - 46.85).abs() < 0.01);
        assert!((h.temp_sensors_c[1] - 38.85).abs() < 0.01);
        assert!((h.temp_sensors_c[2] - 26.85).abs() < 0.01);
    }

    #[test]
    fn identify_controller_thresholds() {
        let mut buf = vec![0u8; NVME_ID_CTRL_SIZE];
        buf[ID_CTRL_WCTEMP_OFF..ID_CTRL_WCTEMP_OFF + 2].copy_from_slice(&358u16.to_le_bytes());
        buf[ID_CTRL_CCTEMP_OFF..ID_CTRL_CCTEMP_OFF + 2].copy_from_slice(&363u16.to_le_bytes());
        let (w, c) = parse_identify_controller(&buf);
        assert!((w.unwrap() - 84.85).abs() < 0.01, "w={:?}", w);
        assert!((c.unwrap() - 89.85).abs() < 0.01, "c={:?}", c);
    }

    #[test]
    fn identify_controller_missing_thresholds() {
        // 0 开尔文 = 控制器没报这个字段 → None，而不是 0℃
        let (w, c) = parse_identify_controller(&[0u8; NVME_ID_CTRL_SIZE]);
        assert_eq!(w, None);
        assert_eq!(c, None);
    }

    #[test]
    fn disk_performance_layout() {
        let mut b = vec![0u8; DISK_PERFORMANCE_SIZE];
        b[0..8].copy_from_slice(&1_000_000u64.to_le_bytes());
        b[8..16].copy_from_slice(&2_000_000u64.to_le_bytes());
        b[16..24].copy_from_slice(&100u64.to_le_bytes());
        b[24..32].copy_from_slice(&200u64.to_le_bytes());
        b[32..40].copy_from_slice(&700u64.to_le_bytes());
        b[40..44].copy_from_slice(&11u32.to_le_bytes());
        b[44..48].copy_from_slice(&22u32.to_le_bytes());
        b[48..52].copy_from_slice(&33u32.to_le_bytes());
        b[52..56].copy_from_slice(&44u32.to_le_bytes());
        b[56..64].copy_from_slice(&1000u64.to_le_bytes());
        b[64..68].copy_from_slice(&7u32.to_le_bytes());
        let p = parse_disk_performance(&b).unwrap();
        assert_eq!(p.bytes_read, 1_000_000);
        assert_eq!(p.bytes_written, 2_000_000);
        assert_eq!(p.read_time, 100);
        assert_eq!(p.write_time, 200);
        assert_eq!(p.idle_time, 700);
        assert_eq!(p.read_count, 11);
        assert_eq!(p.write_count, 22);
        assert_eq!(p.queue_depth, 33);
        assert_eq!(p.split_count, 44);
        assert_eq!(p.query_time, 1000);
        assert_eq!(p.storage_device_number, 7);
        assert!(parse_disk_performance(&[0u8; 64]).is_none());
    }

    #[test]
    fn activity_and_rate_math() {
        // 忙 100 / 时基 1000 → 10%
        assert!((activity_pct(100, 1000) - 10.0).abs() < 1e-4);
        // 驱动时基落后于忙时（重复计数器等）→ 钳到 100，不出现 >100%
        assert_eq!(activity_pct(500, 100), 100.0);
        assert_eq!(activity_pct(0, 0), 0.0);
        assert_eq!(total_activity_pct(75.0), 25.0);
        assert!((rate_mib_s(1024 * 1024, 1.0) - 1.0).abs() < 1e-4);
        assert_eq!(rate_mib_s(1024, 0.0), 0.0);
    }

    #[test]
    fn compute_rates_from_counters() {
        let prev = DiskPerformance::default();
        let cur = DiskPerformance {
            bytes_read: 10 * 1024 * 1024,
            bytes_written: 5 * 1024 * 1024,
            read_time: 200,
            write_time: 100,
            idle_time: 700,
            query_time: 1000,
            ..Default::default()
        };
        let r = compute_rates(&prev, &cur, 1.0).unwrap();
        assert!((r.read_pct - 20.0).abs() < 1e-4);
        assert!((r.write_pct - 10.0).abs() < 1e-4);
        assert!((r.total_pct - 30.0).abs() < 1e-4);
        assert!((r.read_mib_s - 10.0).abs() < 1e-4);
        assert!((r.write_mib_s - 5.0).abs() < 1e-4);
        assert!(compute_rates(&prev, &cur, 0.0).is_none());
    }

    #[test]
    fn merge_perf_pairs_by_index() {
        let mut m = vec![
            StorageMetrics { index: 3, ..Default::default() },
            StorageMetrics { index: 9, ..Default::default() },
        ];
        let s = vec![PerfSample {
            index: 3,
            rates: Some(PerfRates {
                read_pct: 1.0,
                write_pct: 2.0,
                total_pct: 3.0,
                read_mib_s: 4.0,
                write_mib_s: 5.0,
            }),
            code: IOCTL_DISK_PERFORMANCE_CODES[0],
            error: 0,
        }];
        assert_eq!(merge_perf(&mut m, &s), 1);
        assert_eq!(m[0].activity_read_pct, Some(1.0));
        assert_eq!(m[0].activity_total_pct, Some(3.0));
        assert_eq!(m[1].activity_read_pct, None);
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

    #[test]
    fn units_throughput_math() {
        // 1024 单元 = 1024 × 512000 B = 500 MiB（二进制）→ 0.5 s 内 = 1000 MiB/s
        assert!((units_rate_mib_s(1024, 0.5) - 1000.0).abs() < 0.01);
        assert_eq!(units_rate_mib_s(1024, 0.0), 0.0);
    }

    #[test]
    fn throughput_state_baselines_then_fills_only_missing() {
        fn metrics(units_r: u64, units_w: u64) -> StorageMetrics {
            StorageMetrics {
                index: 0,
                data_units_read: units_r,
                data_units_written: units_w,
                ..Default::default()
            }
        }
        let mut st = ThroughputState::new();
        let mut first = vec![metrics(1000, 2000)];
        assert_eq!(st.apply(&mut first), 0, "首轮只建基线");
        assert!(first[0].read_mib_s.is_none());

        std::thread::sleep(std::time::Duration::from_millis(20));
        // 另一个盘（编号 1）也已经过一轮：它的 read_mib_s 被预置成 IOCTL 通路的读数，
        // 兜底路径不允许覆盖它
        let mut second = vec![metrics(2024, 3024)];
        second[0].write_mib_s = Some(7.5);
        let n = st.apply(&mut second);
        assert_eq!(n, 1);
        assert!(second[0].read_mib_s.unwrap() > 0.0, "Δ1024 单元应算出正吞吐");
        assert_eq!(second[0].write_mib_s, Some(7.5), "已有值不被兜底覆盖");
    }

    #[test]
    fn throughput_state_drops_missing_drives() {
        let mut st = ThroughputState::new();
        let mut one = vec![StorageMetrics {
            index: 0,
            data_units_read: 1,
            ..Default::default()
        }];
        st.apply(&mut one);
        assert_eq!(st.entries.len(), 1);
        // 盘掉了：状态里不该留它的旧计数器
        let mut none: Vec<StorageMetrics> = Vec::new();
        st.apply(&mut none);
        assert!(st.entries.is_empty());
    }
}