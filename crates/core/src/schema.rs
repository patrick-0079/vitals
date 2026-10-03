//! 数据契约：核心输出 JSON 的结构定义。
//! 前端/任何宿主只依赖这份契约，展示层随时可换。

use serde::Serialize;

#[derive(Serialize, Clone, Debug, Default)]
pub struct Metrics {
    /// 毫秒级 Unix 时间戳
    pub ts_ms: u64,
    pub cpu: CpuMetrics,
    pub memory: MemoryMetrics,
    pub gpu: Option<GpuMetrics>,
    /// 存储设备（NVMe SMART 等），可能为空
    pub storage: Vec<StorageMetrics>,
    /// 主板 SuperIO 传感器（风扇/板温/电压）；拿不到为 null
    pub superio: Option<SuperIoMetrics>,
    /// 内存模组（DDR5 SPD 直读的温度/型号），可能为空
    pub dimms: Vec<DimmMetrics>,
}

#[derive(Serialize, Clone, Debug, Default)]
pub struct CpuMetrics {
    pub name: String,
    /// 总占用率 %
    pub usage_pct: f32,
    /// 每个逻辑核心占用率 %
    pub per_core_pct: Vec<f32>,
    /// 当前最大核心频率（MHz）
    pub freq_mhz: u64,
    /// Tctl/Tdie 温度，拿不到为 null
    pub temp_c: Option<f32>,
    /// 温度来源："pawnio-smn" | "wmi-thermal" | "hwmon" | "none" | "pending"
    pub temp_source: String,
    /// 每 CCD 温度（仅 AMD PawnIO 通路）
    pub ccd_temps_c: Vec<f32>,
    /// CPU 整包功耗（RAPL）
    pub package_power_w: Option<f32>,
    /// Core 电压（SMU PM 表 VDDCR，Zen4/5 唯一可得来源）
    pub core_voltage_v: Option<f32>,
    /// SoC 电压（SMU PM 表 VDDCR SoC）
    pub soc_voltage_v: Option<f32>,
    /// AMD SMU PM 表解出的传感器全集（电压/电流/分项功耗/额外温度/频率）
    pub smu: Vec<SmuSensor>,
    /// 每核时钟/功耗（MSR，需要管理员权限；非 AMD 或非提权时为空）
    pub per_core: Vec<CpuCoreMetrics>,
}

/// 单个物理核的 MSR 读数（对照 LHM `Amd17Cpu.Core` 的 Clock / Effective Clock / Power 传感器）
#[derive(Serialize, Clone, Debug, Default)]
pub struct CpuCoreMetrics {
    /// 物理核序号（0 起；SMT 兄弟共用一个号）
    pub index: u32,
    /// 该核的逻辑处理器编号（读 MSR 时钉在这个号上）
    pub thread: u32,
    /// 瞬时 P-state 频率（MHz，来自 HW_PSTATE_STATUS 的 CpuFid）
    pub clock_mhz: f32,
    /// 平均有效频率（MHz，来自 APERF 增量 ÷ 采样窗口；含被 halt 掉的时间）
    pub effective_mhz: f32,
    /// 该核功耗（W，来自 CORE_ENERGY_STAT 差分）
    pub power_w: Option<f32>,
    /// 该核 APERF/MPERF 比值（<1 表示低于参考频率；无基线时为 null）
    pub aperf_mperf_ratio: Option<f32>,
}

/// AMD SMU PM 表里的一个传感器（对照 LHM Hardware/RyzenSMU.cs 的传感器表）
#[derive(Serialize, Clone, Debug, Default)]
pub struct SmuSensor {
    pub name: String,
    /// "voltage" | "current" | "power" | "temperature" | "clock"
    pub kind: String,
    pub value: f32,
    /// "V" | "A" | "W" | "°C" | "MHz"
    pub unit: String,
}

#[derive(Serialize, Clone, Debug, Default)]
pub struct MemoryMetrics {
    pub usage_pct: f32,
    pub used_gb: f32,
    pub total_gb: f32,
}

