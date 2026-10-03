# computer-status

实时展示 CPU / 内存 / GPU / 显存占用与温度的小工具。前后端分离：核心是 **Rust 编译的动态库 `cs_core.dll`（C ABI）**，展示层只是它的消费者，随时可换（当前自带一个 CLI 面板，Web 壳下一轮做）。

```
┌─────────────────────────┐      C ABI (JSON in/out)      ┌──────────────┐
│  cs_core.dll (Rust)     │ ◄──────────────────────────► │  展示层       │
│  采样线程 500ms 一帧     │   cs_init / cs_get_metrics   │  CLI ✓       │
│  温度/功耗多源降级链      │   cs_get_info / cs_shutdown  │  Web (下轮)  │
└─────────────────────────┘                               └──────────────┘
```

## 快速开始

```powershell
cargo build --release          # 产物: target\release\cs_core.dll + cs-cli.exe
.\target\release\cs-cli.exe            # 单帧面板
.\target\release\cs-cli.exe --watch    # 持续刷新 (Ctrl+C 退出)
.\target\release\cs-cli.exe --jsonl    # 每帧一行 JSON（喂管道/其他工具）
.\target\release\cs-cli.exe --info     # 硬件静态信息 + 数据源诊断
```

国内网络拉依赖（crates.io 直连失败时）：

```powershell
cargo --config 'source.crates-io.replace-with="rsproxy-sparse"' `
      --config 'source.rsproxy-sparse.registry="sparse+https://rsproxy.cn/index/"' `
      build --release
```

## C ABI（给任何宿主语言用）

```c
int      cs_init();                 // 初始化（幂等），0 = 成功
const char* cs_version();           // 静态字符串，无需释放
char*    cs_get_info_json();        // 硬件静态信息 JSON，用完 cs_free_string
char*    cs_get_metrics_json();     // 最新一帧指标 JSON，用完 cs_free_string
void     cs_free_string(char* p);
void     cs_shutdown();
```

Python ctypes 最小示例（下轮 Web 壳就这么接）：

```python
import ctypes, json
dll = ctypes.CDLL(r"target\release\cs_core.dll")
dll.cs_init()
dll.cs_get_metrics_json.restype = ctypes.c_char_p   # 注意：见下
raw = dll.cs_get_metrics_json()
m = json.loads(ctypes.c_char_p(raw).value)          # 拷贝后再释放
dll.cs_free_string(raw)
```

## 数据源与降级链

| 指标 | 首选 | 降级 | 兜底 |
|---|---|---|---|
| CPU 温度/CCD/功耗 (AMD Zen) | 自研 PawnIO + SMN/MSR（LHM 同源算法） | WMI ACPI 热区 | `null`（诚实报 n/a） |
| CPU 电压/分项功率 (Zen4/5) | SMU PM 表（`RyzenSMU.bin`） | SVI2 电压（Zen4/5 上 LHM 已主动放弃，不移植） | `null` |
| NVMe 温度/磨损/写入量 | `StorageDeviceTemperatureProperty`(52) + `StorageDeviceProtocolSpecificProperty`(50) 读 SMART 健康日志 | 只报盘名（`source:"none"`） | — |
| CPU 占用/频率/内存 | sysinfo | — | — |
| GPU 全指标 | NVML（进程内, 0 开销） | nvidia-smi 子进程 | `null` |

- **存储 SMART 不需要管理员权限**：`IOCTL_STORAGE_QUERY_PROPERTY` 是 `FILE_ANY_ACCESS`，非提权句柄（`FILE_READ_ATTRIBUTES`）即可读全部 NVMe SMART 字段。
- **CPU 温度需要管理员权限**：PawnIO 驱动设备（`\\?\GLOBALROOT\Device\PawnIO`）只对管理员开放，非提权时 `Info.sources.pawnio_access_denied = true`，CLI 面板会打一行提示。右键“以管理员身份运行”即可解锁温度/CCD/整包功耗。
- PawnIO 是正规签名驱动（namazso，HVCI 兼容，不在微软黑名单），本机已装 v2.1.0。模块 `drivers/pawnio/AMDFamily17.bin` 为 LHM 官方签发字节码（MPL-2.0，见 COPYING）。
- 与 LHM/FanControl 共享 `Global\Access_PCI` 全局互斥锁，可同时运行互不踩踏。
- RAPL 能量单位：`MSR_PWR_UNIT(0xC0010299)[12:8] = ESU`，**单位是 1/2^ESU 焦耳**（不是微焦——LHM 源码此处注释有误导，其代码本身按焦耳算才是对的），故 `W = Δcounts × 2^-ESU ÷ Δt`。本机 ESU=16。
- **SMU PM 表**（电压/分项功率）：走 `RyzenSMU.bin` 的 `ioctl_resolve_pm_table` / `ioctl_read_pm_table`，同样是**提权**能力。本机固件 PM 表版本 `0x00620105` 连 LHM master 与 `ryzen_smu` 都未收录，我们按“只收编能与独立真值对拍上的字段”实测逆向出一个 Zen5 子集（详见 `PORTING.md`）。

## 本机实测（9850X3D + RTX 5080）

非提权（`cs-cli.exe`）：

