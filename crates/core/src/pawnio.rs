//! PawnIO 驱动客户端。
//!
//! 协议参照 LibreHardwareMonitor 的 PawnIo.cs（MPL-2.0）：
//! - 设备路径 `\\?\GLOBALROOT\Device\PawnIO`
//! - IOCTL 加载签名 pawn 字节码模块（我们用 LHM 官方签发的 AMDFamily17.bin）
//! - IOCTL 执行模块内函数：输入 = [32 字节函数名][若干 i64 参数]，输出 = i64 数组
//!
//! PawnIO 是正规签名驱动，不在微软易受攻击驱动黑名单，HVCI 开启的机器可用。
//! 另外按 LHM 的 Mutexes.cs 约定，PCI/SMN 访问前后持有 `Global\Access_PCI`
//! 全局互斥锁，与同时运行的 LHM/FanControl 等工具串行化，避免互相踩踏。

#![cfg(windows)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};

use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::CreateFileW;
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Threading::{
    CreateMutexW, ReleaseMutex, WaitForSingleObject,
};

const DEVICE_PATH: &str = r"\\?\GLOBALROOT\Device\PawnIO";
const DEVICE_TYPE: u32 = 41394 << 16;
const IOCTL_PIO_LOAD_BINARY: u32 = DEVICE_TYPE | (0x821 << 2);
const IOCTL_PIO_EXECUTE_FN: u32 = DEVICE_TYPE | (0x841 << 2);
const FN_NAME_LENGTH: usize = 32;

// GENERIC_READ | GENERIC_WRITE | FILE_SHARE_READ | FILE_SHARE_WRITE | OPEN_EXISTING
const GENERIC_READ_WRITE: u32 = 0x8000_0000 | 0x4000_0000;
const FILE_SHARE_RW: u32 = 0x1 | 0x2;
const OPEN_EXISTING: u32 = 3;

const PCI_MUTEX_NAME: &str = r"Global\Access_PCI";
/// `Mutexes.cs` —— LHM 用这把锁把 ISA/LPC 总线访问串行化，与它共存时同样要遵守
const ISA_MUTEX_NAME: &str = r"Global\Access_ISABUS.HTP.Method";

const ERROR_ACCESS_DENIED: u32 = 5;

/// 最近一次 open() 是否因权限被拒（ERROR_ACCESS_DENIED）。
/// 非提权进程打开 PawnIO 设备就是被拒 —— 给上层一个可诊断的信号。
static ACCESS_DENIED: AtomicBool = AtomicBool::new(false);

/// true = 本进程打开 PawnIO 设备时被拒（通常需要以管理员运行）。
pub fn access_denied() -> bool {
    ACCESS_DENIED.load(Ordering::Relaxed)
}

pub struct PawnIo {
    handle: HANDLE,
}

// HANDLE 是进程级句柄值，跨线程传递/使用安全（驱动侧对 IOCTL 串行化）。
// PawnIo 只随 Sampler 移动到采样线程，之后单线程独占使用。
unsafe impl Send for PawnIo {}
unsafe impl Sync for PawnIo {}

impl PawnIo {
    /// 打开 PawnIO 设备并加载字节码模块。失败返回 None（驱动未装/无权限）。
    pub fn open(module: &[u8]) -> Option<PawnIo> {
        let path: Vec<u16> = DEVICE_PATH.encode_utf16().chain(std::iter::once(0)).collect();
        unsafe {
            let handle = CreateFileW(
                path.as_ptr(),
                GENERIC_READ_WRITE,
                FILE_SHARE_RW,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            );
            if handle == INVALID_HANDLE_VALUE {
                ACCESS_DENIED.store(
                    GetLastError() == ERROR_ACCESS_DENIED,
                    Ordering::Relaxed,
                );
                return None;
            }
            let ok = DeviceIoControl(
                handle,
                IOCTL_PIO_LOAD_BINARY,
                module.as_ptr().cast::<c_void>(),
                module.len() as u32,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );
            if ok == 0 {
                CloseHandle(handle);
                return None;
            }
            Some(PawnIo { handle })
        }
    }