#[derive(Serialize, Clone, Debug, Default)]
pub struct GpuMetrics {
    pub name: String,
    pub usage_pct: f32,
    pub temp_c: Option<f32>,
    /// GPU 热点温度。NVML 给不出来，走 NVAPI（RTX 50 系没有独立热点通道 → 本机恒为 null）
    pub hotspot_c: Option<f32>,
    /// 显存结温（memory junction）。同样来自 NVAPI；RTX 50 系是 Temperatures[2]/256
    pub mem_junction_c: Option<f32>,
    /// 离最近一个降频阈值还差多少度（NVML nvmlDeviceGetMarginTemperature；本机 = 降频阈值 − 当前温度）
    pub temp_margin_c: Option<f32>,
    /// 降频阈值 / 关机阈值 / die 最热点阈值（NVML 的温度阈值，静态值）
    pub temp_slowdown_c: Option<f32>,
    pub temp_shutdown_c: Option<f32>,
    pub temp_max_c: Option<f32>,
    /// 显存：used 不含驱动保留（与 nvidia-smi 的 Used/Free/Reserved 同语义）
    pub vram_used_mb: f32,
    pub vram_total_mb: f32,
    pub vram_free_mb: Option<f32>,
    pub vram_reserved_mb: Option<f32>,
    pub vram_usage_pct: f32,
    /// 当前/最大 核心时钟（GRAPHICS 域）与显存时钟（MEM 域），MHz
    pub core_clock_mhz: Option<f32>,
    pub mem_clock_mhz: Option<f32>,
    pub max_core_clock_mhz: Option<f32>,
    pub max_mem_clock_mhz: Option<f32>,
    pub power_w: Option<f32>,
    /// 功耗上限（优先 enforced limit）与实际占用比例
    pub power_limit_w: Option<f32>,
    pub power_limit_pct: Option<f32>,
    pub fan_pct: Option<f32>,
    /// 多风扇取最大转速；fan_count 是风扇数量
    pub fan_rpm: Option<f32>,
    pub fan_count: Option<u32>,
    pub encoder_pct: Option<f32>,
    pub decoder_pct: Option<f32>,
    /// PCIe 吞吐（NVML 报 KB/s，这里换成 MiB/s；采样窗口由驱动维护）
    pub pcie_tx_mib_s: Option<f32>,
    pub pcie_rx_mib_s: Option<f32>,
    /// 形如 "PCIe Gen5 x16"
    pub pcie_link: Option<String>,
    /// 时钟受限原因：`null` = 该 API 不可用/读取失败，`[]` = 读了且**没有**受限原因，
    /// 非空 = 受限原因列表（位定义见 nvml.h 的 nvmlClocksEventReason*）。
    pub throttle_reasons: Option<Vec<String>>,
    /// "nvml" | "nvidia-smi" | "none"
    pub source: String,
}

/// 单块存储设备（对照 LHM Hardware/Storage 的传感器集合）
#[derive(Serialize, Clone, Debug, Default)]
pub struct StorageMetrics {
    /// PhysicalDrive 编号
    pub index: u32,
    pub name: String,
    /// "nvme" | "other"
    pub bus: String,
    /// 温度 ℃（NVMe SMART/Health log 的复合温度）
    pub temp_c: Option<f32>,
    /// 额外温度传感器 ℃（NVMe 日志的 Temperature Sensor 1..8 里有值的那些，
    /// 对照 LHM 的 `Temperature #1..#8`；序号 1 通常等于复合温度）
    #[serde(default)]
    pub temp_sensors_c: Vec<f32>,
    /// 警告复合温度 ℃（LHM `Warning Temperature`，来源 NVMe Identify Controller 的 WCTEMP
    /// 或 STORAGE_TEMPERATURE_DATA_DESCRIPTOR.WarningTemperature）
    pub warning_temp_c: Option<f32>,
    /// 临界复合温度 ℃（LHM `Critical Temperature`，来源 CCTEMP / CriticalTemperature）
    pub critical_temp_c: Option<f32>,
    /// 寿命消耗百分比
    pub percentage_used_pct: Option<f32>,
    /// 可用备用块百分比
    pub available_spare_pct: Option<f32>,
    pub power_on_hours: Option<f64>,
    /// 累计写入 GB（十进制，1 GB = 1e9 B）
    pub data_written_gb: Option<f64>,
    pub data_read_gb: Option<f64>,
    /// NVMe 原始设备计数器（1 单位 = 512000 B）。吞吐的第二条通路（5 s 区间平均），
    /// 与 IOCTL 通路（每帧精细速率）互补
    #[serde(default)]
    pub data_units_read: u64,
    #[serde(default)]
    pub data_units_written: u64,
    /// 读活动率 %（IOCTL_DISK_PERFORMANCE，非提权即可读；读不到时为 null）
    pub activity_read_pct: Option<f32>,
    /// 写活动率 %
    pub activity_write_pct: Option<f32>,
    /// 总活动率 % = 100 − 空闲率
    pub activity_total_pct: Option<f32>,
    /// 读吞吐 MiB/s（IOCTL 通路失败时退到设备计数器差分，此时是 5 s 区间平均）
    pub read_mib_s: Option<f32>,
    /// 写吞吐 MiB/s
    pub write_mib_s: Option<f32>,
    /// "nvme-smart" | "nvme-temp-prop" | "none"
    pub source: String,
}

