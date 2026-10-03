//! 磁盘活动率/吞吐探针：验证两个通路的真伪与字段偏移。
//!
//! [0] `IOCTL_DISK_PERFORMANCE` 在物理盘与卷句柄上分别试一次（看 Windows 有没有关掉它）
//! [1] 逐盘读一次原始计数器（读不到就打印 GetLastError）
//! [2] 空载采两次、间隔已知 → 校验 `QueryTime` 的走速是不是墙钟的 10^7 倍
//!     （这一步验证的是结构体字段偏移，偏移错了走速就不可能对）
//! [3] 用**非缓冲**（FILE_FLAG_NO_BUFFERING | WRITE_THROUGH）写一个已知大小的文件，
//!     前后各采一次 → 计数器差分出的写吞吐 vs 文件实际吞吐、ΔBytesWritten vs 文件大小
//! [4] 同样非缓冲读回 → 读吞吐对拍
//! [5] NVMe 设备计数器对拍：ΔData Units × 512000 B 是否 ≈ 文件大小
//!     （这是 IOCTL 通路被 Windows 关掉时唯一的吞吐来源）
//! [6] 删除测试文件
//!
//! 两个码值（`0x70020` 是新版 winioctl.h 的 FILE_ANY_ACCESS 编码，`0x74020` 是旧文档写法）
//! × 两种访问级别（`GENERIC_READ` / `FILE_READ_ATTRIBUTES`）都试一遍，看驱动认哪个；
//! 实测**不需要管理员**，`FILE_READ_ATTRIBUTES` 句柄即可。设备计数器非提权也能读。
//! 用法：cargo run -p cs-core --example diskperf_probe --release [文件路径]

#![cfg(windows)]

use std::alloc::{alloc, dealloc, Layout};
use std::time::{Duration, Instant};

use cs_core::storage::{merge_perf, parse_disk_performance, units_rate_mib_s, PerfState};
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE,
};
/// FILE_READ_ATTRIBUTES：非提权时物理盘只肯给这个级别，够用（IOCTL 是 FILE_ANY_ACCESS）
const FILE_READ_ATTRIBUTES: u32 = 0x0080;
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, CREATE_ALWAYS, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_NO_BUFFERING,
    FILE_FLAG_WRITE_THROUGH, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::IO::DeviceIoControl;

const MIB: usize = 1024 * 1024;
/// 测试文件大小（MiB）
const FILE_MIB: usize = 512;
/// 单次读写块大小（必须 4096 对齐且是扇区大小整数倍）
const CHUNK: usize = MIB;
/// IOCTL_DISK_PERFORMANCE 的两个候选码值（winioctl.h 新版 = FILE_ANY_ACCESS → 0x70020，
/// 老版本/多数文档 = FILE_READ_ACCESS → 0x74020）。两个都试，看驱动认哪个。
const IOCTL_DISK_PERFORMANCE_CODES: [u32; 2] = [0x0007_0020, 0x0007_4020];

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 4096 字节对齐的缓冲区 —— 非缓冲 IO 的硬性要求。
struct AlignedBuf {
    ptr: *mut u8,
    layout: Layout,
}

impl AlignedBuf {
    fn new(len: usize) -> Self {
        let layout = Layout::from_size_align(len, 4096).expect("layout");
        let ptr = unsafe { alloc(layout) };
        assert!(!ptr.is_null(), "alloc failed");
        unsafe { std::ptr::write_bytes(ptr, 0x5A, len) };
        Self { ptr, layout }
    }

    fn as_slice(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr, self.layout.size()) }
    }

    fn as_mut_slice(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.layout.size()) }
    }
}

impl Drop for AlignedBuf {
    fn drop(&mut self) {
        unsafe { dealloc(self.ptr, self.layout) }
    }
}

