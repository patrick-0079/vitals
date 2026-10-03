//! CLI demo —— 展示层参考实现，同时是 cs_core.dll 的第一个消费者。
//!
//! 用法：
//!   cs-cli              单帧面板，打印一次即退出
//!   cs-cli --watch      持续刷新面板（Ctrl+C 退出）
//!   cs-cli --jsonl      每帧一行 JSON（机器可读，喂给别的工具/管道）
//!   cs-cli --jsonl --watch  持续 JSONL 流
//!   cs-cli --info       打印硬件静态信息 JSON
//!   组合：--info 优先；--jsonl 次之；默认面板

use std::io::{IsTerminal, Write};
use std::time::Duration;

use cs_core::Monitor;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let has = |flag: &str| args.iter().any(|a| a == flag);

    let watch = has("--watch");
    let jsonl = has("--jsonl");

    #[cfg(windows)]
    enable_vt();

    let mon = Monitor::start();

    if has("--info") {
        println!("{}", serde_json::to_string_pretty(mon.info()).unwrap());
        mon.stop();
        return;
    }

    if jsonl {
        // 首帧（Monitor::metrics 会等首帧就绪）
        println!("{}", serde_json::to_string(&mon.metrics()).unwrap());
        let _ = std::io::stdout().flush();
        if watch {
            loop {
                std::thread::sleep(Duration::from_millis(500));
                println!("{}", serde_json::to_string(&mon.metrics()).unwrap());
                let _ = std::io::stdout().flush();
            }
        }
        mon.stop();
        return;
    }

    // 面板模式
    let frames = if watch { u64::MAX } else { 3 };
    for i in 0..frames {
        let m = mon.metrics();
        if i > 0 {
            std::thread::sleep(Duration::from_millis(500));
        }
        // 先跑几帧让 sysinfo 占用率稳定，但只打印最后一帧（单帧模式）
        if watch || i == frames - 1 {
            print_panel(&m, mon.info());
        }
    }
    mon.stop();
}

