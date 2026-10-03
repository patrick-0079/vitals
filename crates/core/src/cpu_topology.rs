//! 逻辑处理器拓扑与线程亲和性 —— 每核 MSR 的前置条件。
//!
//! AMD 的 APERF/MPERF、CORE_ENERGY_STAT、HW_PSTATE_STATUS 都是**每核**寄存器：
//! `rdmsr` 读到的是**调用线程当时所在核**的值。所以读之前必须把线程钉到目标核上，
//! 否则读到的是调度器随手分配的某个核（LHM 在 `Amd17Cpu.cs:164` / `:675` 用
//! `ThreadAffinity.Set` 做的同一件事）。
//!
//! 「物理核 → 逻辑处理器」的对应关系由固件决定，不能假设 i / i+8：
//! 本机（9850X3D，8 核 16 线程）实测是 (0,1) (2,3) … (14,15)，SMT 兄弟相邻。
#![cfg(windows)]

use windows_sys::Win32::System::SystemInformation::{
    GetLogicalProcessorInformationEx, RelationProcessorCore, GROUP_AFFINITY,
    SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX,
};
use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadAffinityMask};

/// 「物理核 → 该核的逻辑处理器编号列表」。取不到时返回空表。
pub fn core_groups() -> Vec<Vec<u32>> {
    let mut len: u32 = 0;
    // 第一次调用只为拿长度，必然返回 0 + ERROR_INSUFFICIENT_BUFFER
    unsafe {
        GetLogicalProcessorInformationEx(RelationProcessorCore, std::ptr::null_mut(), &mut len);
    }
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

    let mut groups: Vec<Vec<u32>> = Vec::new();
    let mut off = 0usize;
    while off + 8 <= len as usize {
        let ent =
            unsafe { &*(buf.as_ptr().add(off) as *const SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX) };
        if ent.Size == 0 {
            break;
        }
        if ent.Relationship == RelationProcessorCore {
            let proc = unsafe { &ent.Anonymous.Processor };
            // GroupMask 声明长度是 1，GroupCount > 1 时得用指针算术取后续项。
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

/// 线程亲和性守卫：`pin` 时钉到指定逻辑处理器，drop 时恢复原掩码。
pub struct Affinity {
    previous: usize,
}

impl Affinity {
    /// 把当前线程钉到 `logical` 号逻辑处理器。
    /// 只支持第 0 组（`logical < 64`）——`SetThreadAffinityMask` 本身就表达不了跨组掩码，
    /// 超出范围返回 None（本机 16 个逻辑核全在第 0 组）。
    pub fn pin(logical: u32) -> Option<Affinity> {
        if logical >= 64 {
            return None;
        }
        let previous = unsafe { SetThreadAffinityMask(GetCurrentThread(), 1usize << logical) };
        if previous == 0 {
            None
        } else {
            Some(Affinity { previous })
        }
    }
}

impl Drop for Affinity {
    fn drop(&mut self) {
        unsafe { SetThreadAffinityMask(GetCurrentThread(), self.previous) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_groups_are_disjoint_and_cover_threads() {
        let groups = core_groups();
        if groups.is_empty() {
            return; // 极端受限环境
        }
        let mut all: Vec<u32> = groups.iter().flatten().copied().collect();
        let total = all.len();
        all.sort_unstable();
        all.dedup();
        assert_eq!(all.len(), total, "同一个逻辑处理器出现在多个核分组里");
        // 本机 16 逻辑核 → 8 个物理核；至少每个核 1 个线程
        assert!(groups.iter().all(|g| !g.is_empty()));
        assert!(groups.len() <= total);
    }

    #[test]
    fn pin_and_restore() {
        let groups = core_groups();
        if groups.is_empty() {
            return;
        }
        let target = groups[groups.len() - 1][0];
        {
            let _g = Affinity::pin(target).expect("pin 失败");
        } // drop 恢复
        // 越界不 panic，返回 None
        assert!(Affinity::pin(9999).is_none());
    }
}
