//! WMI ACPI 热区探针：只测手搓 COM/WMI 链路。
//! `cargo run -p cs-core --example wmi_probe --release`
fn main() {
    println!("[1] CoInitializeEx + ConnectServer + ExecQuery 全链路 ...");
    for i in 0..3 {
        let t0 = std::time::Instant::now();
        match cs_core::wmi_temp::thermal_zone_max_temp_c() {
            Some(c) => println!("    [{}] {}°C ({}ms)", i, c, t0.elapsed().as_millis()),
            None => println!("    [{}] None ({}ms) — 本机无 ACPI 热区或 COM 失败", i, t0.elapsed().as_millis()),
        }
    }
    println!("WMI 无崩溃。");
}
