//! GPU 链路探针：NVML 与 nvidia-smi 两路并排取样 + 字段级对拍。
//! `cargo run -p cs-core --example gpu_probe --release`
//!
//! 用途：
//! 1. 确认 NVML 的可选符号在本机是否可用（缺符号时字段会是 n/a 而不是崩）。
//! 2. 用 nvidia-smi 这条**完全独立**的通路给 NVML 的读数对拍 —— 显存 used/free/reserved
//!    的字段错位 bug 就是这样发现的。
//! 3. 老驱动上 NVML 不可用时，验证降级解析器还能不能跑。

fn fmt_f(v: Option<f32>, unit: &str) -> String {
    match v {
        Some(v) => format!("{:.1}{}", v, unit),
        None => "n/a".into(),
    }
}

fn main() {
    println!("[1] NvmlGpu::open()");
    let nvml = cs_core::gpu::NvmlGpu::open();
    match &nvml {
        Some(g) => println!("    OK  name = {:?}", g.name()),
        None => println!("    FAILED（无 nvml.dll / init 失败 / 无设备）"),
    }

    if let Some(g) = &nvml {
        println!();
        println!("[2] NVML poll() x3");
        for i in 0..3 {
            let m = g.poll();
            println!(
                "    [{}] usage={:.0}%  vram={:.0}/{:.0} MiB ({:.1}%)  free={} reserved={}  temp={}  margin={}",
                i,
                m.usage_pct,
                m.vram_used_mb,
                m.vram_total_mb,
                m.vram_usage_pct,
                fmt_f(m.vram_free_mb, ""),
                fmt_f(m.vram_reserved_mb, ""),
                fmt_f(m.temp_c, "C"),
                fmt_f(m.temp_margin_c, "C")
            );
            println!(
                "         clk={} mem_clk={}  power={} limit={} ({})  fan={} {}  rpm={}",
                fmt_f(m.core_clock_mhz, ""),
                fmt_f(m.mem_clock_mhz, ""),
                fmt_f(m.power_w, "W"),
                fmt_f(m.power_limit_w, "W"),
                fmt_f(m.power_limit_pct, "%"),
                fmt_f(m.fan_pct, "%"),
                m.fan_count.map(|n| format!("x{}", n)).unwrap_or_default(),
                fmt_f(m.fan_rpm, "")
            );
            println!(
                "         pcie={:?}  rx={} tx={} MiB/s  enc={} dec={}  throttle={:?}",
                m.pcie_link,
                fmt_f(m.pcie_rx_mib_s, ""),
                fmt_f(m.pcie_tx_mib_s, ""),
                fmt_f(m.encoder_pct, "%"),
                fmt_f(m.decoder_pct, "%"),
                m.throttle_reasons
            );
            let unknown = m
                .throttle_reasons
                .as_deref()
                .unwrap_or(&[])
                .iter()
                .any(|r| r.starts_with("未知位"));
            if unknown {
                println!("         [!] 有未定义的降频位，说明 nvml.h 位表比驱动旧");
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
    }

    println!();
    println!("[3] nvidia-smi 降级链路 poll() x2");
    let mut smi = None;
    for i in 0..2 {
        match cs_core::gpu::nvidia_smi_poll() {
            Some(m) => {
                println!(
                    "    [{}] usage={:.0}%  vram={:.0}/{:.0} MiB ({:.1}%)  free={} reserved={}  temp={}  power={} limit={}  fan={}%",
                    i,
                    m.usage_pct,
                    m.vram_used_mb,
                    m.vram_total_mb,
                    m.vram_usage_pct,
                    fmt_f(m.vram_free_mb, ""),
                    fmt_f(m.vram_reserved_mb, ""),
                    fmt_f(m.temp_c, "C"),
                    fmt_f(m.power_w, "W"),
                    fmt_f(m.power_limit_w, "W"),
                    fmt_f(m.fan_pct, "")
                );
                println!(
                    "         clk={} mem_clk={}  pcie={:?}  enc={} dec={}  throttle={:?}  source={}",
                    fmt_f(m.core_clock_mhz, ""),
                    fmt_f(m.mem_clock_mhz, ""),
                    m.pcie_link,
                    fmt_f(m.encoder_pct, "%"),
                    fmt_f(m.decoder_pct, "%"),
                    m.throttle_reasons,
                    m.source
                );
                smi = Some(m);
            }
            None => println!("    [{}] FAILED（nvidia-smi 不在 PATH / 解析失败）", i),
        }
    }

    let (Some(g), Some(s)) = (&nvml, &smi) else {
        println!();
        println!("[4] 跳过对拍（两路没有同时可用）");
        return;
    };
    let n = g.poll();
    println!();
    println!("[4] 字段级对拍（NVML vs nvidia-smi，两次取样相隔不到 1 秒）");
    println!("    {:<16} {:>12} {:>12} {:>10}  {}", "字段", "NVML", "nvidia-smi", "差值", "判定");
    let mut fails = 0;
    let mut cmp = |name: &str, a: Option<f32>, b: Option<f32>, tol: f32, unit: &str| {
        let (Some(a), Some(b)) = (a, b) else {
            println!("    {:<16} {:>12} {:>12} {:>10}  {}", name, "n/a", "n/a", "-", "跳过");
            return;
        };
        let d = (a - b).abs();
        let ok = d <= tol;
        if !ok {
            fails += 1;
        }
        println!(
            "    {:<16} {:>12} {:>12} {:>10}  {}",
            name,
            format!("{:.1}{}", a, unit),
            format!("{:.1}{}", b, unit),
            format!("{:.1}", d),
            if ok { "OK" } else { "FAIL" }
        );
    };
    cmp("vram_used_mb", Some(n.vram_used_mb), Some(s.vram_used_mb), 64.0, "");
    cmp("vram_total_mb", Some(n.vram_total_mb), Some(s.vram_total_mb), 1.0, "");
    cmp("vram_free_mb", n.vram_free_mb, s.vram_free_mb, 64.0, "");
    cmp("vram_reserved", n.vram_reserved_mb, s.vram_reserved_mb, 8.0, "");
    cmp("temp_c", n.temp_c, s.temp_c, 3.0, "C");
    cmp("power_w", n.power_w, s.power_w, 60.0, "W");
    cmp("power_limit_w", n.power_limit_w, s.power_limit_w, 1.0, "W");
    cmp("fan_pct", n.fan_pct, s.fan_pct, 3.0, "%");
    cmp("core_clock_mhz", n.core_clock_mhz, s.core_clock_mhz, 200.0, "");
    cmp("mem_clock_mhz", n.mem_clock_mhz, s.mem_clock_mhz, 50.0, "");
    cmp("encoder_pct", n.encoder_pct, s.encoder_pct, 3.0, "%");
    cmp("decoder_pct", n.decoder_pct, s.decoder_pct, 3.0, "%");
    let link_ok = n.pcie_link == s.pcie_link;
    println!(
        "    {:<16} {:>12} {:>12} {:>10}  {}",
        "pcie_link",
        n.pcie_link.clone().unwrap_or_else(|| "n/a".into()),
        s.pcie_link.clone().unwrap_or_else(|| "n/a".into()),
        "-",
        if link_ok { "OK" } else { "FAIL" }
    );
    if !link_ok {
        fails += 1;
    }
    let throttle_ok = n.throttle_reasons == s.throttle_reasons;
    println!(
        "    {:<16} {:>12} {:>12} {:>10}  {}",
        "throttle",
        format!("{:?}", n.throttle_reasons),
        format!("{:?}", s.throttle_reasons),
        "-",
        if throttle_ok { "OK" } else { "FAIL" }
    );
    if !throttle_ok {
        fails += 1;
    }
    println!();
    println!("[5] 结论：{} 项不一致", fails);
}
