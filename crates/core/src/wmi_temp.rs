//! WMI ACPI 热区温度兜底通路（免装任何软件，但很多台式机不暴露）。
//! 查询 root\wmi 命名空间的 MSAcpi_ThermalZoneTemperature.CurrentTemperature
//! （单位：0.1 开尔文）。每次独立连接、查完即释放，无跨线程 COM 状态。
//!
//! 手写 COM vtable 调用：windows-sys 0.59 已不含经典 WMI COM 接口
//! （只有 MI_* 平铺 API），接口定义与其槽位按 windows crate 0.62 的
//! 元数据核对：
//!   IWbemLocator::ConnectServer  = vtable 槽 3
//!   IWbemServices::ExecQuery     = 槽 20
//!   IEnumWbemClassObject::Next   = 槽 4
//!   IWbemClassObject::Get        = 槽 4
//!   IUnknown::Release            = 槽 2

#![cfg(windows)]

use std::ffi::c_void;

use windows_sys::core::{GUID, HRESULT};
use windows_sys::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
};

// {4590F811-1D3A-11D0-891F-00AA004B2E24}
const CLSID_WBEM_LOCATOR: GUID = GUID {
    data1: 0x4590_F811,
    data2: 0x1D3A,
    data3: 0x11D0,
    data4: [0x89, 0x1F, 0x00, 0xAA, 0x00, 0x4B, 0x2E, 0x24],
};
// {DC12A687-737F-11CF-884D-00AA004B2E24}
const IID_IWBEM_LOCATOR: GUID = GUID {
    data1: 0xDC12_A687,
    data2: 0x737F,
    data3: 0x11CF,
    data4: [0x88, 0x4D, 0x00, 0xAA, 0x00, 0x4B, 0x2E, 0x24],
};

const WBEM_FLAG_FORWARD_ONLY: i32 = 0x20;
const VT_I4: u16 = 3;
const S_OK: HRESULT = 0;
const S_FALSE: HRESULT = 1;

// COM 调用约定：x64 上所有 WMI 接口方法都是 stdcall（Rust: extern "system"）
type ReleaseFn = unsafe extern "system" fn(*mut c_void) -> u32;
type ConnectServerFn = unsafe extern "system" fn(
    *mut c_void,          // this
    *const u16,           // strNetworkResource (BSTR)
    *const u16,           // strUser
    *const u16,           // strPassword
    *const u16,           // strLocale
    i32,                  // lSecurityFlags
    *const u16,           // strAuthority
    *const c_void,        // pCtx (IWbemContext)
    *mut *mut c_void,     // ppServices
) -> HRESULT;
type ExecQueryFn = unsafe extern "system" fn(
    *mut c_void,      // this
    *const u16,       // strQueryLanguage (BSTR, "WQL")
    *const u16,       // strQuery (BSTR)
    i32,              // lFlags
    *const c_void,    // pCtx
    *mut *mut c_void, // ppEnum
) -> HRESULT;
type EnumNextFn = unsafe extern "system" fn(
    *mut c_void,      // this
    i32,              // lTimeout (ms)
    u32,              // uCount
    *mut *mut c_void, // apObjects
    *mut u32,         // puReturned
) -> HRESULT;
type ObjGetFn = unsafe extern "system" fn(
    *mut c_void, // this
    *const u16,  // wszName (BSTR)
    i32,         // lFlags
    *mut c_void, // pVal (VARIANT*)
    *mut i32,    // pType
    *mut i32,    // plFlavor
) -> HRESULT;

/// 取接口 vtable 第 i 槽的函数指针。
///
/// 注意类型链：对象首字段是 vtable 指针（`*mut *mut c_void` 读出），
/// vtable 的元素是**函数指针**（8 字节），所以 `.add(i)` 必须作用在
/// `*mut *mut c_void` 上——若作用在 `*mut c_void` 上，偏移按
/// size_of::<c_void>()（≠8）计算，会读到错位垃圾指针导致访问违例。
#[inline]
unsafe fn vslot(p: *mut c_void, i: usize) -> *mut c_void {
    // 对象首字段 = vtable 指针
    let vtable: *mut c_void = *(p as *mut *mut c_void);
    // vtable 视为函数指针数组（元素 8 字节），.add(i) 才是 i×8 偏移
    let slots: *mut *mut c_void = vtable as *mut *mut c_void;
    *slots.add(i)
}

#[inline]
unsafe fn release(p: *mut c_void) {
    if !p.is_null() {
        let f: ReleaseFn = std::mem::transmute(vslot(p, 2));
        f(p);
    }
}

/// 手工构造 BSTR（长度前缀 + UTF-16 + 终止符），用 u64 缓冲保证对齐。
struct Bstr {
    _buf: Vec<u64>,
    ptr: *mut u16,
}

