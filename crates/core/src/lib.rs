//! computer-status 核心采集引擎。
//!
//! 架构：本 crate 编译为 C ABI 动态库（cs_core.dll / libcs_core.so），
//! 展示层（web 壳 / GUI / CLI）只是它的消费者，随时可换。
//!
//! C ABI：
//! ```c
//! int   cs_init();                    // 初始化（幂等），0 = 成功
//! const char* cs_version();           // 版本号，静态字符串，无需释放
//! char* cs_get_info_json();           // 硬件静态信息 JSON，需 cs_free_string 释放
//! char* cs_get_metrics_json();        // 一次指标快照 JSON，需 cs_free_string 释放
//! void  cs_free_string(char* p);      // 释放上面两个函数返回的字符串
//! void  cs_shutdown();                // 停止后台采样线程
//! ```
//!
//! 温度多源降级链（Windows AMD）：
//! 自研 PawnIO+SMN（正规签名驱动，HVCI 兼容）→ WMI ACPI 热区 → N/A。
//! GPU 走 NVML（进程内）→ nvidia-smi 子进程。

pub mod amd_temp;
pub mod gpu;
pub mod schema;
#[cfg(windows)]
pub mod cpu_topology;
#[cfg(windows)]
pub mod nvapi;
#[cfg(windows)]
pub mod pawnio;
#[cfg(windows)]
pub mod smbus;
#[cfg(windows)]
pub mod spd;
#[cfg(windows)]
pub mod smu;
#[cfg(windows)]
pub mod storage;
#[cfg(windows)]
pub mod superio;
#[cfg(windows)]
pub mod wmi_temp;

/// 非 Windows 占位：存储 SMART 尚未实现
#[cfg(not(windows))]
pub mod storage {
    use crate::schema::StorageMetrics;

    pub fn poll() -> Vec<StorageMetrics> {
        Vec::new()
    }

    /// 占位：非 Windows 没有 IOCTL_DISK_PERFORMANCE
    #[derive(Default)]
    pub struct PerfState;

    #[derive(Clone, Copy)]
    pub struct PerfSample {
        pub index: u32,
        pub code: u32,
        pub error: u32,
    }

    impl PerfState {
        pub fn new() -> Self {
            Self
        }

        pub fn sample(&mut self) -> Vec<PerfSample> {
            Vec::new()
        }
    }

    pub fn merge_perf(_metrics: &mut [StorageMetrics], _samples: &[PerfSample]) -> usize {
        0
    }

    /// 占位：非 Windows 没有 NVMe 设备计数器
    #[derive(Default)]
    pub struct ThroughputState;

    impl ThroughputState {
        pub fn new() -> Self {
            Self
        }

        pub fn apply(&mut self, _metrics: &mut [StorageMetrics]) -> usize {
            0
        }
    }
}

/// 非 Windows 占位：SMU 只存在于 AMD 平台
#[cfg(not(windows))]
pub mod smu {
    use crate::schema::SmuSensor;
    pub fn find(sensors: &[SmuSensor], name: &str, kind: &str) -> Option<f32> {
        sensors
            .iter()
            .find(|s| s.name == name && s.kind == kind)
            .map(|s| s.value)
    }
}

/// 非 Windows 占位：SuperIO（LPC 硬件监控芯片）只存在于 Windows 侧的 x86 主板
#[cfg(not(windows))]
pub mod superio {
    pub fn access_denied() -> bool {
        false
    }
}

/// 非 Windows 占位：NVAPI 只在 NVIDIA 驱动带 nvapi64.dll 的 Windows 上可用
#[cfg(not(windows))]
pub mod nvapi {
    /// 占位类型：与 Windows 侧同名，供 `gpu::NvmlGpu` 持有 `Option<NvapiGpu>`
    pub struct NvapiGpu;

    impl NvapiGpu {
        pub fn open() -> Option<NvapiGpu> {
            None
        }

        pub fn read_thermal(&self) -> (Option<f32>, Option<f32>) {
            (None, None)
        }
    }
}

/// 非 Windows 占位：SPD 直读依赖 PawnIO + PIIX4 SMBus（Windows only）
#[cfg(not(windows))]
pub mod spd {
    use crate::schema::DimmMetrics;
    pub fn poll() -> Vec<DimmMetrics> {
        Vec::new()
    }
}

