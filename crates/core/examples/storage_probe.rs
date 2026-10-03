//! 存储探针：逐盘打印 IOCTL 结果，排障用。
//! 用法：cargo run -p cs-core --example storage_probe --release
//! 磁盘活动率/吞吐（IOCTL_DISK_PERFORMANCE）需要管理员权限才有值。

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
        println!(
            "  desc: bus_type={} vendor={:?} product={:?} serial={:?} access=0x{:X}",
            p.desc.bus_type, p.desc.vendor, p.desc.product, p.desc.serial, p.access
        );
        println!("  health_error={} bytes_returned={}", p.health_error, p.bytes_returned);
        match p.temp_desc.as_ref() {
            Some(d) => println!(
                "  temp_desc: composite={:?} warning={:?} critical={:?} sensors={:?}",
                d.composite_c, d.warning_c, d.critical_c, d.sensors
            ),
            None => println!("  temp_desc(prop=52): 读失败"),
        }
        println!(
            "  identify_controller: WCTEMP={:?} CCTEMP={:?}",
            p.id_warn_c, p.id_crit_c
        );
        match p.perf.as_ref() {
            Some(pf) => {
                println!(
                    "  perf: code=0x{:05X} bytes_read={} bytes_written={} read_time={} write_time={} idle_time={} query_time={}",
                    p.perf_code, pf.bytes_read, pf.bytes_written, pf.read_time, pf.write_time, pf.idle_time, pf.query_time
                );
                println!(
                    "        read_count={} write_count={} queue={} split={} device_number={}",
                    pf.read_count, pf.write_count, pf.queue_depth, pf.split_count, pf.storage_device_number
                );
            }
            None => println!(
                "  perf: 读失败 GetLastError={}（1 = 计数器被系统关闭，5 = 需要管理员；两个码值都试过）",
                p.perf_error
            ),
        }
        if p.desc.bus_type == cs_core::storage::BUS_TYPE_NVME && p.health.is_none() {
            println!("  -- NVMe 查询变体穷举 --");
            for a in cs_core::storage::diagnose_nvme(i) {
                println!(
                    "     {:<24} err={:<3} bytes={:<5} temp={:?}",
                    a.label, a.error, a.bytes_returned, a.temp_c
                );
            }
        }
        match p.health.as_ref() {
            Some(h) => {
                println!(
                    "  temp_c={:.1} sensors={:?} spare={:.0}% used={:.0}% hours={} written={:.1} GB read={:.1} GB",
                    h.temp_c,
                    h.temp_sensors_c,
                    h.available_spare_pct,
                    h.percentage_used_pct,
                    h.power_on_hours,
                    h.data_units_written as f64 * cs_core::storage::NVME_DATA_UNIT_BYTES / 1e9,
                    h.data_units_read as f64 * cs_core::storage::NVME_DATA_UNIT_BYTES / 1e9
                );
                println!(
                    "  warn={} cycles={} unsafe_shutdowns={} media_errors={}",
                    h.critical_warning, h.power_cycles, h.unsafe_shutdowns, h.media_errors
                );
            }
            None => println!("  health: 无（非 NVMe 或读取失败）"),
        }
    }
    println!("\n共发现 {} 块盘", found);
}