impl Bstr {
    fn new(s: &str) -> Bstr {
        let chars: Vec<u16> = s.encode_utf16().collect();
        let n_u64 = (4 + chars.len() * 2 + 2).div_ceil(8);
        let mut buf = vec![0u64; n_u64];
        let bytes =
            unsafe { std::slice::from_raw_parts_mut(buf.as_mut_ptr().cast::<u8>(), buf.len() * 8) };
        bytes[0..4].copy_from_slice(&((chars.len() as u32 * 2).to_le_bytes()));
        for (i, c) in chars.iter().enumerate() {
            bytes[4 + i * 2..4 + i * 2 + 2].copy_from_slice(&c.to_le_bytes());
        }
        let ptr = unsafe { (buf.as_mut_ptr() as *mut u8).add(4) as *mut u16 };
        Bstr { _buf: buf, ptr }
    }
}

/// 查询所有 ACPI 热区，返回其中的最高温度（℃）。
/// 过滤 -20..150 之外的离谱值（固定假数据常见 27.85 等，宁可信其无）。
pub fn thermal_zone_max_temp_c() -> Option<f32> {
    unsafe {
        let hr = CoInitializeEx(std::ptr::null(), COINIT_MULTITHREADED as u32);
        if hr != S_OK && hr != S_FALSE {
            return None; // COM 不可用（如线程模型冲突）
        }
        let result = query_thermal_zones();
        if hr == S_OK || hr == S_FALSE {
            CoUninitialize();
        }
        result
    }
}

unsafe fn query_thermal_zones() -> Option<f32> {
    let mut locator: *mut c_void = std::ptr::null_mut();
    let hr = CoCreateInstance(
        &CLSID_WBEM_LOCATOR,
        std::ptr::null_mut(),
        CLSCTX_INPROC_SERVER,
        &IID_IWBEM_LOCATOR,
        &mut locator,
    );
    if hr != S_OK {
        return None;
    }

    let ns = Bstr::new(r"root\wmi");
    let wql = Bstr::new("WQL");
    let query = Bstr::new("SELECT * FROM MSAcpi_ThermalZoneTemperature");

    // IWbemLocator::ConnectServer（槽 3）
    let mut services: *mut c_void = std::ptr::null_mut();
    let connect: ConnectServerFn = std::mem::transmute(vslot(locator, 3));
    let hr = connect(
        locator,
        ns.ptr,
        std::ptr::null(),        // user = 当前用户
        std::ptr::null(),        // password
        std::ptr::null(),        // locale = 当前
        0,                       // security flags
        std::ptr::null(),        // authority
        std::ptr::null(),        // context
        &mut services,
    );
    if hr != S_OK {
        release(locator);
        return None;
    }

    // IWbemServices::ExecQuery（槽 20）
    let mut enumerator: *mut c_void = std::ptr::null_mut();
    let exec: ExecQueryFn = std::mem::transmute(vslot(services, 20));
    let hr = exec(
        services,
        wql.ptr,
        query.ptr,
        WBEM_FLAG_FORWARD_ONLY,
        std::ptr::null(),
        &mut enumerator,
    );
    if hr != S_OK {
        release(services);
        release(locator);
        return None;
    }

    // IEnumWbemClassObject::Next（槽 4）— 逐条取实例
    let next: EnumNextFn = std::mem::transmute(vslot(enumerator, 4));
    let name = Bstr::new("CurrentTemperature");
    let mut best: Option<f32> = None;
    loop {
        let mut obj: *mut c_void = std::ptr::null_mut();
        let mut got: u32 = 0;
        let hr = next(enumerator, 2000, 1, &mut obj, &mut got);
        if hr != S_OK || got == 0 {
            break;
        }
        // IWbemClassObject::Get（槽 4）— 每个对象取一次
        let get: ObjGetFn = std::mem::transmute(vslot(obj, 4));
        // VARIANT 在 x64 上 24 字节，用 32 字节缓冲规避布局细节。
        let mut var: [u64; 4] = [0; 4];
        let mut ptype: i32 = 0;
        let mut flavor: i32 = 0;
        let hr = get(
            obj,
            name.ptr,
            0,
            var.as_mut_ptr() as *mut c_void,
            &mut ptype,
            &mut flavor,
        );
        if hr == S_OK {
            let vt = var[0] as u16; // VARTYPE 在偏移 0
            if vt == VT_I4 {
                // union 起始于偏移 8（x64 对齐）
                let val = ((var[1] & 0xFFFF_FFFF) as u32) as i32;
                let c = val as f32 / 10.0 - 273.15;
                if (-20.0..150.0).contains(&c) {
                    best = Some(best.map_or(c, |b: f32| b.max(c)));
                }
            }
        }
        release(obj);
    }

    release(enumerator);
    release(services);
    release(locator);
    best
}