/// 主板 SuperIO 的一个传感器（命名来自主板档案，与 LHM 的命名一致）
#[derive(Serialize, Clone, Debug)]
pub struct BoardSensor {
    pub name: String,
    /// "fan" | "temperature" | "voltage"
    pub kind: String,
    pub value: f32,
    /// "RPM" | "°C" | "V"
    pub unit: String,
}

/// 主板 SuperIO（LPC 硬件监控芯片）读数
#[derive(Serialize, Clone, Debug)]
pub struct SuperIoMetrics {
    /// 芯片名，如 "Nuvoton NCT6701D"
    pub chip: String,
    /// 命名档案 id，如 "asus-am5-b850m"
    pub profile: String,
    pub sensors: Vec<BoardSensor>,
}

/// 单条内存模组（DDR5 SPD，经 SMBus 直读；对照 RAMSPDToolkit 的 SPD 访问器）
#[derive(Serialize, Clone, Debug, Default)]
pub struct DimmMetrics {
    /// SPD 从机地址 - 0x50（0..7）
    pub index: u8,
    /// SMBus 从机地址（0x50..=0x57）
    pub address: u8,
    /// 模组型号（SPD 第 4 页 0x209..0x226，去掉 0x20 填充）
    pub part_number: String,
    /// 模组序列号（SPD 0x205..0x208 的大写十六进制串）
    pub serial_number: String,
    /// JEP106 厂商名；不在精简表里时为 `0x{bank:02X}/0x{id:02X}`
    pub manufacturer: String,
    /// 制造日期 `2024-W05`；未编码为 null
    pub manufacture_date: Option<String>,
    /// 模组温度 ℃（MR 空间 0x31）
    pub temp_c: Option<f32>,
    /// "Good" | "AboveHighLimit" | "BelowLowLimit" | "AboveCriticalHighLimit" | "BelowCriticalLowLimit" | "Unknown"
    pub thermal_status: String,
    /// "ddr5-spd" | "none"
    pub source: String,
}

/// 静态硬件信息 + 数据源可用性（诊断用）
#[derive(Serialize, Clone, Debug)]
pub struct Info {
    pub version: &'static str,
    pub cpu_name: String,
    pub logical_cores: usize,
    pub cpu_family: u32,
    pub cpu_model: u32,
    pub total_memory_gb: f32,
    pub gpu_name: Option<String>,
    pub platform: &'static str,
    /// SMU PM 表版本（如 0x00540004 = Zen4 布局），拿不到为 null
    pub smu_pm_table_version: Option<u32>,
    /// 主板上的 SuperIO 芯片名（探测到但未实现解码时也会填）
    pub superio_chip: Option<String>,
    pub sources: SourcesStatus,
}

#[derive(Serialize, Clone, Debug, Default)]
pub struct SourcesStatus {
    pub pawnio: bool,
    /// true = PawnIO 设备打开被拒（非提权运行的典型情况）——
    /// 提示宿主“以管理员运行可解锁 CPU 温度/功耗”
    pub pawnio_access_denied: bool,
    pub nvml: bool,
    pub nvidia_smi: bool,
    pub wmi: bool,
    /// 至少有一块盘读到了 NVMe SMART 健康日志
    pub storage_smart: bool,
    /// 磁盘活动率可用（IOCTL_DISK_PERFORMANCE 成功过一次）。
    /// **不需要管理员**（`FILE_READ_ATTRIBUTES` 句柄即可）；为 false 只说明驱动
    /// 不认这个 IOCTL（例如只认老码值的老驱动）。吞吐不受此标志影响 ——
    /// IOCTL 不通时退到 NVMe 设备计数器差分（5 s 区间平均）。
    pub storage_perf: bool,
    /// SMU PM 表可用（AMD Zen 的电压/电流来源）
    pub smu: bool,
    /// 主板 SuperIO 可用且有已实现解码的芯片（风扇/板温/电压）
    pub superio: bool,
    /// 至少识别到一条内存模组的 SPD（DDR5 温度来源）
    pub spd: bool,
    /// 每核 MSR（时钟/功耗）可用 —— 需要 administrator + AMD Zen
    pub msr_cores: bool,
}
