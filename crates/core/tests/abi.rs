//! C ABI 契约测试：真实加载编译出的 cdylib（cs_core.dll），
//! 验证导出符号、调用约定、字符串所有权与 JSON schema。
//!
//! 展示层（web 壳/GUI）与 DLL 的集成方式完全同构——这份测试就是用法示例。

use std::ffi::c_char;
use std::ffi::CStr;

use libloading::Library;

fn dll_path() -> std::path::PathBuf {
    let profile = if cfg!(debug_assertions) { "debug" } else { "release" };
    let ext = if cfg!(windows) { "dll" } else { "so" };
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("target")
        .join(profile)
        .join(format!("cs_core.{ext}"))
        .canonicalize()
        .expect("cdylib 存在（cargo test 会先构建 lib 目标）")
}

#[test]
fn c_abi_roundtrip() {
    unsafe {
        let lib = Library::new(dll_path()).expect("加载 cs_core cdylib");

        let init: libloading::Symbol<unsafe extern "C" fn() -> i32> =
            lib.get(b"cs_init").expect("符号 cs_init");
        let version: libloading::Symbol<unsafe extern "C" fn() -> *const c_char> =
            lib.get(b"cs_version").expect("符号 cs_version");
        let info_json: libloading::Symbol<unsafe extern "C" fn() -> *mut c_char> =
            lib.get(b"cs_get_info_json").expect("符号 cs_get_info_json");
        let metrics_json: libloading::Symbol<unsafe extern "C" fn() -> *mut c_char> =
            lib.get(b"cs_get_metrics_json").expect("符号 cs_get_metrics_json");
        let free_string: libloading::Symbol<unsafe extern "C" fn(*mut c_char)> =
            lib.get(b"cs_free_string").expect("符号 cs_free_string");
        let shutdown: libloading::Symbol<unsafe extern "C" fn()> =
            lib.get(b"cs_shutdown").expect("符号 cs_shutdown");

        // init 幂等
        assert_eq!(init(), 0);
        assert_eq!(init(), 0);

        // version 是静态字符串
        let v = CStr::from_ptr(version()).to_string_lossy().into_owned();
        assert_eq!(v, env!("CARGO_PKG_VERSION"));

        let free_fn = *free_string;

        // info JSON
        let p = info_json();
        assert!(!p.is_null(), "init 后 info 不应为 NULL");
        let s = take_string(p, free_fn);
        let info: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert!(info["cpu_name"].as_str().unwrap().len() > 0);
        assert!(info["logical_cores"].as_u64().unwrap() > 0);
        assert!(info["total_memory_gb"].as_f64().unwrap() > 0.0);
        assert!(info["platform"].as_str().is_some());
        for k in ["pawnio", "nvml", "nvidia_smi", "wmi"] {
            assert!(info["sources"][k].as_bool().is_some(), "info.sources.{k}");
        }

        // metrics JSON（首帧最多 2s）
        let p = metrics_json();
        assert!(!p.is_null());
        let s = take_string(p, free_fn);
        let m: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert!(m["ts_ms"].as_u64().unwrap() > 0);
        assert!(m["cpu"]["usage_pct"].as_f64().is_some());
        assert!(m["cpu"]["per_core_pct"].as_array().is_some());
        assert!(m["memory"]["total_gb"].as_f64().unwrap() > 0.0);
        if m["gpu"].is_object() {
            assert!(m["gpu"]["name"].as_str().unwrap().len() > 0);
        }

        // 双帧推进：时间戳应增长
        std::thread::sleep(std::time::Duration::from_millis(600));
        let p = metrics_json();
        let s2 = take_string(p, free_fn);
        let m2: serde_json::Value = serde_json::from_str(&s2).unwrap();
        assert!(m2["ts_ms"].as_u64().unwrap() > m["ts_ms"].as_u64().unwrap());

        // shutdown 后不崩、metrics 仍可调（快照冻结语义）
        shutdown();
        shutdown(); // 幂等
        let p = metrics_json();
        if !p.is_null() {
            take_string(p, free_fn);
        }

        drop(lib);
    }
}

/// 取走 C 字符串并立即释放（调用方所有权）。
unsafe fn take_string(p: *mut c_char, free: unsafe extern "C" fn(*mut c_char)) -> String {
    let s = CStr::from_ptr(p).to_string_lossy().into_owned();
    free(p);
    s
}
