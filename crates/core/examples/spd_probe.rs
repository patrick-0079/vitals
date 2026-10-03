//! DDR5 SPD 探针：把 [`cs_core::spd`] 的每一步摊开打印，便于提权对拍。
//!
//! 用法（管理员）：
//! ```text
//! Start-Process pwsh -Verb RunAs -ArgumentList '-NoProfile','-ExecutionPolicy','Bypass',`
//!   '-File','scripts\elev-spd.ps1' -Wait
//! ```
//! 输出全部 ASCII（提权脚本用 `*>` 重定向，中文会因 GBK 解码变乱码）。
//!
//! 期望与 `Get-CimInstance Win32_PhysicalMemory` 交叉一致：
//! `PartNumber = CP32G60C40U5W.M8B1`、`SerialNumber = EB7F58B8` / `EB7F5EE7`、
//! `Manufacturer = Micron`。

#![cfg(windows)]

use cs_core::smbus::{self, Piix4};
use cs_core::spd::{self, Ddr5};

fn hexdump(title: &str, bytes: &[u8]) {
    println!("{title}");
    for (row, chunk) in bytes.chunks(16).enumerate() {
        let mut line = format!("  {:03X}:", row * 16);
        for b in chunk {
            line.push_str(&format!(" {b:02X}"));
        }
        // 右侧 ASCII 列，非可打印字符用 '.'
        let ascii: String = chunk
            .iter()
            .map(|&b| {
                if (0x20..0x7F).contains(&b) {
                    b as char
                } else {
                    '.'
                }
            })
            .collect();
        println!("{line}   {ascii}");
    }
}

fn main() {
    println!("== DDR5 SPD probe ==");
    println!(
        "[consts] SPD_BEGIN=0x{:02X} SPD_END=0x{:02X} MR_PAGE=0x{:02X} MR_TEMPERATURE=0x{:02X} \
         PART_NUMBER=0x{:03X}..0x{:03X} SERIAL=0x{:03X}..0x{:03X}",
        smbus::SPD_BEGIN,
        smbus::SPD_END,
        spd::MR_PAGE,
        spd::MR_TEMPERATURE,
        spd::SPD_MODULE_PART_NUMBER_BEGIN,
        spd::SPD_MODULE_PART_NUMBER_END,
        spd::SPD_MODULE_SERIAL_BEGIN,
        spd::SPD_MODULE_SERIAL_END
    );

    let Some(bus) = Piix4::open(Some(0)) else {
        println!("[!] Piix4::open failed - need administrator rights?");
        return;
    };
    let (vendor, device) = bus.pci_ids();
    println!(
        "[1] identity={} base=0x{:04X} pci={:04X}:{:04X} port={} intel={}",
        bus.identity(),
        bus.base(),
        vendor,
        device,
        bus.port(),
        bus.is_intel()
    );

    for address in smbus::SPD_BEGIN..=smbus::SPD_END {
        println!("--- slave 0x{address:02X} ---");
        match bus.read_byte_data(address, spd::MR_DEVICE_TYPE_MOST) {
            Err(e) => {
                println!("  [2] no response (errno {e})");
                continue;
            }
            Ok(mr0) => {
                let mr1 = bus.read_byte_data(address, spd::MR_DEVICE_TYPE_LEAST);
                println!(
                    "  [2] MR0=0x{mr0:02X} MR1={}",
                    mr1.map(|v| format!("0x{v:02X}"))
                        .unwrap_or_else(|e| format!("err {e}"))
                );
                let is_ddr5 = spd::is_ddr5(mr0, mr1.unwrap_or(0));
                println!("  [3] is_ddr5={is_ddr5}");
                if !is_ddr5 {
                    continue;
                }
            }
        }

        let Some(mut dimm) = Ddr5::detect(&bus, address) else {
            println!("  [!] Ddr5::detect returned None");
            continue;
        };

        // [4] MR 空间：能力位 / 写恢复 / 页寄存器
        let cap = bus.read_byte_data(address, spd::MR_DEVICE_CAPABILITY);
        let rec = bus.read_byte_data(address, spd::MR_WRITE_RECOVERY_TIME);
        let page = bus.read_byte_data(address, spd::MR_PAGE);
        let enabled = bus.read_byte_data(address, spd::MR_THERMAL_SENSOR_ENABLED);
        println!(
            "  [4] cap={} rec={} page={} enabled={}",
            cap.map(|v| format!("0x{v:02X}(thermal={})", spd::has_thermal_sensor(v)))
                .unwrap_or_else(|e| format!("err {e}")),
            rec.map(|v| format!("0x{v:02X}({}ms)", spd::write_recovery_ms(v)))
                .unwrap_or_else(|e| format!("err {e}")),
            page.map(|v| format!("{v}"))
                .unwrap_or_else(|e| format!("err {e}")),
            enabled
                .map(|v| format!("0x{v:02X}"))
                .unwrap_or_else(|e| format!("err {e}"))
        );

        // [5] 温度原始值 + 换算
        let raw_temp = dimm.mr_word(spd::MR_TEMPERATURE);
        match raw_temp {
            Ok(raw) => println!(
                "  [5] temp_raw=0x{raw:04X} ({raw}) -> {:.2} C",
                spd::decode_temperature(raw)
            ),
            Err(e) => println!("  [5] temp_raw read failed (errno {e})"),
        }

        // [6] 完整解码（身份 + 温度）
        let m = dimm.read_metrics();
        println!(
            "  [6] index={} address=0x{:02X} source={}",
            m.index, m.address, m.source
        );
        println!(
            "      part_number=\"{}\" serial=\"{}\"",
            m.part_number, m.serial_number
        );
        println!(
            "      manufacturer=\"{}\" date={} temp={} status={}",
            m.manufacturer,
            m.manufacture_date.as_deref().unwrap_or("n/a"),
            m.temp_c
                .map(|t| format!("{t:.2} C"))
                .unwrap_or_else(|| "n/a".into()),
            m.thermal_status
        );

        // [7] EEPROM 页转储（与 WMI 的 PartNumber/SerialNumber 对照）
        for page in [0u8, 4u8] {
            match dimm.dump_page(page) {
                Ok(bytes) => hexdump(&format!("  [7] EEPROM page {page}:"), &bytes),
                Err(e) => println!("  [7] dump page {page} failed (errno {e})"),
            }
        }
        // 复读温度，确认 dump 之后页状态没被搞乱
        println!(
            "  [8] temp after dumps = {:?}",
            dimm.temperature().map(|t| format!("{t:.2} C"))
        );

        // [9] 证明采样循环用的 refresh_temperatures 真的会重读（先塞哨兵值）
        let mut list = vec![cs_core::schema::DimmMetrics {
            address,
            temp_c: Some(-999.0),
            thermal_status: "SENTINEL".to_string(),
            ..Default::default()
        }];
        spd::refresh_temperatures(&bus, &mut list);
        println!(
            "  [9] refresh_temperatures: sentinel(-999.0/SENTINEL) -> temp={} status={}",
            list[0]
                .temp_c
                .map(|t| format!("{t:.2} C"))
                .unwrap_or_else(|| "None".into()),
            list[0].thermal_status
        );
    }

    println!("== done ==");
}
