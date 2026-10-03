#![cfg(windows)]
//! SMBus / SPD 探测探针 —— 内存条（DIMM）温度路线的现场测绘工具。
//!
//! 分步打点，每一步都先打印再执行，崩溃/失败时能立刻定位到第几步。
//! **输出刻意全部使用 ASCII**：提权脚本用 `*>` 落盘时按 GBK 解码，中文会变乱码。
//!
//! ```text
//! cargo build --release --examples
//! start-process pwsh -Verb RunAs -File scripts\elev-smbus.ps1
//! ```

use cs_core::smbus::{self, Piix4};
use std::time::Instant;

/// 把 `Result<T, i32>` 压成一行可读文本：成功给值，失败给 `E<errno>`。
fn r<T: std::fmt::Display>(v: &Result<T, i32>) -> String {
    match v {
        Ok(x) => format!("{x}"),
        Err(e) => format!("E{e}"),
    }
}

/// 写操作的 `Result<(), i32>` 版本：成功给 `ok`。
fn ok(v: &Result<(), i32>) -> String {
    match v {
        Ok(()) => "ok".into(),
        Err(e) => format!("E{e}"),
    }
}

/// DDR5 判定：页 0 的 0x00 == 0x51 且 0x01 == 0x18（`DDR5Accessor.cs:87-160`）。
fn looks_like_ddr5(a: &Result<u8, i32>, b: &Result<u8, i32>) -> bool {
    matches!((a, b), (Ok(0x51), Ok(0x18)))
}

/// DDR4 判定：0x02 属于 {DDR4=12, DDR4E=14, LPDDR4=16, LPDDR4X=17}。
fn looks_like_ddr4(t: &Result<u8, i32>) -> bool {
    matches!(t, Ok(12) | Ok(14) | Ok(16) | Ok(17))
}

/// DDR3 判定：0x02 属于 {DDR3=11, LPDDR3=15}。
fn looks_like_ddr3(t: &Result<u8, i32>) -> bool {
    matches!(t, Ok(11) | Ok(15))
}