```
CPU     AMD Ryzen 7 9850X3D 8-Core Processor usage  46.8%   4700 MHz    n/a
        ! CPU 温度/功耗需要管理员权限（PawnIO 设备只对管理员开放）
CORES   ██████▌··· █████▌···· ...（16 逻辑核）
MEM      24.3 /  61.7 GB  ( 39.4%)
GPU     NVIDIA GeForce RTX 5080      usage  65.0%   10192/16303  MiB ( 62.5%)   57.0°C   205.1 W  fan  41%
DISK0   Samsung SSD 990 EVO 2TB       53.9°C  wear   0%  wrote    8939 GB     541 h
DISK1   Fanxiang S690 2TB             43.9°C  wear   0%  wrote   16337 GB    3564 h
DISK2   Samsung SSD 970 EVO Plus 2TB  53.9°C  wear   0%  wrote    6844 GB      69 h
```

提权后（`scripts\elev-verify.ps1`，经 UAC 一次确认）——CPU 温度/CCD/整包功耗 + SMU 电压/分项功率 + 三盘 SMART 全部解锁：

```
CPU     AMD Ryzen 7 9850X3D 8-Core Processor usage  38.9%   4700 MHz   69.0°C   95.3 W
CCD     66.8°C  (pawnio-smn)
VOLT    Core 1.3922 V   SoC n/a
SMU     CPU PPT 101.2W  Package 66.8°C  Core Power 107.6W  SOC Power 6.2W  Misc Power 9.2W  Total Power 129.5W
MEM      25.0 /  61.7 GB  ( 40.6%)
GPU     NVIDIA GeForce RTX 5080      usage  46.0%    9662/16303  MiB ( 59.3%)   54.0°C   157.7 W  fan  42%
DISK0   Samsung SSD 990 EVO 2TB       51.9°C  wear   0%  wrote    8941 GB     543 h
DISK1   Fanxiang S690 2TB             43.9°C  wear   0%  wrote   16337 GB    3565 h
DISK2   Samsung SSD 970 EVO Plus 2TB  51.9°C  wear   0%  wrote    6844 GB      69 h
```

同一帧内 **`Package` 与 SMN 的 `CCD` 相等（66.8°C）**、**`CPU PPT` 101.2 W 与 RAPL 的 95.3 W 同量级** —— SMU PM 表与 SMN/MSR 是两条完全独立的通路，互相印证说明下标选对了。

GPU 显存、温度、功耗、风扇在两种权限下都正常（NVML 不要求提权）。

## 结构

```
crates/core/          # cs_core.dll：采样引擎 + C ABI
  src/lib.rs          # Monitor 状态机 + 后台采样线程 + ABI 导出
  src/pawnio.rs       # PawnIO 驱动客户端（IOCTL 装载/执行签名字节码）
  src/amd_temp.rs     # AMD Zen 温度/功耗纯数学（SMN/MSR 解码，单测覆盖）
  src/wmi_temp.rs     # 手写 COM/WMI 兜底（windows-sys 无经典 WMI 接口）
  src/gpu.rs          # NVML（libloading）+ nvidia-smi 降级
  src/storage.rs      # NVMe SMART（IOCTL_STORAGE_QUERY_PROPERTY，纯解析函数单测覆盖）
  src/smu.rs          # SMU PM 表（Zen4 用 LHM 布局；Zen5 0x620105 为实测逆向子集）
  src/schema.rs       # JSON 契约（展示层唯一依赖的东西）
  examples/*.rs       # 探针：temp_probe / gpu_probe / wmi_probe / storage_probe / smu_probe
crates/cli/           # cs-cli.exe：面板/JSONL/--info，展示层参考实现
drivers/pawnio/       # 签名字节码模块 + MPL-2.0 COPYING
scripts/elev-verify.ps1  # 一次 UAC 提权验证（CPU 温度/功耗 + SMU + 存储 SMART）
scripts/elev-smu.ps1     # 只跑 SMU 探针（逆向 PM 表时用）
.ref/                 # 参考源码（只读，不参与构建）
```

### 构建注意

`cargo build --release --examples` 是**目标过滤器**，只建 lib + examples，**不会重链 `cs-cli.exe`**；改完核心代码后要跑 `cargo build --release`（不带 `--examples`）才会更新 CLI 二进制。`cargo test` 同样不重链该 exe。

## 已知边界

- GPU 显存 `used = total - free`（NVML 语义，含驱动预留；nvidia-smi 的数字不含，两者相差 ~4GB 属正常口径差）。
- WMI ACPI 热区在多数台式机主板上不可用（本机即无），它只是兜底链的一环。
- Windows 的存储 WMI 可靠性计数器（`MSFT_StorageReliabilityCounter` / `Get-StorageReliabilityCounter`）**确实需要管理员**，本机非提权报“无法从客户端中访问 CIM 资源”；本实现不依赖它。
- `STORAGE_PROPERTY_ID` 枚举在 `StorageAdapterCryptoProperty(17)` 之后**跳到 48**，协议专属属性是 49/50 而非 18/19 —— 曾因此得到统一的 `ERROR_INVALID_FUNCTION(1)` 而误判为权限问题。
- `--watch` 面板刷新与采样同为 500ms，个别帧可能重复上一帧数据（读写同相位竞争，自愈，不影响正确性）。
- **Zen5 的 SoC 电压拿不到**：本机 PM 表版本 `0x00620105` 无公开布局，LHM 在这台机器上自己也一个 SMU 传感器都不出。我们只把能与 RAPL / SMN 两个独立真值对拍上的下标收进布局（PPT、Package 温度、Core/SOC/Misc/Total 功率、VDDCR），因此 `soc_voltage_v` 恒为 `null` —— 宁可空着也不报一个错值。全表里 `[83]/[173]/[175]/[176]/[225]` 等 0.6~1.0 V 的候选值经两次运行比对是**固定标称值**（完全不变），不是遥测。
- Intel CPU 温度暂未实现（本机 AMD）；Linux 走 hwmon 的路线留了 cfg 分支。
