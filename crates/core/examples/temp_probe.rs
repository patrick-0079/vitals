//! 温度/功耗链路探针：逐步打印 PawnIO → SMN → CCD → RAPL 每一步。
//! 用于定位访问违例。`cargo run -p cs-core --example temp_probe --release`

fn main() {
    println!("[1] 打开 PawnIO 设备并加载 AMDFamily17.bin ...");
    let bin: &[u8] = include_bytes!("../../../drivers/pawnio/AMDFamily17.bin");
    let p = match cs_core::pawnio::PawnIo::open(bin) {
        Some(p) => p,
        None => {
            println!("    PawnIO 打开失败（驱动未装/无权限）");
            return;
        }
    };
    println!("    OK");

    println!("[2] CPUID 身份 ...");
    let id = cs_core::amd_temp::cpuid_family_model();
    println!("    family=0x{:X} model=0x{:X} zen={}", id.family, id.model, cs_core::amd_temp::is_zen(id));

    println!("[3] PciBusGuard (Global\\Access_PCI) ...");
    let guard = cs_core::pawnio::PciBusGuard::wait(5000);
    println!("    {}", if guard.is_some() { "OK" } else { "超时/失败" });
    drop(guard);

    println!("[4] read_smn(0x59800) Tctl ...");
    match p.read_smn(0x0005_9800) {
        Some(raw) => println!("    raw=0x{:X} → {}°C", raw, cs_core::amd_temp::tctl_from_raw(raw)),
        None => println!("    失败"),
    }

    println!("[5] CCD 温度 (0x59b08+i*4, 8 个) ...");
    if let Some(base) = cs_core::amd_temp::ccd_layout(id) {
        for i in 0..cs_core::amd_temp::CCD_COUNT {
            match p.read_smn(base + i as u32 * 4) {
                Some(raw) => println!("    ccd[{}]: raw=0x{:X} → {}°C", i, raw, cs_core::amd_temp::ccd_temp_from_raw(raw)),
                None => println!("    ccd[{}]: 失败", i),
            }
        }
    } else {
        println!("    该型号无 CCD 布局");
    }

    println!("[6] RAPL: read_msr(0xC0010299) 能量单位 ...");
    match p.read_msr(0xC001_0299) {
        Some(v) => {
            println!("    raw=0x{:X}  ESU[12:8]={}", v, (v >> 8) & 0x1F);
            match cs_core::amd_temp::read_energy_unit_j(&p) {
                Some(u) => println!("    单位 = {} J/增量", u),
                None => println!("    解析失败"),
            }
        }
        None => println!("    失败"),
    }

    println!("[7] RAPL: read_msr(0xC001029B) 整包能量, 三次差分 ...");
    let unit = cs_core::amd_temp::read_energy_unit_j(&p).unwrap_or(1.0);
    let mut last: Option<(u32, std::time::Instant)> = None;
    for i in 0..3 {
        match cs_core::amd_temp::read_pkg_energy(&p) {
            Some(e) => {
                let w = cs_core::amd_temp::calc_power_w(last, e, unit);
                println!(
                    "    [{}] energy={} → {} W",
                    i,
                    e,
                    w.map(|x| format!("{x:.2}")).unwrap_or_else(|| "基线".into())
                );
                last = Some((e, std::time::Instant::now()));
            }
            None => println!("    [{}] 失败", i),
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }

    println!("全部完成，无崩溃。");
}