/// 直接对某个设备路径发一次 IOCTL_DISK_PERFORMANCE（两个候选码值 × 两种访问级别都试）。
/// 返回 `(计数器, 生效码值, 访问级别)`；全失败时返回最后一个 Win32 错误码。
fn try_perf_ioctl(path: &str) -> Result<(cs_core::storage::DiskPerformance, u32, u32), u32> {
    let p = wide(path);
    // 非提权时 GENERIC_READ 打不开物理盘，但 FILE_READ_ATTRIBUTES 可以 —— 而
    // IOCTL_DISK_PERFORMANCE 是 FILE_ANY_ACCESS，这个句柄就够（实测）。
    for access in [GENERIC_READ, FILE_READ_ATTRIBUTES] {
        let h = unsafe {
            CreateFileW(
                p.as_ptr(),
                access,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                std::ptr::null_mut(),
            )
        };
        if h == INVALID_HANDLE_VALUE {
            continue;
        }
        let mut last_err = 0u32;
        for &code in IOCTL_DISK_PERFORMANCE_CODES.iter() {
            let mut out = vec![0u8; 88];
            let mut read: u32 = 0;
            let ok = unsafe {
                DeviceIoControl(
                    h,
                    code,
                    std::ptr::null(),
                    0,
                    out.as_mut_ptr().cast(),
                    out.len() as u32,
                    &mut read,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                last_err = unsafe { GetLastError() };
                continue;
            }
            if let Some(pf) = parse_disk_performance(&out) {
                unsafe { CloseHandle(h) };
                return Ok((pf, code, access));
            }
            last_err = 13;
        }
        unsafe { CloseHandle(h) };
        return Err(last_err);
    }
    Err(unsafe { GetLastError() })
}

/// 所有 NVMe 盘的设备计数器（编号, Data Units Read, Data Units Written）。
fn nvme_units() -> Vec<(u32, u64, u64)> {
    (0..32u32)
        .filter_map(|i| {
            let p = cs_core::storage::probe_drive(i);
            p.health.map(|h| {
                (
                    i,
                    h.data_units_read.min(u64::MAX as u128) as u64,
                    h.data_units_written.min(u64::MAX as u128) as u64,
                )
            })
        })
        .collect()
}

fn main() {
    println!("== 磁盘活动率/吞吐探针（IOCTL_DISK_PERFORMANCE + NVMe 设备计数器）==");
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| format!("{}\\cs-diskperf.bin", std::env::temp_dir().display()));
    println!("测试文件: {}", path);

    // ---- [0] IOCTL 在物理盘与卷上分别试一次 ----
    println!("\n[0] IOCTL_DISK_PERFORMANCE 可用性（1 = ERROR_INVALID_FUNCTION，5 = 权限不足）");
    for dev in [
        r"\\.\PhysicalDrive0",
        r"\\.\PhysicalDrive1",
        r"\\.\PhysicalDrive2",
        r"\\.\C:",
        r"\\.\A:",
    ] {
        match try_perf_ioctl(dev) {
            Ok((p, code, access)) => println!(
                "  {:<20} OK (code=0x{:05X}, access=0x{:08X}) query_time={} bytes_read={} bytes_written={}",
                dev, code, access, p.query_time, p.bytes_read, p.bytes_written
            ),
            Err(e) => println!("  {:<20} 失败 GetLastError={}", dev, e),
        }
    }

    // ---- [1] 原始计数器 ----
    println!("\n[1] 原始计数器（每条通道各自开-查-关）");
    let mut drives = Vec::new();
    for i in 0..32u32 {
        let p = cs_core::storage::probe_drive(i);
        if p.open_error != 0 {
            continue;
        }
        drives.push(i);
        let name = format!("{} {}", p.desc.vendor.trim(), p.desc.product.trim());
        match p.perf.as_ref() {
            Some(pf) => println!(
                "  [{}] {:<28} code=0x{:05X} bytes_read={:<12} bytes_written={:<12} read_time={:<10} write_time={:<10} idle={:<10} query={}",
                i, name.trim(), p.perf_code, pf.bytes_read, pf.bytes_written, pf.read_time, pf.write_time, pf.idle_time, pf.query_time
            ),
            None => println!(
                "  [{}] {:<28} perf 读失败 GetLastError={}（1 = 计数器被系统关闭，5 = 权限不足）",
                i, name.trim(), p.perf_error
            ),
        }
    }
    println!("  物理盘编号: {:?}", drives);
    let units0 = nvme_units();
    println!("  设备计数器基线: {:?}", units0);

    // ---- [2] QueryTime 走速 vs 墙钟 ----
    println!("\n[2] QueryTime 走速校验（若远小于 1 说明它是惰性快照时间戳，速率必须用墙钟做分母）");
    let mut state = PerfState::new();
    let _ = state.sample();
    let t0 = Instant::now();
    std::thread::sleep(Duration::from_millis(2000));
    let wall = t0.elapsed().as_secs_f64();
    let before: Vec<(u32, u64)> = drives
        .iter()
        .filter_map(|i| cs_core::storage::probe_drive(*i).perf.map(|p| (*i, p.query_time)))
        .collect();
    let _ = state.sample();
    let after: Vec<(u32, u64)> = drives
        .iter()
        .filter_map(|i| cs_core::storage::probe_drive(*i).perf.map(|p| (*i, p.query_time)))
        .collect();
    for (i, a) in &after {
        if let Some((_, b)) = before.iter().find(|(j, _)| j == i) {
            let delta = a.saturating_sub(*b) as f64;
            println!(
                "  [{}] ΔQueryTime={:.0} (100ns) = {:.3} s，墙钟 {:.3} s，比值 {:.4}",
                i,
                delta,
                delta / 1e7,
                wall,
                delta / 1e7 / wall
            );
        }
    }

    // ---- [3] 非缓冲写入 ----
    println!("\n[3] 非缓冲写入 {} MiB", FILE_MIB);
    let wpath = wide(&path);
    let h = unsafe {
        CreateFileW(
            wpath.as_ptr(),
            GENERIC_WRITE,
            0,
            std::ptr::null(),
            CREATE_ALWAYS,
            FILE_ATTRIBUTE_NORMAL | FILE_FLAG_NO_BUFFERING | FILE_FLAG_WRITE_THROUGH,
            std::ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE {
        println!("  创建文件失败 GetLastError={}（磁盘空间不足？）", unsafe {
            GetLastError()
        });
        return;
    }
    let mut buf = AlignedBuf::new(CHUNK);
    // 这一轮采样只为给 PerfState 建立对照快照（速率由下一轮差分出来）
    let _pre_write = state.sample();
    let mut written: u64 = 0;
    let t0 = Instant::now();
    let mut io_err = 0u32;
    for _ in 0..(FILE_MIB * MIB / CHUNK) {
        let mut n: u32 = 0;
        let ok = unsafe {
            WriteFile(
                h,
                buf.as_slice().as_ptr(),
                CHUNK as u32,
                &mut n,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            io_err = unsafe { GetLastError() };
            break;
        }
        written += n as u64;
    }
    unsafe { CloseHandle(h) };
    let write_elapsed = t0.elapsed().as_secs_f64();
    let post_write = state.sample();
    let units1 = nvme_units();
    println!(
        "  WriteFile: 写了 {} MiB，用时 {:.3} s，文件吞吐 {:.1} MiB/s，err={}",
        written / MIB as u64,
        write_elapsed,
        written as f64 / MIB as f64 / write_elapsed,
        io_err
    );
    report(&post_write, write_elapsed, "写");
    units_report(&units0, &units1, write_elapsed, "写", written);

    // ---- [4] 非缓冲读取 ----
    println!("\n[4] 非缓冲读取");
    let rpath = wide(&path);
    let h = unsafe {
        CreateFileW(
            rpath.as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL | FILE_FLAG_NO_BUFFERING,
            std::ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE {
        println!("  打开文件失败 GetLastError={}", unsafe { GetLastError() });
        return;
    }
    // 同上：给读阶段的速率差分建对照快照
    let _pre_read = state.sample();
    let mut read: u64 = 0;
    let t0 = Instant::now();
    loop {
        let mut n: u32 = 0;
        let ok = unsafe {
            ReadFile(
                h,
                buf.as_mut_slice().as_mut_ptr(),
                CHUNK as u32,
                &mut n,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 || n == 0 {
            io_err = unsafe { GetLastError() };
            break;
        }
        read += n as u64;
    }
    unsafe { CloseHandle(h) };
    let read_elapsed = t0.elapsed().as_secs_f64();
    let post_read = state.sample();
    let units2 = nvme_units();
    println!(
        "  ReadFile: 读了 {} MiB，用时 {:.3} s，文件吞吐 {:.1} MiB/s，err={}",
        read / MIB as u64,
        read_elapsed,
        read as f64 / MIB as f64 / read_elapsed,
        io_err
    );
    report(&post_read, read_elapsed, "读");
    units_report(&units1, &units2, read_elapsed, "读", read);

    // ---- [5] 全程设备计数器 ----
    println!("\n[5] 全程设备计数器（写+读合计 {} MiB 写 / {} MiB 读）", written / MIB as u64, read / MIB as u64);
    for (i, r0, w0) in &units0 {
        if let Some((_, r2, w2)) = units2.iter().find(|(j, _, _)| j == i) {
            let dw = w2.saturating_sub(*w0);
            let dr = r2.saturating_sub(*r0);
            let dt = write_elapsed + read_elapsed;
            println!(
                "  [{}] Δ写={} 单元（{:.1} MiB）  Δ读={} 单元（{:.1} MiB）  区间平均 {:.1}/{:.1} MiB/s",
                i,
                dw,
                dw as f64 * 512000.0 / MIB as f64,
                dr,
                dr as f64 * 512000.0 / MIB as f64,
                units_rate_mib_s(dr, dt),
                units_rate_mib_s(dw, dt)
            );
        }
    }

    // ---- [6] 清理 ----
    match std::fs::remove_file(&path) {
        Ok(()) => println!("\n[6] 已删除测试文件"),
        Err(e) => println!("\n[6] 删除测试文件失败: {}", e),
    }
}

/// 把 IOCTL 通路的采样结果与文件实际吞吐对拍（只打印参与了这次 IO 的盘）。
fn report(post: &[cs_core::storage::PerfSample], file_elapsed: f64, label: &str) {
    let mut any = false;
    for p in post {
        let Some(r) = p.rates else { continue };
        if r.read_pct.max(r.write_pct) < 0.5 && r.read_mib_s.max(r.write_mib_s) < 0.5 {
            continue;
        }
        any = true;
        println!(
            "  [{}] {}活动率 R{:.1}% W{:.1}% T{:.1}%  吞吐 R{:.1} / W{:.1} MiB/s   (文件用时 {:.3} s)",
            p.index, label, r.read_pct, r.write_pct, r.total_pct, r.read_mib_s, r.write_mib_s, file_elapsed
        );
    }
    if !any {
        println!("  （IOCTL 通路无数据：计数器可能被 Windows 关闭）");
    }
}

/// 设备计数器在某个阶段的增量与文件 I/O 对拍：1 单元 = 512000 B。
fn units_report(pre: &[(u32, u64, u64)], post: &[(u32, u64, u64)], elapsed: f64, label: &str, file_bytes: u64) {
    let expected_mib = file_bytes as f64 / MIB as f64;
    for (i, r0, w0) in pre {
        let Some((_, r1, w1)) = post.iter().find(|(j, _, _)| j == i) else {
            continue;
        };
        let (dr, dw) = (r1.saturating_sub(*r0), w1.saturating_sub(*w0));
        let (rd_mib, wr_mib) = (
            dr as f64 * 512000.0 / MIB as f64,
            dw as f64 * 512000.0 / MIB as f64,
        );
        println!(
            "  [{}] {}阶段设备计数器: Δ读 {:.1} MiB（{:.1} MiB/s）  Δ写 {:.1} MiB（{:.1} MiB/s）  文件 {:.1} MiB",
            i,
            label,
            rd_mib,
            units_rate_mib_s(dr, elapsed),
            wr_mib,
            units_rate_mib_s(dw, elapsed),
            expected_mib
        );
    }
}

// merge_perf 也在此处被引用，避免 unused import 警告（探针不构造 Metrics）
#[allow(dead_code)]
fn _touch(m: &mut [cs_core::schema::StorageMetrics], s: &[cs_core::storage::PerfSample]) {
    merge_perf(m, s);
}
