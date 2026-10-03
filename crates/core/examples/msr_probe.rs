//! AMD 每核时钟 / VID 电压 / 核心功耗探针（LHM `Amd17Cpu.Core` + `CpuThread` 的等价实现）。
//!
//! 目的：在**集成进采样循环之前**先验证三条 MSR 通路在本机（Zen5 / family 0x1A）能不能读、
//! 数值量级对不对。输出刻意全 ASCII（提权重定向按 GBK 解码会把中文变乱码）。
//!
//! 步骤：
//!   [1] 用 GetLogicalProcessorInformationEx 拿到「物理核 → 逻辑处理器」分组（LHM 靠 CpuId 拓扑）
//!   [2] 逐核读 MSR 0xC0010293（HW P-state Status）→ CurHwPstate / CurCpuVid / CurCpuFid
//!   [3] 逐核读 0xC00000E8 / 0xC00000E7（APERF/MPERF）两次 → Effective Clock
//!   [4] 逐核读 0xC001029A（CORE_ENERGY_STAT）两次 → 每核功耗
//!   [5] 读 0xC001029B（PKG_ENERGY_STAT）→ 整包功耗，与「各核之和」对比
//!   [6] 校验：读 MSR 前必须把线程钉到目标核上，验证不钉会怎样
#![cfg(windows)]

use std::time::{Duration, Instant};

use windows_sys::Win32::System::SystemInformation::{
    GetLogicalProcessorInformationEx, RelationProcessorCore,
    SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX, GROUP_AFFINITY,
};
use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadAffinityMask};

const MSR_APERF_RO: u32 = 0xC000_00E8;
const MSR_MPERF_RO: u32 = 0xC000_00E7;
const MSR_HW_PSTATE_STATUS: u32 = 0xC001_0293;
const MSR_CORE_ENERGY_STAT: u32 = 0xC001_029A;
const MSR_PKG_ENERGY_STAT: u32 = 0xC001_029B;
const MSR_PWR_UNIT: u32 = 0xC001_0299;

/// 把当前线程钉到 `logical` 号逻辑处理器，返回上一份掩码（0 = 失败）。
fn pin(logical: u32) -> usize {
    unsafe { SetThreadAffinityMask(GetCurrentThread(), 1usize << logical) }
}

fn unpin(previous: usize) {
    if previous != 0 {
        unsafe { SetThreadAffinityMask(GetCurrentThread(), previous) };
    }
}

/// 「物理核 → 该核的逻辑处理器列表」。本机 8 核 16 线程，SMT 兄弟的编号排布由固件决定，
/// 所以不能假设 i / i+8，必须问系统。
fn core_groups() -> Vec<Vec<u32>> {
    let mut len: u32 = 0;
    unsafe { GetLogicalProcessorInformationEx(RelationProcessorCore, std::ptr::null_mut(), &mut len) };
    if len == 0 {
        return Vec::new();
    }
    let mut buf = vec![0u8; len as usize + 1024];
    let ok = unsafe {
        GetLogicalProcessorInformationEx(
            RelationProcessorCore,
            buf.as_mut_ptr() as *mut SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX,
            &mut len,
        )
    };
    if ok == 0 {
        return Vec::new();
    }

    let mut groups = Vec::new();
    let mut off = 0usize;
    while off + 8 <= len as usize {
        let ent = unsafe { &*(buf.as_ptr().add(off) as *const SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX) };
        if ent.Size == 0 {
            break;
        }
        if ent.Relationship == RelationProcessorCore {
            let proc = unsafe { &ent.Anonymous.Processor };
            // GroupMask 声明长度是 1，GroupCount > 1 时要用指针算术取后续项。
            let first: *const GROUP_AFFINITY = &proc.GroupMask[0] as *const _;
            let mut threads = Vec::new();
            for g in 0..proc.GroupCount as usize {
                let ga = unsafe { &*first.add(g) };
                for bit in 0..64u32 {
                    if ga.Mask & (1usize << bit) != 0 {
                        threads.push(ga.Group as u32 * 64 + bit);
                    }
                }
            }
            if !threads.is_empty() {
                groups.push(threads);
            }
        }
        off += ent.Size as usize;
    }
    groups
}

