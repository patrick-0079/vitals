//! 存储探针：逐盘打印 IOCTL 结果，排障用。
//! 用法：cargo run -p cs-core --example storage_probe --release

fn main() {
    println!("== 存储探测（PhysicalDrive 扫描）==");
    let mut found = 0;
    for i in 0..32u32 {
        let p = cs_core::storage::probe_drive(i);
        if p.open_error != 0 {
            if i < 12 {
                println!("[{:2}] 打开失败 GetLastError={}", i, p.open_error);
            }
            continue;
        }
        found += 1;
        println!("\n[{}] \\\\.\\PhysicalDrive{}", i, i);
        println!("  desc: bus_type={} vendor={:?} product={:?} serial={:?} access=0x{:X}",
            p.desc.bus_type, p.desc.vendor, p.desc.product, p.desc.serial, p.access);
        println!("  temp_prop(prop=52)={:?}", p.temp_prop);
        println!("  health_error={} bytes_returned={}", p.health_error, p.bytes_returned);
        if p.desc.bus_type == cs_core::storage::BUS_TYPE_NVME && p.health.is_none() {
            println!("  -- NVMe 查询变体穷举 --");
            for a in cs_core::storage::diagnose_nvme(i) {
                println!("     {:<24} err={:<3} bytes={:<5} temp={:?}",
                    a.label, a.error, a.bytes_returned, a.temp_c);
            }
        }
        match p.health.as_ref() {
            Some(h) => {
                println!("  temp_c={:.1} spare={:.0}% used={:.0}% hours={} written={:.1} GB read={:.1} GB",
                    h.temp_c,
                    h.available_spare_pct,
                    h.percentage_used_pct,
                    h.power_on_hours,
                    h.data_units_written as f64 * cs_core::storage::NVME_DATA_UNIT_BYTES / 1e9,
                    h.data_units_read as f64 * cs_core::storage::NVME_DATA_UNIT_BYTES / 1e9);
                println!("  warn={} cycles={} unsafe_shutdowns={} media_errors={}",
                    h.critical_warning, h.power_cycles, h.unsafe_shutdowns, h.media_errors);
            }
            None => println!("  health: 无（非 NVMe 或读取失败）"),
        }
    }
    println!("\n共发现 {} 块盘", found);
}