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
pub mod pawnio;
#[cfg(windows)]
pub mod smu;
#[cfg(windows)]
pub mod storage;
#[cfg(windows)]
pub mod wmi_temp;

/// 非 Windows 占位：存储 SMART 尚未实现
#[cfg(not(windows))]
pub mod storage {
    use crate::schema::StorageMetrics;
    pub fn poll() -> Vec<StorageMetrics> {
        Vec::new()
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

use std::ffi::c_char;
use std::ffi::CString;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use schema::{CpuMetrics, Info, MemoryMetrics, Metrics, SmuSensor, SourcesStatus, StorageMetrics};

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
        let storage_seed = storage::poll();
        sources.storage_smart = storage_seed.iter().any(|s| s.source == "nvme-smart");

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
            smu: smu_client,
            smu_sensors: Vec::new(),
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
    #[cfg(windows)]
    smu: Option<smu::SmuClient>,
    #[cfg(not(windows))]
    smu: Option<()>,
    /// SMU 传感器缓存（PM 表读一次要 update+read 两个 IOCTL，每 1s 刷一次）
    smu_sensors: Vec<SmuSensor>,
    tick: u64,
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
        }

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
            },
            memory: MemoryMetrics {
                usage_pct: mem_pct,
                used_gb: mem_used_gb,
                total_gb: mem_total_gb,
            },
            gpu,
            storage: self.storage.clone(),
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
