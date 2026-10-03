//! NVML 探针：只测 GPU 链路。`cargo run -p cs-core --example gpu_probe --release`
fn main() {
    println!("[1] NvmlGpu::open() ...");
    let g = match cs_core::gpu::NvmlGpu::open() {
        Some(g) => g,
        None => {
            println!("    NVML 打开失败（没有 nvml.dll / init 失败）");
            return;
        }
    };
    println!("    OK, name = {:?}", g.name());

    println!("[2] poll() × 3 ...");
    for i in 0..3 {
        let m = g.poll();
        println!(
            "    [{}] usage={}%, mem={} MB / {} MB ({}%), temp={:?}°C, power={:?} W, fan={:?}%",
            i, m.usage_pct, m.vram_used_mb, m.vram_total_mb, m.vram_usage_pct,
            m.temp_c, m.power_w, m.fan_pct
        );
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    println!("NVML 无崩溃。");

    println!("[3] nvidia_smi 降级链路 ...");
    match cs_core::gpu::nvidia_smi_poll() {
        Some(m) => println!("    OK: {:?}", m),
        None => println!("    失败"),
    }
}
