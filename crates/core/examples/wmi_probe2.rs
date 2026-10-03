//! WMI 分步打点探针：复刻 wmi_temp.rs 的链路，每步 eprintln（stderr 无缓冲）。
//! `cargo run -p cs-core --example wmi_probe2 --release`
use std::ffi::c_void;
use windows_sys::core::GUID;
use windows_sys::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
};

const CLSID_WBEM_LOCATOR: GUID = GUID {
    data1: 0x4590_F811,
    data2: 0x1D3A,
    data3: 0x11D0,
    data4: [0x89, 0x1F, 0x00, 0xAA, 0x00, 0x4B, 0x2E, 0x24],
};
const IID_IWBEM_LOCATOR: GUID = GUID {
    data1: 0xDC12_A687,
    data2: 0x737F,
    data3: 0x11CF,
    data4: [0x88, 0x4D, 0x00, 0xAA, 0x00, 0x4B, 0x2E, 0x24],
};

type ReleaseFn = unsafe extern "system" fn(*mut c_void) -> u32;
type ConnectServerFn = unsafe extern "system" fn(
    *mut c_void, *const u16, *const u16, *const u16, *const u16, i32, *const u16, *const c_void,
    *mut *mut c_void,
) -> i32;

#[inline]
unsafe fn vslot(p: *mut c_void, i: usize) -> *mut c_void {
    let vtable: *mut c_void = *(p as *mut *mut c_void);
    let slots: *mut *mut c_void = vtable as *mut *mut c_void;
    *slots.add(i)
}

struct Bstr {
    _buf: Vec<u64>,
    ptr: *mut u16,
}

impl Bstr {
    fn new(s: &str) -> Bstr {
        let chars: Vec<u16> = s.encode_utf16().collect();
        let n_u64 = (4 + chars.len() * 2 + 2).div_ceil(8);
        let mut buf = vec![0u64; n_u64];
        let bytes = unsafe {
            std::slice::from_raw_parts_mut(buf.as_mut_ptr().cast::<u8>(), buf.len() * 8)
        };
        bytes[0..4].copy_from_slice(&((chars.len() as u32 * 2).to_le_bytes()));
        for (i, c) in chars.iter().enumerate() {
            bytes[4 + i * 2..4 + i * 2 + 2].copy_from_slice(&c.to_le_bytes());
        }
        let ptr = unsafe { (buf.as_mut_ptr() as *mut u8).add(4) as *mut u16 };
        Bstr { _buf: buf, ptr }
    }
}

fn main() {
    unsafe {
        eprintln!("A: CoInitializeEx ...");
        let hr = CoInitializeEx(std::ptr::null(), COINIT_MULTITHREADED as u32);
        eprintln!("A: hr=0x{:X}", hr);

        eprintln!("B: CoCreateInstance ...");
        let mut locator: *mut c_void = std::ptr::null_mut();
        let hr = CoCreateInstance(
            &CLSID_WBEM_LOCATOR,
            std::ptr::null_mut(),
            CLSCTX_INPROC_SERVER,
            &IID_IWBEM_LOCATOR,
            &mut locator,
        );
        eprintln!("B: hr=0x{:X} locator={:p}", hr, locator);
        if hr != 0 {
            eprintln!("B: 失败，退出");
            return;
        }

        eprintln!("C: 读 vtable 槽 3 (ConnectServer) ...");
        let vt = *(locator as *mut *mut c_void);
        eprintln!("C: vtable={:p} slot3={:p}", vt, vslot(locator, 3));
        let connect: ConnectServerFn = std::mem::transmute(vslot(locator, 3));

        eprintln!("D: 构造 BSTR + ConnectServer 调用 ...");
        let ns = Bstr::new(r"root\wmi");
        eprintln!("D: ns.ptr={:p} (len 前缀已写)", ns.ptr);
        let mut services: *mut c_void = std::ptr::null_mut();
        let hr = connect(
            locator,
            ns.ptr,
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            std::ptr::null(),
            std::ptr::null(),
            &mut services,
        );
        eprintln!("D: hr=0x{:X} services={:p}", hr, services);

        if hr == 0 {
            eprintln!("E: Release(services) ...");
            let rel: ReleaseFn = std::mem::transmute(vslot(services, 2));
            rel(services);
        }
        eprintln!("F: Release(locator) ...");
        let rel: ReleaseFn = std::mem::transmute(vslot(locator, 2));
        rel(locator);

        eprintln!("G: CoUninitialize ...");
        CoUninitialize();
        eprintln!("全部完成");
    }
}
