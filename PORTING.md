# LHM → Rust 移植进度

目标：照着 [LibreHardwareMonitor](https://github.com/LibreHardwareMonitor/LibreHardwareMonitor) 逐模块族把硬件监控能力用 Rust 重写进 `cs_core.dll`，边写边在本机（AMD 9850X3D + RTX 5080）验证。LHM 源码参考副本在 `.ref/LHM/LibreHardwareMonitor-master/`。

## 已完成

| 能力 | 对应 LHM | 实现位置 | 本机状态 |
|---|---|---|---|
| AMD Zen 温度 (Tctl / 每 CCD) | `Hardware/Cpu/Amd17Cpu.cs` | `src/amd_temp.rs` + `src/pawnio.rs` | ✅ 提权：Tctl 61.8°C，CCD0 49.1°C（单 CCD，其余槽位 raw=0 按 LHM 条件滤除） |
| 整包功耗 (RAPL) | `Amd17Cpu.cs:170-271` | `src/amd_temp.rs` | ✅ 提权：53.8~71.1 W（ESU=16） |
| WMI ACPI 热区兜底 | `Hardware/Cpu/GenericCpu.cs` | `src/wmi_temp.rs` | ⚠️ 本机主板不提供，仅作降级链一环 |
| CPU 占用/频率、内存 | `Hardware/Memory/GenericMemory.cs` | `src/lib.rs`（sysinfo） | ✅ 非提权 |
| GPU 全指标 | `Hardware/Gpu/NvidiaGpu.cs` | `src/gpu.rs`（NVML 进程内） | ✅ 非提权：温度/功耗/风扇/显存 |
| NVMe SMART | `Hardware/Storage/StorageDevice.cs`（LHM 委托 DiskInfoToolkit） | `src/storage.rs`（自研 IOCTL） | ✅ **非提权**：3 块盘的温度/磨损/写入量/通电时长 |
| CPU 电压 + 分项功率 | `Hardware/RyzenSMU.cs` + `Hardware/Cpu/Amd17Cpu.cs:405-418` | `src/smu.rs`（PM 表） | ✅ 提权：Vcore 1.34~1.40 V；`Package` 温度与 SMN 的 Tctl 逐帧吻合；SoC 电压本固件无法确证 → 留 `null` |

## 关键机制（踩过的坑，都已写进代码注释）

- **PawnIO 协议**：设备 `\\?\GLOBALROOT\Device\PawnIO`（仅管理员）；`IOCTL_LOAD_BINARY = 0xA1D22084`、`IOCTL_EXECUTE_FN = 0xA1D22104`（输入 = 32 字节 ASCII 函数名 + i64 参数，输出 i64 数组）。SMN/MSR 访问前需持有 `Global\Access_PCI` 互斥锁。
- **SMN**：`THM_TCON_CUR_TMP = 0x59800` → `(raw>>21)×125` 毫度，RANGE_SEL/TJ_SEL 置位则 −49℃；CCD 基址 `0x59b08`（model 0x61/0x44），过滤条件 `raw>0 && temp<125`。
- **RAPL 单位**：`MSR_PWR_UNIT(0xC0010299)[12:8] = ESU`，单位是 **1/2^ESU 焦耳**；`W = Δcounts × 2^-ESU ÷ Δt`。LHM 注释写“micro Joule”是误导（其代码按焦耳算才对）——按 µJ 理解会差 1e6 倍。
- **`STORAGE_PROPERTY_ID` 枚举陷阱**：`StorageAdapterCryptoProperty(17)` 之后**跳到 48**；协议专属属性 = **49**(Adapter) / **50**(Device)，温度属性 = **52**。传 18/19 会得到统一的 `ERROR_INVALID_FUNCTION(1)`，极易误判为权限问题（本项目就误判过一次）。
- **NVMe SMART 不需要管理员**：`IOCTL_STORAGE_QUERY_PROPERTY` 是 `FILE_ANY_ACCESS`。健康日志查询：输入 `STORAGE_PROPERTY_QUERY`（`AdditionalParameters` 起点放 `STORAGE_PROTOCOL_SPECIFIC_DATA`，`ProtocolDataOffset = 40` 即结构自身大小），输出 `STORAGE_PROTOCOL_DATA_DESCRIPTOR`（头 8 字节 + 协议结构 + 512 字节日志）。
- **存储 WMI 可靠性计数器确实要管理员**（本机非提权报“无法从客户端中访问 CIM 资源”），不用它。
- **Zen4/5 的死路**：`Amd17Cpu.cs:375-396` 对 model 0x61/0x44 主动置位 `smuSvi0Tfn`，使 Core/SoC 的 SVI2 电压传感器双双不激活 → 本机电压只能从 SMU PM 表拿，SVI2 路径（`F17H_M01H_SVI = 0x0005A000`）不要移植。
- **SMU PM 表协议**：`ioctl_get_code_name`（**不加** PCI 锁）→ `ioctl_resolve_pm_table`（out 2：version, tableBase）→ `ioctl_update_pm_table` + `ioctl_read_pm_table`（out = `(size+7)/8` 个 i64，持 `Global\Access_PCI`）。读回来的是 i64 数组，但**必须按原始字节重解释成 f32**（LHM `RyzenSMU.cs:257-266` 的 `Buffer.BlockCopy`），不是逐元素转浮点。本机：codeName=17(GraniteRidge)、tableBase=0x70D01000、SMU 版本 0x00625200。
- **PM 表版本 0x00620105 是业界未收录的新固件**：LHM master 的 `_supportedPmTableVersions` 只到 Zen4 的 `0x00540004`（另有 `0x00540104` 只在 `SetupPmTableSize` 里、**没有布局** → LHM 自己也不解它）；`ryzen_smu` master 的 `userspace/monitor_cpu.c` 里连 Granite Ridge 都没有。**LHM 在本机上零 SMU 传感器**。
- **未知表版本的逆向方法论**（可复用）：① 元数据先通（codeName/resolve/smu_version）确认卡点只是版本号；② 按已知族的尺寸试读，看能否解出合理 f32；③ **拿完全独立的第二个真值源逐项对拍**——我们用 RAPL 整包功耗（MSR `0xC001029B`）对 `[3] CPU PPT`、用 SMN Tctl（`0x59800`）对 `[11] Package` 温度，两者都与 PM 表无任何共享代码路径；④ 只有对拍上的下标才收编，对不上的（本机 `[48/49]` 读成电压而非 TDC/EDC 电流、`[52/57]` 读成 37~49 而非 SoC 电压、`[211]` 恒 3000、`[268]` 恒 120、`[539/540]` 恒 0）**全部丢弃**；⑤ 候选值若两次运行完全相同（`[83]=0.9000`、`[173]=0.9550`、`[175]=0.8550`、`[176]=0.7500`、`[225]=0.6000`），说明是固定标称值而非遥测，同样不能用。
- **PM 表结构线索**（Zen5，供后续扩展）：`[39..46]` 与 `[309..316]` 各是 8 个逐核电压（8 核），紧邻的 `[317..324]` 是 8 个逐核温度（实测 55.5~66.7°C，最高值贴近同时刻 Tctl）；`[389..396]` 是 8 个完全相同的 63.00（更像逐核限值而非实时温度）。日志/限值类固定值多成对出现（`[227]/[228]=48`、`[241]/[242]=60`、`[251]/[253]=25`、`[263]/[264]=32`、`[10]=95`、`[67]/[111]/[112]/[179..208]=100`）。

## 待移植

LHM 自带的 PawnIO 模块（`.ref/.../Resources/PawnIo/*.bin`，共 14 个，未全部用到）：
`AMDFamily0F 8644`、`AMDFamily10 16868`、`AMDFamily17 10652`(已用)、`IntelMSR 5324`、`IsaBridgeEC 51164`、`LpcACPIEC 2612`、`LpcCrOSEC 24908`、`LpcIO 18076`、`Nvidia 20796`、`RyzenSMU 50764`(已用)、`SmbusI801 55540`、`SmbusIntelSkylakeIMC 19828`、`SmbusNCT6793 21244`、`SmbusPIIX4 42404`

按对本机的价值排序：

1. **主板 SuperIO**（`LpcIO.bin` + `Hardware/Motherboard/Lpc/*`）：风扇转速、主板温度、+3.3V/+5V/+12V 电压。需先探测芯片型号（Nuvoton/ITE/Winbond）。**提权**。
2. **内存 DIMM 温度**（`SmbusNCT6793` / `SmbusPIIX4` + SMBus SPD）：桌面 AMD 平台 DIMM 温度支持依内存条而定。**提权**。
3. 其余族（Battery / Psu / PowerMonitor / Network / Controller）：对本机（台式机、无 UPS）价值低，可跳过。

> 已完成的 SMU 里程碑遗留：Zen5 的 **SoC 电压**仍无解（本固件无公开布局，候选值经二次比对均为固定标称值）。若将来需要，可用“空载 vs 满载两次全表 diff + 与 Vcore/功耗做相关性”的办法继续辨认，但当前按“不猜”原则留空。

## 排障工具

- `examples/temp_probe.rs` 逐级打点：PawnIO → CPUID → 互斥锁 → SMN → CCD → MSR 能量单位 → 逐帧功率。
- `examples/smu_probe.rs` 6 步打点：元数据 → 试读原始 f32 → **按 Zen4 下标打印实读值（对拍真值用）** → 全表扫描疑似电压/温度 → 解码结果。**逆向新表版本时先加这一步，别直接信下标表**。
- `examples/storage_probe.rs` 逐盘打印 access / 各属性 ID 组合的 err 与温度（`diagnose_nvme` 穷举）。
- `examples/gpu_probe.rs`、`examples/wmi_probe.rs` 分别隔离 NVML 与 COM/WMI。
- `scripts/elev-verify.ps1`：一次 UAC 确认跑完上面全部并落盘到 `%TEMP%\cs-elev\`；`scripts/elev-smu.ps1` 只跑 SMU 探针。脚本都走 `Start-Process pwsh -Verb RunAs -File <脚本>`（内联命令里的嵌套引号曾触发网关 JSON 报错）。
- 提权脚本用 `*>` 重定向落盘的是当前控制台代码页（GBK）解出的 UTF-8，中文行是乱码、**数字不受影响**；别照抄中文。

> 构建提示：`cargo build --release --examples` **只建 lib + examples，不重链 `cs-cli.exe`**；要更新 CLI 必须跑不带 `--examples` 的 `cargo build --release`。