fn main() {
    println!("[1] 物理核拓扑（GetLogicalProcessorInformationEx / RelationProcessorCore）");
    let groups = core_groups();
    if groups.is_empty() {
        println!("    [!] 拿不到拓扑，退出");
        return;
    }
    for (i, g) in groups.iter().enumerate() {
        println!("    core {i}: threads {g:?}");
    }

    println!("\n[2] 打开 PawnIO 并加载 AMDFamily17.bin ...");
    let bin: &[u8] = include_bytes!("../../../drivers/pawnio/AMDFamily17.bin");
    let p = match cs_core::pawnio::PawnIo::open(bin) {
        Some(p) => p,
        None => {
            println!("    PawnIO 打开失败（驱动未装/需要管理员权限）");
            return;
        }
    };
    println!("    已打开");

    let guard = cs_core::pawnio::PciBusGuard::wait(5000);
    println!("    Global\\Access_PCI 锁: {}", if guard.is_some() { "已持有" } else { "未拿到（继续）" });

    println!("\n[3] 逐核 MSR 0xC0010293 (HW P-state Status)");
    println!("    pstate  vid     fid     -> Vcore(V)  CoreCOF(MHz)   [Zen5: Fid[11:0] * 5MHz]");
    for (i, g) in groups.iter().enumerate() {
        let prev = pin(g[0]);
        if prev == 0 {
            println!("    core {i}: SetThreadAffinityMask 失败");
            continue;
        }
        let raw = p.read_msr(MSR_HW_PSTATE_STATUS);
        unpin(prev);
        match raw {
            Some(v) => {
                let eax = v as u32;
                let pstate = (eax >> 22) & 0x7;
                let vid = (eax >> 14) & 0xFF;
                let dfs = (eax >> 8) & 0x3F;
                let fid = eax & 0xFFF;
                let vcore = 1.550 - 0.00625 * vid as f64;
                println!(
                    "    core {i} (thread {}): raw={:016X} pstate={} vid=0x{:02X} dfsid=0x{:02X} fid=0x{:03X} -> {:.4} V  {} MHz",
                    g[0], v, pstate, vid, dfs, fid, vcore, fid * 5
                );
            }
            None => println!("    core {i}: 读失败"),
        }
    }

    println!("\n[4] 逐核 APERF/MPERF 两次采样（间隔 500ms）-> Effective Clock");
    let mut first = Vec::new();
    for g in &groups {
        let prev = pin(g[0]);
        let aperf = p.read_msr(MSR_APERF_RO);
        let mperf = p.read_msr(MSR_MPERF_RO);
        unpin(prev);
        first.push((aperf, mperf));
    }
    let t0 = Instant::now();
    std::thread::sleep(Duration::from_millis(500));
    let dt_us = t0.elapsed().as_secs_f64() * 1e6;
    println!("    采样窗口 {:.0} us", dt_us);
    for (i, g) in groups.iter().enumerate() {
        let prev = pin(g[0]);
        let aperf = p.read_msr(MSR_APERF_RO);
        let mperf = p.read_msr(MSR_MPERF_RO);
        unpin(prev);
        match (first[i].0, first[i].1, aperf, mperf) {
            (Some(a0), Some(m0), Some(a1), Some(m1)) => {
                let da = a1.wrapping_sub(a0);
                let dm = m1.wrapping_sub(m0);
                let eff = da as f64 / dt_us;
                let ratio = if dm > 0 { da as f64 / dm as f64 } else { 0.0 };
                println!(
                    "    core {i} (thread {}): dAPERF={:>10} dMPERF={:>10} aperf/mperf={:.3}  effective={:.0} MHz",
                    g[0], da, dm, ratio, eff
                );
            }
            _ => println!("    core {i}: 读失败"),
        }
    }

    println!("\n[5] 逐核 MSR 0xC001029A (CORE_ENERGY_STAT) 两次采样 -> 每核功耗");
    let unit_raw = p.read_msr(MSR_PWR_UNIT);
    let esu = unit_raw.map(|v| ((v as u32 >> 8) & 0x1F) as i32).unwrap_or(16);
    let unit_j = 2f64.powi(-esu);
    println!("    MSR_PWR_UNIT ESU[12:8]={esu} -> {unit_j} J/增量（=2^-{esu}）");

    let mut e0 = Vec::new();
    for g in &groups {
        let prev = pin(g[0]);
        let e = p.read_msr(MSR_CORE_ENERGY_STAT);
        unpin(prev);
        e0.push(e);
    }
    let pkg0 = {
        let prev = pin(groups[0][0]);
        let e = p.read_msr(MSR_PKG_ENERGY_STAT);
        unpin(prev);
        e
    };
    let t1 = Instant::now();
    std::thread::sleep(Duration::from_millis(1000));
    let dt_s = t1.elapsed().as_secs_f64();
    println!("    采样窗口 {dt_s:.3} s");

    let mut sum = 0.0f64;
    for (i, g) in groups.iter().enumerate() {
        let prev = pin(g[0]);
        let e = p.read_msr(MSR_CORE_ENERGY_STAT);
        unpin(prev);
        if let (Some(a), Some(b)) = (e0[i], e) {
            let d = (b as u32).wrapping_sub(a as u32) as f64;
            let w = d * unit_j / dt_s;
            sum += w;
            println!("    core {i} (thread {}): energy {:08X} -> {:08X}  delta={:>12}  {:.2} W", g[0], a as u32, b as u32, d as u64, w);
        }
    }
    let pkg1 = {
        let prev = pin(groups[0][0]);
        let e = p.read_msr(MSR_PKG_ENERGY_STAT);
        unpin(prev);
        e
    };
    if let (Some(a), Some(b)) = (pkg0, pkg1) {
        let d = (b as u32).wrapping_sub(a as u32) as f64;
        let pkg_w = d * unit_j / dt_s;
        println!("    整包(PKG_ENERGY_STAT): delta={} -> {:.2} W", d as u64, pkg_w);
        println!("    各核之和 = {:.2} W   （应 <= 整包 {:.2} W，差值属于 SoC/IO/内存控制器等非核心域）", sum, pkg_w);
    }

    println!("\n[6] 钉核 vs 不钉核：同一 MSR 连续读 3 次的差异");
    for label in ["不钉核", "钉 core0"] {
        let prev = if label == "钉 core0" { pin(groups[0][0]) } else { 0 };
        let vals: Vec<String> = (0..3)
            .map(|_| match p.read_msr(MSR_HW_PSTATE_STATUS) {
                Some(v) => format!("{:08X}", v as u32),
                None => "fail".into(),
            })
            .collect();
        unpin(prev);
        println!("    {label}: {}", vals.join("  "));
    }
    let cur = {
        let prev = pin(groups[0][0]);
        let v = p.read_msr(MSR_MPERF_RO);
        unpin(prev);
        v
    };
    println!("\n    参考：钉 core0 时读 MPERF = {:?}", cur.map(|v| v as u64));
    println!("    参考：当前线程未钉核时读 MPERF 会随调度漂移（这就是必须先 SetThreadAffinityMask 的原因）");
    println!("\n[7] 负载下扫描 0xC0010293 的各个 8 位窗口 + 与 SMU PM 表对照");
    println!("    问题：LHM 用 [21:14] 当 CurCpuVid 算 Vcore = 1.550 - 0.00625*vid，本机空载读到 0.19 V（荒谬）");
    println!("    同时 LHM 的 Core Clock = fid[11:0]*5 会给出 5570 MHz，而 APERF 实测只有 ~2447 MHz");
    println!("    做法：把核心 7 打满，看哪个窗口的电压解读会随负载变到 Vcore 量级，以及 fid 是否随负载下降");

    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let busy_core = groups[7][0];
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    let busy = std::thread::spawn(move || {
        let prev = pin(busy_core);
        let mut x = 1.0f64;
        while !flag.load(Ordering::Relaxed) {
            for _ in 0..200_000 {
                x = (x * 1.0000001 + 0.5).sin().abs();
            }
            std::hint::black_box(x);
        }
        unpin(prev);
    });
    std::thread::sleep(Duration::from_millis(1500));

    let (idle_raw, a0) = {
        let prev = pin(busy_core);
        let r = p.read_msr(MSR_HW_PSTATE_STATUS);
        let a = p.read_msr(MSR_APERF_RO);
        unpin(prev);
        (r, a)
    };
    std::thread::sleep(Duration::from_millis(400));
    let (_, a1) = {
        let prev = pin(busy_core);
        let r = p.read_msr(MSR_HW_PSTATE_STATUS);
        let a = p.read_msr(MSR_APERF_RO);
        unpin(prev);
        (r, a)
    };
    stop.store(true, Ordering::Relaxed);
    let _ = busy.join();

    if let Some(v) = idle_raw {
        let eax = v as u32;
        println!("    core 7 满载时 0xC0010293 raw={:016X}", v);
        for (lo, hi) in [(14u32, 21u32), (15, 22), (16, 23), (17, 24)] {
            let field = (eax >> lo) & 0xFF;
            let volts = 1.550 - 0.00625 * field as f64;
            println!("      [{hi}:{lo}] = 0x{field:02X} ({field:3})  -> {volts:.4} V");
        }
        let fid = eax & 0xFFF;
        println!("      fid[11:0] = 0x{fid:03X} -> {fid} MHz*5 = {} MHz", fid * 5);
        let dfs = (eax >> 8) & 0x3F;
        println!("      dfsid[13:8] = 0x{dfs:02X}");
    }
    if let (Some(a0), Some(a1)) = (a0, a1) {
        println!(
            "    core 7 满载 APERF 实测平均时钟 = {:.0} MHz（400ms 窗口）",
            a1.wrapping_sub(a0) as f64 / 400_000.0
        );
    }

    if let Some(smu) = cs_core::smu::SmuClient::open() {
        if let Some(f) = smu.read_floats_sized(0x948) {
            let at = |i: usize| f.get(i).copied().unwrap_or(f32::NAN);
            println!("    SMU PM 表（同一时刻）:");
            println!("      VDDCR [47] = {:.4} V", at(47));
            let a: Vec<String> = (39..47).map(|i| format!("{:.4}", at(i))).collect();
            println!("      [39..46]  = {}", a.join(" "));
            let b: Vec<String> = (309..317).map(|i| format!("{:.4}", at(i))).collect();
            println!("      [309..316]= {}", b.join(" "));
            let t: Vec<String> = (317..325).map(|i| format!("{:.1}", at(i))).collect();
            println!("      [317..324] 温度 = {}", t.join(" "));
        }
    } else {
        println!("    SMU 客户端打开失败（跳过对照）");
    }

    println!("\n[8] 结论");
    println!("    逐核时钟/VID/功耗三条 MSR 通路若上面均有合理数值，即可集成进采样循环。");
    drop(guard);
}