/// 单帧面板。ANSI 光标归位，不整屏清（避免闪烁）。
fn print_panel(m: &cs_core::schema::Metrics, info: &cs_core::schema::Info) {
    let mut out = String::with_capacity(1024);
    if std::io::stdout().is_terminal() {
        // \x1b[H 光标回左上角；\x1b[J 清除从光标到屏幕尾
        out.push_str("\x1b[H\x1b[J");
    }
    out.push_str(&format!(
        "computer-status v{}  {}\n\n",
        info.version, m.cpu.temp_source
    ));

    // CPU 行
    let temp = m
        .cpu
        .temp_c
        .map(|t| format!("{:5.1}°C", t))
        .unwrap_or_else(|| "  n/a".into());
    let power = m
        .cpu
        .package_power_w
        .map(|p| format!("{:5.1} W", p))
        .unwrap_or_default();
        out.push_str(&format!(
            "{:<8}{:<28} usage {:5.1}%  {:5} MHz  {}  {}\n",
            "CPU", m.cpu.name, m.cpu.usage_pct, m.cpu.freq_mhz, temp, power
        ));

    // CPU 温度拿不到且 PawnIO 被拒 → 明说原因，不给“坏了”的错觉
    if m.cpu.temp_c.is_none() && info.sources.pawnio_access_denied {
        out.push_str("        ! CPU 温度/功耗需要管理员权限（PawnIO 设备只对管理员开放）\n");
    }

    // 每核占用条
    if !m.cpu.per_core_pct.is_empty() {
        out.push_str(&format!(
            "{:<8}{}\n",
            "CORES",
            m.cpu
                .per_core_pct
                .iter()
                .map(|&u| bar(u))
                .collect::<Vec<_>>()
                .join(" ")
        ));
    }

    // CCD 温度
    if !m.cpu.ccd_temps_c.is_empty() {
        let ccds = m
            .cpu
            .ccd_temps_c
            .iter()
            .map(|t| format!("{:4.1}", t))
            .collect::<Vec<_>>()
            .join(" ");
        out.push_str(&format!("{:<8}{}°C  ({})\n", "CCD", ccds, m.cpu.temp_source));
    }

    // SMU PM 表（Zen4/5 的 Core/SoC 电压、电流、分项功耗）
    if !m.cpu.smu.is_empty() {
        let volt = |x: Option<f32>| {
            x.map(|v| format!("{:.4} V", v))
                .unwrap_or_else(|| "n/a".into())
        };
        out.push_str(&format!(
            "{:<8}Core {}   SoC {}\n",
            "VOLT",
            volt(m.cpu.core_voltage_v),
            volt(m.cpu.soc_voltage_v),
        ));

        // 其余传感器折成一行；VDDCR/VDDCR SoC 已在上一行
        let rest = m
            .cpu
            .smu
            .iter()
            .filter(|s| !(s.kind == "voltage" && (s.name == "VDDCR" || s.name == "VDDCR SoC")))
            .map(|s| {
                let v = if s.kind == "clock" {
                    format!("{:.0}", s.value)
                } else {
                    format!("{:.1}", s.value)
                };
                format!("{} {}{}", s.name, v, s.unit)
            })
            .collect::<Vec<_>>()
            .join("  ");
        if !rest.is_empty() {
            out.push_str(&format!("{:<8}{}\n", "SMU", rest));
        }
    } else if info.sources.pawnio && !info.sources.smu {
        out.push_str("        ! SMU PM 表不可用（CPU 代号/表版本没有已知布局）\n");
    }

    // 内存
    out.push_str(&format!(
        "{:<8}{:5.1} / {:5.1} GB  ({:5.1}%)  {}\n",
        "MEM",
        m.memory.used_gb,
        m.memory.total_gb,
        m.memory.usage_pct,
        bar(m.memory.usage_pct)
    ));

    // GPU
    if let Some(g) = &m.gpu {
        let temp = g
            .temp_c
            .map(|t| format!("{:5.1}°C", t))
            .unwrap_or_else(|| "  n/a".into());
        let power = g
            .power_w
            .map(|p| format!("{:6.1} W", p))
            .unwrap_or_else(|| "    n/a".into());
        let fan = g
            .fan_pct
            .map(|f| format!("{:3.0}%", f))
            .unwrap_or_else(|| "n/a".into());
        out.push_str(&format!(
            "{:<8}{:<28} usage {:5.1}%  {:6.0}/{:<6.0} MiB ({:5.1}%)  {}  {}  fan {}\n",
            "GPU",
            g.name,
            g.usage_pct,
            g.vram_used_mb,
            g.vram_total_mb,
            g.vram_usage_pct,
            temp,
            power,
            fan
        ));
    } else {
        out.push_str(&format!("{:<8}n/a\n", "GPU"));
    }

    // 存储（NVMe SMART）
    for s in &m.storage {
        let temp = s
            .temp_c
            .map(|t| format!("{:5.1}°C", t))
            .unwrap_or_else(|| "  n/a".into());
        let wear = s
            .percentage_used_pct
            .map(|w| format!("{:3.0}%", w))
            .unwrap_or_else(|| "n/a".into());
        let written = s
            .data_written_gb
            .map(|g| format!("{:7.0} GB", g))
            .unwrap_or_else(|| "     n/a".into());
        let hours = s
            .power_on_hours
            .map(|h| format!("{:6.0} h", h))
            .unwrap_or_else(|| "   n/a".into());
        out.push_str(&format!(
            "{:<8}{:<28} {}  wear {}  wrote {}  {}\n",
            format!("DISK{}", s.index),
            s.name,
            temp,
            wear,
            written,
            hours
        ));
    }
    if m.storage.iter().all(|s| s.source == "none") && info.sources.pawnio_access_denied {
        out.push_str("        ! NVMe SMART 查询被拒（可尝试以管理员身份运行）\n");
    }

    print!("{out}");
    let _ = std::io::stdout().flush();
}

/// 10 格占用条：`▏░▒▓█` 渐进
fn bar(pct: f32) -> String {
    let pct = pct.clamp(0.0, 100.0);
    let filled = (pct / 10.0).round() as usize;
    let width = 10;
    let mut s = String::with_capacity(width * 3);
    for i in 0..width {
        let c = if i < filled {
            if i + 1 == filled && pct < 100.0 {
                "▌" // 部分格
            } else {
                "█"
            }
        } else {
            "·"
        };
        s.push_str(c);
    }
    s
}

/// Windows: 启用 VT 转义序列处理（conhost 需要；Windows Terminal 原生支持）。
#[cfg(windows)]
fn enable_vt() {
    use windows_sys::Win32::System::Console::{
        GetConsoleMode, GetStdHandle, SetConsoleMode, CONSOLE_MODE,
        ENABLE_VIRTUAL_TERMINAL_PROCESSING, STD_OUTPUT_HANDLE,
    };
    unsafe {
        let h = GetStdHandle(STD_OUTPUT_HANDLE);
        if h.is_null() || h as isize == -1 {
            return;
        }
        let mut mode: CONSOLE_MODE = 0;
        if GetConsoleMode(h, &mut mode) != 0 {
            SetConsoleMode(h, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING);
        }
    }
}