    /// 执行模块内函数。输入 i64 数组，输出 i64 数组；失败返回 None。
    pub fn execute(&self, name: &str, input: &[i64], out_len: usize) -> Option<Vec<i64>> {
        let mut total = vec![0u8; FN_NAME_LENGTH + input.len() * 8];
        let name_bytes = name.as_bytes();
        let n = name_bytes.len().min(FN_NAME_LENGTH - 1);
        total[..n].copy_from_slice(&name_bytes[..n]);
        for (i, v) in input.iter().enumerate() {
            total[FN_NAME_LENGTH + i * 8..FN_NAME_LENGTH + i * 8 + 8]
                .copy_from_slice(&v.to_le_bytes());
        }
        let mut out = vec![0u8; out_len * 8];
        let mut read: u32 = 0;
        unsafe {
            let ok = DeviceIoControl(
                self.handle,
                IOCTL_PIO_EXECUTE_FN,
                total.as_ptr().cast::<c_void>(),
                total.len() as u32,
                out.as_mut_ptr().cast::<c_void>(),
                out.len() as u32,
                &mut read,
                std::ptr::null_mut(),
            );
            if ok == 0 {
                return None;
            }
        }
        let n = (read as usize) / 8;
        Some(
            (0..n)
                .map(|i| i64::from_le_bytes(out[i * 8..i * 8 + 8].try_into().unwrap()))
                .collect(),
        )
    }

    /// 读 SMN（System Management Network）寄存器 —— AMD 温度/电压的入口。
    pub fn read_smn(&self, addr: u32) -> Option<u32> {
        self.execute("ioctl_read_smn", &[addr as i64], 1)
            .map(|v| v[0] as u32)
    }

    /// 读 MSR，返回 (edx:eax) 64 位值。注意：在调用线程当前所在核心上执行。
    pub fn read_msr(&self, index: u32) -> Option<u64> {
        self.execute("ioctl_read_msr", &[index as i64], 1)
            .map(|v| v[0] as u64)
    }
}

impl Drop for PawnIo {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.handle) };
    }
}

/// `Global\Access_PCI` 互斥锁守卫：与 LHM/FanControl 等共享工具串行化 SMN/PCI 访问。
/// 拿不到锁（超时/权限）时仍返回 None 策略由调用方决定 —— 这里 wait 失败返回 None，
/// 调用方跳过本轮读取，宁缺毋脏。
pub struct PciBusGuard {
    mutex: HANDLE,
}

impl PciBusGuard {
    pub fn wait(timeout_ms: u32) -> Option<PciBusGuard> {
        Some(PciBusGuard {
            mutex: wait_named_mutex(PCI_MUTEX_NAME, timeout_ms)?,
        })
    }
}

impl Drop for PciBusGuard {
    fn drop(&mut self) {
        unsafe {
            ReleaseMutex(self.mutex);
            CloseHandle(self.mutex);
        }
    }
}

/// `Global\Access_ISABUS.HTP.Method` 互斥锁守卫（LHM `Mutexes.WaitIsaBus`）：
/// SuperIO / LPC 端口访问与 LHM、FanControl 串行化。同样遵循「拿不到就跳过本轮」。
pub struct IsaBusGuard {
    mutex: HANDLE,
}

impl IsaBusGuard {
    pub fn wait(timeout_ms: u32) -> Option<IsaBusGuard> {
        Some(IsaBusGuard {
            mutex: wait_named_mutex(ISA_MUTEX_NAME, timeout_ms)?,
        })
    }
}

impl Drop for IsaBusGuard {
    fn drop(&mut self) {
        unsafe {
            ReleaseMutex(self.mutex);
            CloseHandle(self.mutex);
        }
    }
}

/// 打开（或创建）一个命名互斥锁并等待它；超时或权限失败返回 None。
fn wait_named_mutex(name: &str, timeout_ms: u32) -> Option<HANDLE> {
    let name: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let mutex = CreateMutexW(std::ptr::null(), 0, name.as_ptr());
        if mutex.is_null() || mutex == INVALID_HANDLE_VALUE {
            return None;
        }
        // 0 = WAIT_OBJECT_0, 0x80 = WAIT_ABANDONED（持有者崩溃，锁仍归我们）
        let r = WaitForSingleObject(mutex, timeout_ms);
        if r != 0 && r != 0x80 {
            CloseHandle(mutex);
            return None;
        }
        Some(mutex)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_device_load_module() {
        // 机器上 PawnIO 驱动在跑时才应成功；本机已验证 v2.1.0 RUNNING。
        let bin = include_bytes!("../../../drivers/pawnio/AMDFamily17.bin");
        let p = PawnIo::open(bin);
        if let Some(p) = p {
            let v = p.read_smn(0x00059800);
            assert!(v.is_some(), "read_smn THM_TCON_CUR_TMP 应有返回");
        }
        // 驱动不在的机器上允许 None，不算失败 —— 所以无 else 断言。
    }
}