fn main() {
    let t0 = Instant::now();

    println!("[1] PawnIo::open(SmbusPIIX4.bin) + ioctl_identity");
    let Some(bus) = Piix4::open(Some(0)) else {
        println!("[!] open failed - need administrator rights?");
        return;
    };
    let (vendor, device) = bus.pci_ids();
    println!(
        "    identity={} base=0x{:04X} pci={:04X}:{:04X} port={} intel={}",
        bus.identity(),
        bus.base(),
        vendor,
        device,
        bus.port(),
        bus.is_intel()
    );

    println!("[2] scan SPD addresses 0x50..=0x57 (raw byte reads)");
    let mut present: Vec<u8> = Vec::new();
    for addr in smbus::SPD_BEGIN..=smbus::SPD_END {
        let n = smbus::SPD_DATA_RETRIES;
        // 0x00/0x01/0x02 是不加 bit7 的读法（RAMSPDToolkit 的 IsAvailable 就这么读）
        let b00 = bus.read_byte_data_retry(addr, 0x00, n);
        let b01 = bus.read_byte_data_retry(addr, 0x01, n);
        let b02 = bus.read_byte_data_retry(addr, 0x02, n);
        // 0x8x 是加了 bit7 的读法（同一个寄存器的另一种寻址，待真机对比）
        let b80 = bus.read_byte_data_retry(addr, 0x80, n);
        let b81 = bus.read_byte_data_retry(addr, 0x81, n);
        let b82 = bus.read_byte_data_retry(addr, 0x82, n);
        let page = bus.read_byte_data_retry(addr, 0x0B, n);
        let page_hi = bus.read_byte_data_retry(addr, 0x8B, n);

        let any_ok = [&b00, &b01, &b02, &b80, &b81, &b82, &page, &page_hi]
            .iter()
            .any(|x| x.is_ok());
        if any_ok {
            present.push(addr);
        }
        println!(
            "    0x{addr:02X}: 00={} 01={} 02={} | 80={} 81={} 82={} | 0B={} 8B={}",
            r(&b00),
            r(&b01),
            r(&b02),
            r(&b80),
            r(&b81),
            r(&b82),
            r(&page),
            r(&page_hi)
        );
    }
    println!("    responding addresses: {present:?}");

    println!("[3] classify by SPD byte 0x02 (memory type) and 0x00/0x01 magic");
    for addr in &present {
        let n = smbus::SPD_DATA_RETRIES;
        let b00 = bus.read_byte_data_retry(*addr, 0x00, n);
        let b01 = bus.read_byte_data_retry(*addr, 0x01, n);
        let b02 = bus.read_byte_data_retry(*addr, 0x02, n);
        let kind = if looks_like_ddr5(&b00, &b01) {
            "DDR5"
        } else if looks_like_ddr4(&b02) {
            "DDR4"
        } else if looks_like_ddr3(&b02) {
            "DDR3"
        } else {
            "unknown"
        };
        println!(
            "    0x{addr:02X}: type=0x{:02X} ({}) kind={kind}",
            b02.clone().unwrap_or(0xFF),
            r(&b02)
        );
    }

    println!("[4] per-DIMM page 0 dump (offset | 0x80) + thermal registers");
    for addr in &present {
        let n = smbus::SPD_DATA_RETRIES;
        // DDR5 的 At() 强制 bit7；这里两个寻址都试，看哪个能出数据。
        let mut lo_line = String::from("    lo(0x00..0x3F, no bit7):");
        let mut hi_line = String::from("    hi(0x80..0xBF, bit7)   :");
        let mut ok_lo = 0;
        let mut ok_hi = 0;
        for off in 0x00u8..0x40 {
            match bus.read_byte_data_retry(*addr, off, n) {
                Ok(v) => {
                    ok_lo += 1;
                    lo_line.push_str(&format!(" {v:02X}"));
                }
                Err(e) => lo_line.push_str(&format!(" E{e}")),
            }
            match bus.read_byte_data_retry(*addr, off | 0x80, n) {
                Ok(v) => {
                    ok_hi += 1;
                    hi_line.push_str(&format!(" {v:02X}"));
                }
                Err(e) => hi_line.push_str(&format!(" E{e}")),
            }
        }
        println!("  0x{addr:02X}: ok_lo={ok_lo}/64 ok_hi={ok_hi}/64");
        println!("{lo_line}");
        println!("{hi_line}");

        // 温度与相关信息（DDR5 用 SPD 自身地址；DDR4 是 0x18|slot 的 TSOD）
        let cap = bus.read_byte_data_retry(*addr, 0x05, n);
        let enabled = bus.read_byte_data_retry(*addr, 0x1A, n);
        let status = bus.read_byte_data_retry(*addr, 0x33, n);
        let wr = bus.read_byte_data_retry(*addr, 0x06, n);
        let temp_lo = bus.read_word_data_retry(*addr, 0x31, n);
        let temp_hi = bus.read_word_data_retry(*addr, 0xB1, n);
        let temp_bare = bus.read_word_data_retry(*addr, 0x31, smbus::SPD_TS_RETRIES);
        println!(
            "    cap(0x05)={} enabled(0x1A)={} status(0x33)={} write_rec(0x06)={}",
            r(&cap),
            r(&enabled),
            r(&status),
            r(&wr)
        );
        println!(
            "    temp word 0x31={} 0xB1={} (again 0x31: {})",
            r(&temp_lo),
            r(&temp_hi),
            r(&temp_bare)
        );
        if let (Ok(c), Ok(w)) = (cap, temp_lo) {
            println!("    => capability=0x{c:02X} (bit1 thermal sensor = {})", (c >> 1) & 1);
            println!("    => raw temp=0x{w:04X} -> {:.2} C", decode_temp(w));
        }
    }

    println!("[5] page switch test: write MR11(0x0B)=1 then read back, then restore 0");
    for addr in &present {
        let n = smbus::SPD_DATA_RETRIES;
        let before = bus.read_byte_data_retry(*addr, 0x0B, n);
        let w = bus.write_byte_data(*addr, 0x0B, 1);
        std::thread::sleep(std::time::Duration::from_millis(2));
        let after = bus.read_byte_data_retry(*addr, 0x0B, n);
        let dump = bus.read_byte_data_retry(*addr, 0x80, n);
        let back = bus.write_byte_data(*addr, 0x0B, 0);
        println!(
            "    0x{addr:02X}: before={} write1={} after={} (data[0]={}) restore={}",
            r(&before),
            ok(&w),
            r(&after),
            r(&dump),
            ok(&back)
        );
    }

    println!("done in {:?}", t0.elapsed());
}

/// `SPDTemperatureConverter.CheckAndConvertTemperature`：bit12 是符号位。
fn decode_temp(raw: u16) -> f32 {
    if raw & 0x1000 != 0 {
        ((raw & !0x1000) as f32) * 0.0625 - 256.0
    } else {
        (raw as f32) * 0.0625
    }
}