use std::ffi::c_char;
use std::ffi::CString;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use schema::{
    CpuCoreMetrics, CpuMetrics, DimmMetrics, Info, MemoryMetrics, Metrics, SmuSensor,
    SourcesStatus, StorageMetrics, SuperIoMetrics,
};

/// LHM 官方签发的 PawnIO 模块（MPL-2.0，来源见 drivers/pawnio/）。
#[cfg(windows)]
const AMDF17_BIN: &[u8] = include_bytes!("../../../drivers/pawnio/AMDFamily17.bin");

const TICK: Duration = Duration::from_millis(500);
const GB: f64 = 1024.0 * 1024.0 * 1024.0;

// ---------------------------------------------------------------------------
// Rust 侧 API（CLI 和测试直接用；C ABI 是它的薄封装）

pub struct Monitor {
    shared: Arc<Shared>,
}

struct Shared {
    stop: AtomicBool,
    snapshot: Mutex<Option<Metrics>>,
    info: Info,
}

impl Monitor {
    /// 启动监控：探测硬件与数据源，起后台采样线程。
    /// 返回即可用；首次 `metrics()` 最多等待 ~2s 出首帧（正常 ~0.9s）。
    pub fn start() -> Monitor {
        let mut sources = SourcesStatus::default();

        // CPU 身份（PawnIO AMD 通路的前置判断）
        #[cfg(target_arch = "x86_64")]
        let id = amd_temp::cpuid_family_model();
        #[cfg(not(target_arch = "x86_64"))]
        let id = amd_temp::CpuIdentity { family: 0, model: 0 };

        // PawnIO + AMD Family17 模块（仅 Windows + AMD Zen）
        #[cfg(windows)]
        let pawnio = {
            let p = if amd_temp::is_zen(id) {
                pawnio::PawnIo::open(AMDF17_BIN)
            } else {
                None
            };
            sources.pawnio = p.is_some();
            sources.pawnio_access_denied = pawnio::access_denied();
            p
        };
        #[cfg(not(windows))]
        let pawnio: Option<()> = None;

        // SMU PM 表（Zen4/5 的 Core/SoC 电压）：与 PawnIO 同源，同样需要提权
        #[cfg(windows)]
        let smu_client = {
            let c = if amd_temp::is_zen(id) {
                smu::SmuClient::open()
            } else {
                None
            };
            sources.smu = c.as_ref().map(|c| c.layout().is_some()).unwrap_or(false);
            c
        };
        #[cfg(not(windows))]
        let smu_client: Option<()> = None;
        #[cfg(windows)]
        let smu_pm_table_version = smu_client.as_ref().map(|c| c.pm_table_version);
        #[cfg(not(windows))]
        let smu_pm_table_version = None;

        // 主板 SuperIO（风扇/板温/电压）：LPC 端口同样要管理员权限
        #[cfg(windows)]
        let superio_client = {
            let s = superio::SuperIo::open();
            sources.superio = s.as_ref().map(|s| s.is_supported()).unwrap_or(false);
            s
        };
        #[cfg(not(windows))]
        let superio_client: Option<()> = None;
        #[cfg(windows)]
        let superio_chip = superio_client
            .as_ref()
            .map(|s| s.chip_name().to_string());
        #[cfg(not(windows))]
        let superio_chip: Option<String> = None;

        // 内存模组 SPD（DDR5 温度/型号）：走 PIIX4 SMBus，与 CPU 温度同属提权能力。
        // 总线句柄常驻（与 LHM 的 RAMSPDToolkitDriver 同粒度），避免每帧重载模块。
        #[cfg(windows)]
        let spd_bus = smbus::Piix4::open(Some(0));
        #[cfg(not(windows))]
        let spd_bus: Option<()> = None;
        #[cfg(windows)]
        let dimms_seed = spd_bus
            .as_ref()
            .map(spd::enumerate)
            .unwrap_or_default();
        #[cfg(not(windows))]
        let dimms_seed: Vec<DimmMetrics> = Vec::new();
        sources.spd = dimms_seed.iter().any(|d| d.source == "ddr5-spd");

        // 每核 MSR（时钟/功耗）：拓扑一次拿定，基线在采样线程里维护
        #[cfg(windows)]
        let core_groups = if amd_temp::is_zen(id) {
            cpu_topology::core_groups()
        } else {
            Vec::new()
        };
        #[cfg(not(windows))]
        let core_groups: Vec<Vec<u32>> = Vec::new();
        sources.msr_cores = pawnio.is_some() && !core_groups.is_empty();

        // NVML
        let nvml = gpu::NvmlGpu::open();
        sources.nvml = nvml.is_some();
        let gpu_name = nvml.as_ref().map(|g| g.name().to_string());

        // sysinfo：CPU 列表 + 内存
        let mut sys = sysinfo::System::new_all();
        sys.refresh_cpu_usage(); // 首刷，预热
        let cpu_name = sys
            .cpus()
            .first()
            .map(|c| c.brand().trim().to_string())
            .unwrap_or_default();
        let logical_cores = sys.cpus().len();
        let total_memory_gb = (sys.total_memory() as f64 / GB) as f32;

        // 兜底数据源预探测（决定 Info 里的 flags）
        let smi_ok = nvml.is_none() && gpu::nvidia_smi_poll().is_some();
        sources.nvidia_smi = smi_ok;
        #[cfg(windows)]
        let wmi_ok = pawnio.is_none() && wmi_temp::thermal_zone_max_temp_c().is_some();
        #[cfg(not(windows))]
        let wmi_ok = false;
        sources.wmi = wmi_ok;

        // 存储（NVMe SMART）：探测一次，既做数据源标记也给采样线程做种子
        let mut storage_seed = storage::poll();
        sources.storage_smart = storage_seed.iter().any(|s| s.source == "nvme-smart");
        // 磁盘性能计数器（活动率/吞吐）：首轮只建基线，顺带判定权限是否够
        let mut storage_perf = storage::PerfState::new();
        sources.storage_perf = storage_perf.sample().iter().any(|s| s.error == 0);
        // 设备计数器差分（吞吐兜底）：同样首轮只建基线
        let mut storage_units = storage::ThroughputState::new();
        storage_units.apply(&mut storage_seed);

        let info = Info {
            version: env!("CARGO_PKG_VERSION"),
            cpu_name: cpu_name.clone(),
            logical_cores,
            cpu_family: id.family,
            cpu_model: id.model,
            total_memory_gb,
            gpu_name,
            platform: std::env::consts::OS,
            smu_pm_table_version,
            superio_chip,
            sources: sources.clone(),
        };

        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            snapshot: Mutex::new(None),
            info,
        });

        let sampler_shared = Arc::clone(&shared);
        let sampler = Sampler {
            sys,
            pawnio,
            nvml,
            id,
            cpu_name,
            energy_unit_j: None,
            last_energy: None,
            wmi_cache: None,
            storage: storage_seed,
            storage_perf,
            storage_units,
            smu: smu_client,
            smu_sensors: Vec::new(),
            superio: superio_client,
            superio_metrics: None,
            spd_bus,
            dimms: dimms_seed,
            core_groups,
            core_baselines: Vec::new(),
            core_metrics: Vec::new(),
            tick: 0,
        };
        std::thread::Builder::new()
            .name("cs-sampler".into())
            .spawn(move || sampler_loop(sampler, sampler_shared))
            .expect("spawn sampler thread");

        Monitor { shared }
    }

    /// 最新快照（阻塞至首帧，超时返回带 pending 标记的空帧）。
    pub fn metrics(&self) -> Metrics {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let snap = self.shared.snapshot.lock().unwrap();
            if let Some(m) = snap.as_ref() {
                return m.clone();
            }
            drop(snap);
            if Instant::now() >= deadline {
                let mut m = Metrics::default();
                m.ts_ms = now_ms();
                m.cpu.temp_source = "pending".into();
                return m;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    pub fn info(&self) -> &Info {
        &self.shared.info
    }

    pub fn stop(&self) {
        self.shared.stop.store(true, Ordering::Release);
    }
}

struct Sampler {
    sys: sysinfo::System,
    #[cfg(windows)]
    pawnio: Option<pawnio::PawnIo>,
    #[cfg(not(windows))]
    pawnio: Option<()>,
    nvml: Option<gpu::NvmlGpu>,
    id: amd_temp::CpuIdentity,
    cpu_name: String,
    #[cfg(windows)]
    energy_unit_j: Option<f64>,
    #[cfg(windows)]
    last_energy: Option<(u32, Instant)>,
    wmi_cache: Option<f32>,
    /// 存储指标缓存（SMART 查询较重，每 5s 刷一次）
    storage: Vec<StorageMetrics>,
    /// 磁盘活动率/吞吐采样器（计数器每帧差分一次，很轻）
    storage_perf: storage::PerfState,
    /// NVMe 设备计数器差分器（IOCTL_DISK_PERFORMANCE 不可用时的吞吐兜底，5s 粒度）
    storage_units: storage::ThroughputState,
    #[cfg(windows)]
    smu: Option<smu::SmuClient>,
    #[cfg(not(windows))]
    smu: Option<()>,
    /// SMU 传感器缓存（PM 表读一次要 update+read 两个 IOCTL，每 1s 刷一次）
    smu_sensors: Vec<SmuSensor>,
    #[cfg(windows)]
    superio: Option<superio::SuperIo>,
    #[cfg(not(windows))]
    superio: Option<()>,
    /// SuperIO 读数缓存（每 1s 刷一次；持不到 ISA 总线锁时留上一帧）
    superio_metrics: Option<SuperIoMetrics>,
    /// PIIX4 SMBus 句柄（内存 SPD 用；无权限时为 None）
    #[cfg(windows)]
    spd_bus: Option<smbus::Piix4>,
    #[cfg(not(windows))]
    spd_bus: Option<()>,
    /// 内存模组缓存（身份静态，温度每 5s 刷一次）
    dimms: Vec<DimmMetrics>,
    /// 物理核拓扑（物理核 → 逻辑处理器列表），启动时一次
    core_groups: Vec<Vec<u32>>,
    /// 每核 MSR 基线（APERF/MPERF/能耗计数器 + 时间戳）
    core_baselines: Vec<CoreBaseline>,
    /// 每核读数缓存
    core_metrics: Vec<CpuCoreMetrics>,
    tick: u64,
}

/// 每核计数器的上一次采样值。APERF/MPERF/能耗都是**每核独立**的累加计数器，
/// 差分必须与「上一次在同一个核上读到的值」配对，所以基线要按核存。
#[derive(Clone, Copy, Default)]
struct CoreBaseline {
    aperf: u64,
    mperf: u64,
    energy: Option<u32>,
    at: Option<Instant>,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn sampler_loop(mut s: Sampler, shared: Arc<Shared>) {
    // sysinfo 需要两次刷新间隔 >= MINIMUM_CPU_UPDATE_INTERVAL 才有占用率
    std::thread::sleep(Duration::from_millis(350));
    loop {
        if shared.stop.load(Ordering::Acquire) {
            return;
        }
        let m = s.tick();
        *shared.snapshot.lock().unwrap() = Some(m);

        // 分段睡眠，及时响应 stop
        let deadline = Instant::now() + TICK;
        while Instant::now() < deadline {
            if shared.stop.load(Ordering::Acquire) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Sampler {
    fn tick(&mut self) -> Metrics {
        self.tick += 1;
        self.sys.refresh_cpu_usage();
        self.sys.refresh_memory();

        let per_core: Vec<f32> = self.sys.cpus().iter().map(|c| c.cpu_usage()).collect();
        let usage = self.sys.global_cpu_usage();
        let freq_mhz = self.sys.cpus().iter().map(|c| c.frequency()).max().unwrap_or(0);
        let mem_total_gb = (self.sys.total_memory() as f64 / GB) as f32;
        let mem_used_gb = (self.sys.used_memory() as f64 / GB) as f32;
        let mem_pct = if mem_total_gb > 0.0 {
            mem_used_gb / mem_total_gb * 100.0
        } else {
            0.0
        };

        let (temp, source, ccds, pkg_power) = self.cpu_temp();

        let gpu = if let Some(n) = self.nvml.as_ref() {
            Some(n.poll())
        } else if self.tick % 2 == 1 {
            // nvidia-smi 子进程降级，隔 1s 一次，省进程开销
            gpu::nvidia_smi_poll()
        } else {
            None
        };

        // 存储 SMART：每 5s（10 tick）刷一次，其余帧用缓存
        if self.tick % 10 == 1 {
            self.storage = storage::poll();
            // 设备计数器差分（吞吐兜底）：与 SMART 同频，得到的是 5s 区间平均
            self.storage_units.apply(&mut self.storage);
        }

        // 磁盘活动率/吞吐：每帧采一次（每块盘一个 IOCTL，成本几十 µs）。
        // 速率靠与上一帧配对，所以必须每帧都采，不能跟 SMART 一起降到 5s。
        let perf = self.storage_perf.sample();
        storage::merge_perf(&mut self.storage, &perf);

        // SMU PM 表：每 1s（2 tick）刷一次。读失败（拿不到 PCI 锁等）就留上一帧，
        // 宁缺毋脏。
        #[cfg(windows)]
        if self.tick % 2 == 1 {
            if let Some(c) = self.smu.as_ref() {
                if let Some(list) = c.read_sensors() {
                    self.smu_sensors = list;
                }
            }
        }

        // SuperIO（主板风扇/板温/电压）：每 2s（4 tick）刷一次 —— 28 个温度下标 + 16 路
        // 电压 + 7 路风扇，每个寄存器要 4 次端口 I/O，比 SMU 重得多。
        #[cfg(windows)]
        if self.tick % 4 == 1 {
            if let Some(s) = self.superio.as_ref() {
                if let Some(sensors) = s.read_sensors() {
                    self.superio_metrics = Some(SuperIoMetrics {
                        chip: s.chip_name().to_string(),
                        profile: s.profile_id().to_string(),
                        sensors,
                    });
                }
            }
        }

        // 内存 SPD 温度：每 5s（10 tick）刷一次。身份信息（型号/序列号）是静态的，
        // 启动时读过一次就不再碰 EEPROM 页，避免每帧几十次 1ms 级字节读。
        #[cfg(windows)]
        if self.tick % 10 == 1 {
            if let Some(bus) = self.spd_bus.as_ref() {
                spd::refresh_temperatures(bus, &mut self.dimms);
            }
        }

        // 每核 MSR（时钟/有效频率/功耗）：每帧都读。8 核 × 4 个 MSR 约 32 次 IOCTL，
        // 单次 ~20µs，总开销在 1ms 量级，比 SMU/SuperIO 轻得多；读之前按核钉线程。
        #[cfg(windows)]
        self.sample_cores();

        Metrics {
            ts_ms: now_ms(),
            cpu: CpuMetrics {
                name: self.cpu_name.clone(),
                usage_pct: usage,
                per_core_pct: per_core,
                freq_mhz,
                temp_c: temp,
                temp_source: source,
                ccd_temps_c: ccds,
                package_power_w: pkg_power,
                core_voltage_v: smu::find(&self.smu_sensors, "VDDCR", "voltage"),
                soc_voltage_v: smu::find(&self.smu_sensors, "VDDCR SoC", "voltage"),
                smu: self.smu_sensors.clone(),
                per_core: self.core_metrics.clone(),
            },
            memory: MemoryMetrics {
                usage_pct: mem_pct,
                used_gb: mem_used_gb,
                total_gb: mem_total_gb,
            },
            gpu,
            storage: self.storage.clone(),
            superio: self.superio_metrics.clone(),
            dimms: self.dimms.clone(),
        }
    }

    /// CPU 温度（+ CCD + 功耗）。返回 (温度, 来源标签, CCD 列表, 整包功耗 W)。
    fn cpu_temp(&mut self) -> (Option<f32>, String, Vec<f32>, Option<f32>) {
        #[cfg(windows)]
        if let Some(p) = self.pawnio.as_ref() {
            // 与 LHM/FanControl 等工具共享 PCI 总线互斥锁（拿不到锁跳过本轮，
            // 宁缺毋脏）
            let _pci = pawnio::PciBusGuard::wait(5000);

            let temp = p
                .read_smn(amd_temp::SMN_THM_TCON_CUR_TMP)
                .map(amd_temp::tctl_from_raw);

            let ccds = amd_temp::ccd_layout(self.id)
                .map(|base| {
                    (0..amd_temp::CCD_COUNT)
                        .filter_map(|i| p.read_smn(base + i * 4))
                        .map(amd_temp::ccd_temp_from_raw)
                        .filter(|t| (0.0..125.0).contains(t))
                        .collect::<Vec<f32>>()
                })
                .unwrap_or_default();

            // RAPL 整包功耗
            let power = pkg_power_step(
                p,
                &mut self.energy_unit_j,
                &mut self.last_energy,
            );

            return (temp, "pawnio-smn".into(), ccds, power);
        }

        // WMI 兜底（每 5s 刷一次缓存）
        if self.tick % 10 == 1 || self.wmi_cache.is_none() {
            #[cfg(windows)]
            {
                self.wmi_cache = wmi_temp::thermal_zone_max_temp_c();
            }
        }
        match self.wmi_cache {
            Some(t) => (Some(t), "wmi-thermal".into(), vec![], None),
            None => (None, "none".into(), vec![], None),
        }
    }

    /// 逐核读 MSR（时钟 / 有效频率 / 功耗）。
    ///
    /// 三条通路的共同前提：**读之前把线程钉到目标核**（`rdmsr` 是每核寄存器）。
    /// - 瞬时频率：`HW_PSTATE_STATUS` 的 CpuFid（Zen5: Fid[11:0]×5 MHz）
    /// - 有效频率：APERF 增量 ÷ 窗口（APERF 只在实际执行时计数，故摊出来是平均值）
    /// - 每核功耗：`CORE_ENERGY_STAT` 差分 × 能量单位 ÷ 时间
    ///
    /// 首帧只建立基线（差分需要两次采样），计数器倒挂或跳变过大时丢弃该轮并重建基线。
    #[cfg(windows)]
    fn sample_cores(&mut self) {
        let Some(p) = self.pawnio.as_ref() else {
            return;
        };
        if self.core_groups.is_empty() {
            return;
        }
        if self.energy_unit_j.is_none() {
            self.energy_unit_j = amd_temp::read_energy_unit_j(p);
        }
        let unit_j = self.energy_unit_j.unwrap_or(0.0);

        if self.core_baselines.len() != self.core_groups.len() {
            self.core_baselines = vec![CoreBaseline::default(); self.core_groups.len()];
        }

        // 与 cpu_temp 共用同一把 PCI 锁（两次取锁互不嵌套，不会自死锁）
        let _pci = pawnio::PciBusGuard::wait(1000);

        let mut out = Vec::with_capacity(self.core_groups.len());
        for (idx, group) in self.core_groups.iter().enumerate() {
            let thread = group[0]; // 与 LHM 一致：取该核的第一个线程
            let Some(_aff) = cpu_topology::Affinity::pin(thread) else {
                continue;
            };
            let pstate = amd_temp::read_pstate_status(p);
            let aperf = p.read_msr(amd_temp::MSR_APERF_RO);
            let mperf = p.read_msr(amd_temp::MSR_MPERF_RO);
            let energy = amd_temp::read_core_energy(p);
            let now = Instant::now();
            // _aff 在此 drop → 恢复调度器原样

            let base = self.core_baselines[idx];
            // 瞬时频率（不依赖基线）
            let raw_clock = match pstate {
                Some(eax) if self.id.family >= 0x1A => amd_temp::core_clock_mhz_zen5(eax),
                Some(eax) => amd_temp::core_clock_mhz_legacy(eax, 100.0),
                None => 0.0,
            };

            let mut clock_mhz = raw_clock;
            let mut effective_mhz = 0.0;
            let mut ratio = None;
            let mut power_w = None;

            if let (Some(at), Some(a1), Some(m1)) = (base.at, aperf, mperf) {
                let dt_s = now.duration_since(at).as_secs_f64();
                match (
                    amd_temp::counter_delta(a1, base.aperf),
                    amd_temp::counter_delta(m1, base.mperf),
                ) {
                    (Some(da), Some(dm)) if da > 0 && dm > 0 && dt_s > 0.0 => {
                        effective_mhz = amd_temp::effective_clock_mhz(da, dt_s * 1e6);
                        ratio = Some((da as f64 / dm as f64) as f32);
                        clock_mhz = amd_temp::ratio_adjusted_clock_mhz(raw_clock, da, dm);
                    }
                    _ => {
                        // 计数器倒挂/跳变：本轮不给差值，只重建基线
                        self.core_baselines[idx] = CoreBaseline {
                            aperf: a1,
                            mperf: m1,
                            energy,
                            at: Some(now),
                        };
                        out.push(CpuCoreMetrics {
                            index: idx as u32,
                            thread,
                            clock_mhz: raw_clock as f32,
                            effective_mhz: 0.0,
                            power_w: None,
                            aperf_mperf_ratio: None,
                        });
                        continue;
                    }
                }

                if let (Some(e1), Some(e0)) = (energy, base.energy) {
                    let d = e1.wrapping_sub(e0) as u64;
                    if d < 20_000_000_000 {
                        power_w = amd_temp::calc_core_power_w(d, unit_j, dt_s);
                    }
                }
            }

            self.core_baselines[idx] = CoreBaseline {
                aperf: aperf.unwrap_or(base.aperf),
                mperf: mperf.unwrap_or(base.mperf),
                energy: energy.or(base.energy),
                at: Some(now),
            };

            out.push(CpuCoreMetrics {
                index: idx as u32,
                thread,
                clock_mhz: clock_mhz as f32,
                effective_mhz: effective_mhz as f32,
                power_w,
                aperf_mperf_ratio: ratio,
            });
        }
        self.core_metrics = out;
    }
}

/// RAPL 差分功耗（Windows PawnIO 通路）。能量单位只在首帧读一次。
#[cfg(windows)]
fn pkg_power_step(
    p: &pawnio::PawnIo,
    energy_unit: &mut Option<f64>,
    last: &mut Option<(u32, Instant)>,
) -> Option<f32> {
    if energy_unit.is_none() {
        *energy_unit = amd_temp::read_energy_unit_j(p);
    }
    let unit = (*energy_unit)?;
    let energy = amd_temp::read_pkg_energy(p)?;
    let power = amd_temp::calc_power_w(*last, energy, unit);
    *last = Some((energy, Instant::now()));
    power
}

// ---------------------------------------------------------------------------
// C ABI 导出

static MONITOR: OnceLock<Monitor> = OnceLock::new();

/// 初始化（幂等）。0 = 成功。
#[no_mangle]
pub extern "C" fn cs_init() -> i32 {
    MONITOR.get_or_init(Monitor::start);
    0
}

/// 版本号（静态内存，无需释放）。
#[no_mangle]
pub extern "C" fn cs_version() -> *const c_char {
    concat!(env!("CARGO_PKG_VERSION"), "\0").as_ptr() as *const c_char
}

/// 硬件静态信息 JSON。调用方负责 cs_free_string；未 init 返回 NULL。
#[no_mangle]
pub extern "C" fn cs_get_info_json() -> *mut c_char {
    let Some(m) = MONITOR.get() else { return std::ptr::null_mut() };
    to_cstring(serde_json::to_string(m.info()).ok())
}

/// 指标快照 JSON。调用方负责 cs_free_string；未 init 返回 NULL。
#[no_mangle]
pub extern "C" fn cs_get_metrics_json() -> *mut c_char {
    let Some(m) = MONITOR.get() else { return std::ptr::null_mut() };
    to_cstring(serde_json::to_string(&m.metrics()).ok())
}

/// 释放 JSON 字符串。
#[no_mangle]
pub unsafe extern "C" fn cs_free_string(p: *mut c_char) {
    if !p.is_null() {
        drop(CString::from_raw(p));
    }
}

/// 停止采样线程（幂等）。
#[no_mangle]
pub extern "C" fn cs_shutdown() {
    if let Some(m) = MONITOR.get() {
        m.stop();
    }
}

fn to_cstring(json: Option<String>) -> *mut c_char {
    match json.and_then(|s| CString::new(s).ok()) {
        Some(c) => c.into_raw(),
        None => std::ptr::null_mut(),
    }
}
