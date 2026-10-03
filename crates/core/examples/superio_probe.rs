//! SuperIO 探测探针 —— 摸清本机主板上的 LPC SuperIO 芯片型号与传感器寄存器。
//!
//! 对应 LHM 的 `Hardware/Motherboard/Lpc/LpcIO.cs` + `LpcPort.cs` + `PawnIo/LpcIO.cs`。
//!
//! 协议（LHM `LpcPort.cs` / `Nct677X.cs`）：
//! - 配置空间：写 `registerPort` = 0x87 两次进入；之后 regPort/valuePort 组成寄存器号+数据；
//!   写 0xAA 退出。芯片 ID 在 0x20、版本在 0x21、运行时基址在 0x60。
//! - 运行时空间（NCT677X 一类）：`base+0x05` 写 0x4E（bank select），`base+0x06` 写 bank，
//!   `base+0x05` 写寄存器号，读 `base+0x06`。
//! - 运行时空间（NCT668X EC 一类）：`base+0x04` 页选择（0xFF = 空闲）、`base+0x05` index、
//!   `base+0x06` data。
//!
//! 注意：**本文件刻意只用 ASCII 输出** —— 提权脚本用 `*>` 重定向时 PowerShell 按控制台代码页
//! （GBK）解码再转 UTF-8，中文会变成 `鎵撳紑` 之类的乱码。
//!
//! 需要管理员权限（PawnIO 设备只对管理员开放）。

#![cfg(windows)]

use cs_core::pawnio::PawnIo;

const MODULE: &[u8] = include_bytes!("../../../drivers/pawnio/LpcIO.bin");

/// LHM `LpcIO.cs:801-803`
const REGISTER_PORTS: [u16; 2] = [0x2E, 0x4E];
const VALUE_PORTS: [u16; 2] = [0x2F, 0x4F];

/// LHM `LpcIO.cs:780-786`
const CHIP_ID_REGISTER: u8 = 0x20;
const CHIP_REVISION_REGISTER: u8 = 0x21;
const BASE_ADDRESS_REGISTER: u8 = 0x60;
const DEVICE_SELECT_REGISTER: u8 = 0x07;
const NUVOTON_HARDWARE_MONITOR_IO_SPACE_LOCK: u8 = 0x28;
const WINBOND_NUVOTON_HARDWARE_MONITOR_LDN: u8 = 0x0B;
const FINTEK_HARDWARE_MONITOR_LDN: u8 = 0x04;

/// LHM `Nct677X.cs:23-25`
const ADDRESS_REGISTER_OFFSET: u16 = 0x05;
const DATA_REGISTER_OFFSET: u16 = 0x06;
const BANK_SELECT_REGISTER: u8 = 0x4E;
/// LHM `Nct677X.cs:28-31`
const EC_SPACE_PAGE_REGISTER_OFFSET: u16 = 0x04;
const EC_SPACE_INDEX_REGISTER_OFFSET: u16 = 0x05;
const EC_SPACE_DATA_REGISTER_OFFSET: u16 = 0x06;
const EC_SPACE_PAGE_SELECT: u8 = 0xFF;

struct Lpc<'a> {
    p: &'a PawnIo,
}

impl Lpc<'_> {
    fn select_slot(&self, slot: i64) {
        self.p.execute("ioctl_select_slot", &[slot], 0);
    }
    fn find_bars(&self) {
        self.p.execute("ioctl_find_bars", &[], 0);
    }
    fn pio_inb(&self, port: u16) -> u8 {
        self.p
            .execute("ioctl_pio_inb", &[port as i64], 1)
            .map_or(0xFF, |v| v[0] as u8)
    }
    fn pio_outb(&self, port: u16, value: u8) {
        self.p.execute("ioctl_pio_outb", &[port as i64, value as i64], 0);
    }
    fn superio_inb(&self, register: u8) -> u8 {
        self.p
            .execute("ioctl_superio_inb", &[register as i64], 1)
            .map_or(0xFF, |v| v[0] as u8)
    }
    fn superio_inw(&self, register: u8) -> u16 {
        self.p
            .execute("ioctl_superio_inw", &[register as i64], 1)
            .map_or(0xFFFF, |v| v[0] as u16)
    }
    fn superio_outb(&self, register: u8, value: u8) {
        self.p
            .execute("ioctl_superio_outb", &[register as i64, value as i64], 0);
    }
    fn winbond_enter(&self, reg_port: u16) {
        self.pio_outb(reg_port, 0x87);
        self.pio_outb(reg_port, 0x87);
    }
    fn winbond_exit(&self, reg_port: u16) {
        self.pio_outb(reg_port, 0xAA);
    }
    fn it87_enter(&self, reg_port: u16) {
        self.pio_outb(reg_port, 0x87);
        self.pio_outb(reg_port, 0x01);
        self.pio_outb(reg_port, 0x55);
        self.pio_outb(reg_port, if reg_port == 0x4E { 0xAA } else { 0x55 });
    }
}

/// LHM `LpcIO.cs:109-456` 的 Winbond / Nuvoton / Fintek 识别表。
/// 返回值含芯片名与是否走 EC 空间（NCT668x 一族）。
fn identify_winbond(id: u8, revision: u8) -> Option<(&'static str, bool)> {
    let hi = revision & 0xF0;
    let name = match (id, revision, hi) {
        (0x05, 0x07, _) => "Fintek F71858",
        (0x05, 0x41, _) => "Fintek F71882",
        (0x06, 0x01, _) => "Fintek F71862",
        (0x07, 0x23, _) => "Fintek F71889F",
        (0x08, 0x14, _) => "Fintek F71869",
        (0x09, 0x01, _) => "Fintek F71808E",
        (0x09, 0x09, _) => "Fintek F71889ED",
        (0x10, 0x05, _) => "Fintek F71889AD",
        (0x10, 0x07, _) => "Fintek F71869A",
        (0x11, 0x06, _) => "Fintek F71878AD",
        (0x11, 0x18, _) => "Fintek F71811",
        (0x52, 0x17, _) | (0x52, 0x3A, _) | (0x52, 0x41, _) => "Winbond W83627HF",
        (0x82, _, 0x80) => "Winbond W83627THF",
        (0x85, 0x41, _) => "Winbond W83687THF",
        (0x88, _, 0x50) | (0x88, _, 0x60) => "Winbond W83627EHF",
        (0xA0, _, 0x20) => "Winbond W83627DHG",
        (0xA5, _, 0x10) => "Winbond W83667HG",
        (0xB0, _, 0x70) => "Winbond W83627DHGP",
        (0xB3, _, 0x50) => "Winbond W83667HGB",
        (0xB4, _, 0x70) => "Nuvoton NCT6771F",
        (0xC3, _, 0x30) => "Nuvoton NCT6776F",
        (0xC4, _, 0x50) => "Nuvoton NCT610XD",
        (0xC5, _, 0x60) => "Nuvoton NCT6779D",
        (0xC7, 0x32, _) => "Nuvoton NCT6683D (EC space)",
        (0xC8, 0x03, _) => "Nuvoton NCT6791D",
        (0xC9, 0x11, _) => "Nuvoton NCT6792D",
        (0xC9, 0x13, _) => "Nuvoton NCT6792DA",
        (0xD1, 0x21, _) => "Nuvoton NCT6793D",
        (0xD3, 0x52, _) => "Nuvoton NCT6795D",
        (0xD4, 0x23, _) => "Nuvoton NCT6796D",
        (0xD4, 0x2A, _) => "Nuvoton NCT6796DR / NCT5585D",
        (0xD4, 0x51, _) => "Nuvoton NCT6797D",
        (0xD4, 0x2B, _) => "Nuvoton NCT6798D",
        (0xD4, 0x40, _) | (0xD4, 0x41, _) => "Nuvoton NCT6686D (EC space)",
        (0xD5, 0x92, _) => "Nuvoton NCT6687D / NCT6687DR (EC space)",
        (0xD8, 0x02, _) => "Nuvoton NCT6799D / NCT6796DS",
        (0xD8, 0x06, _) => "Nuvoton NCT6701D",
        _ => return None,
    };
    let ec = matches!(name, n if n.contains("EC space"));
    Some((name, ec))
}

/// LHM `LpcIO.cs:613-641` 的 ITE 识别表
fn identify_ite(chip_id: u16) -> Option<&'static str> {
    Some(match chip_id {
        0x8613 => "ITE IT8613E",
        0x8620 => "ITE IT8620E",
        0x8625 => "ITE IT8625E",
        0x8628 => "ITE IT8628E",
        0x8631 => "ITE IT8631E",
        0x8638 => "ITE IT8638E",
        0x8655 => "ITE IT8655E",
        0x8665 => "ITE IT8665E",
        0x8686 => "ITE IT8686E",
        0x8688 => "ITE IT8688E",
        0x8689 => "ITE IT8689E",
        0x8696 => "ITE IT8696E",
        0x8705 => "ITE IT8705F",
        0x8712 => "ITE IT8712F",
        0x8716 => "ITE IT8716F",
        0x8718 => "ITE IT8718F",
        0x8720 => "ITE IT8720F",
        0x8721 => "ITE IT8721F",
        0x8726 => "ITE IT8726F",
        0x8728 => "ITE IT8728F",
        0x8733 => "ITE IT8792E",
        0x8771 => "ITE IT8771E",
        0x8772 => "ITE IT8772E",
        0x8790 => "ITE IT8790E",
        0x8695 => "ITE IT87952E",
        _ => return None,
    })
}

/// NCT677X 一族的运行时读（LHM `Nct677X.cs:1250-1260`，bank + register 协议）
struct Runtime<'a> {
    lpc: &'a Lpc<'a>,
    base: u16,
    ec_space: bool,
}

impl Runtime<'_> {
    fn read(&self, address: u16) -> u8 {
        if !self.ec_space {
            let bank = (address >> 8) as u8;
            let register = (address & 0xFF) as u8;
            self.lpc.pio_outb(self.base + ADDRESS_REGISTER_OFFSET, BANK_SELECT_REGISTER);
            self.lpc.pio_outb(self.base + DATA_REGISTER_OFFSET, bank);
            self.lpc.pio_outb(self.base + ADDRESS_REGISTER_OFFSET, register);
            return self.lpc.pio_inb(self.base + DATA_REGISTER_OFFSET);
        }
        let page = (address >> 8) as u8;
        let index = (address & 0xFF) as u8;
        // 等访问窗口空闲（LHM 超时 500ms 后强占）
        let mut access = self.lpc.pio_inb(self.base + EC_SPACE_PAGE_REGISTER_OFFSET);
        let mut spins = 0;
        while access != EC_SPACE_PAGE_SELECT && spins < 500 {
            std::thread::sleep(std::time::Duration::from_millis(1));
            access = self.lpc.pio_inb(self.base + EC_SPACE_PAGE_REGISTER_OFFSET);
            spins += 1;
        }
        self.lpc
            .pio_outb(self.base + EC_SPACE_PAGE_REGISTER_OFFSET, page);
        self.lpc
            .pio_outb(self.base + EC_SPACE_INDEX_REGISTER_OFFSET, index);
        let result = self.lpc.pio_inb(self.base + EC_SPACE_DATA_REGISTER_OFFSET);
        self.lpc
            .pio_outb(self.base + EC_SPACE_PAGE_REGISTER_OFFSET, EC_SPACE_PAGE_SELECT);
        result
    }
}

fn main() {
    let Some(pawn) = PawnIo::open(MODULE) else {
        println!("[!] PawnIo::open(LpcIO.bin) failed - need administrator rights?");
        return;
    };
    println!("[1] PawnIo opened, LpcIO module loaded");
    let lpc = Lpc { p: &pawn };

    let mut found: Option<(&'static str, u16, u16)> = None; // (chip, reg_port, base)

    for (i, &reg_port) in REGISTER_PORTS.iter().enumerate() {
        let slot = if reg_port == 0x2E { 0 } else { 1 };
        lpc.select_slot(slot);
        println!("\n[2.{i}] config port 0x{reg_port:X} (slot {slot})");

        // --- Winbond / Nuvoton / Fintek ---
        lpc.winbond_enter(reg_port);
        let id = lpc.superio_inb(CHIP_ID_REGISTER);
        let revision = lpc.superio_inb(CHIP_REVISION_REGISTER);
        let vendor_id = lpc.superio_inw(0x23);
        println!(
            "      winbond-enter: id=0x{id:02X} rev=0x{revision:02X} vendor(0x23)=0x{vendor_id:04X}"
        );
        let winbond = identify_winbond(id, revision);
        match winbond {
            Some((name, ec)) => {
                lpc.find_bars();
                lpc.superio_outb(DEVICE_SELECT_REGISTER, WINBOND_NUVOTON_HARDWARE_MONITOR_LDN);
                let mut address = lpc.superio_inw(BASE_ADDRESS_REGISTER);
                std::thread::sleep(std::time::Duration::from_millis(1));
                let verify = lpc.superio_inw(BASE_ADDRESS_REGISTER);
                let lock = lpc.superio_inb(NUVOTON_HARDWARE_MONITOR_IO_SPACE_LOCK);
                println!(
                    "      => {name}  ec_space={ec}  LDN=0x0B  base=0x{address:04X} verify=0x{verify:04X} io_lock=0x{lock:02X}"
                );
                if address == verify && (lock & 0x10) != 0 {
                    lpc.superio_outb(NUVOTON_HARDWARE_MONITOR_IO_SPACE_LOCK, lock & !0x10);
                    println!("      io space lock disabled (0x28: 0x{lock:02X} -> 0x{:02X})", lock & !0x10);
                }
                // Fintek 有些芯片地址寄存器已经带 0x05 偏移（LHM LpcIO.cs:511-512）
                if (address & 0x07) == 0x05 {
                    address &= 0xFFF8;
                    println!("      fintek address alignment applied -> 0x{address:04X}");
                }
                lpc.winbond_exit(reg_port);
                if address == verify && address >= 0x100 && (address & 0xF007) == 0 {
                    found = Some((name, reg_port, address));
                    break;
                }
                println!("      [!] address verification failed, skip");
                continue;
            }
            None => {
                if id != 0 && id != 0xFF {
                    println!("      unknown Winbond/Nuvoton/Fintek id=0x{id:02X} rev=0x{revision:02X}");
                }
                lpc.winbond_exit(reg_port);
            }
        }

        // --- ITE (only 0x2E / 0x4E) ---
        let before = lpc.superio_inw(CHIP_ID_REGISTER);
        if before == 0xFFFF {
            lpc.it87_enter(reg_port);
        }
        let chip_id = lpc.superio_inw(CHIP_ID_REGISTER);
        println!("      it87: chip_id=0x{chip_id:04X} (before-enter=0x{before:04X})");
        if let Some(name) = identify_ite(chip_id) {
            lpc.find_bars();
            lpc.superio_outb(DEVICE_SELECT_REGISTER, 0x04); // IT87_ENVIRONMENT_CONTROLLER_LDN
            let address = lpc.superio_inw(BASE_ADDRESS_REGISTER);
            std::thread::sleep(std::time::Duration::from_millis(1));
            let verify = lpc.superio_inw(BASE_ADDRESS_REGISTER);
            let version = lpc.superio_inb(0x22) & 0x0F;
            println!("      => {name}  version=0x{version:X} base=0x{address:04X} verify=0x{verify:04X}");
            lpc.pio_outb(reg_port, 0x02);
            lpc.pio_outb(VALUE_PORTS[i], 0x02);
            if address == verify && address >= 0x100 && (address & 0xF007) == 0 {
                found = Some((name, reg_port, address));
                break;
            }
        }
    }

    let Some((chip, reg_port, base)) = found else {
        println!("\n[!] no SuperIO chip identified");
        return;
    };
    let ec_space = chip.contains("EC space");
    println!("\n[3] chip={chip} reg_port=0x{reg_port:X} runtime_base=0x{base:X}");

    let rt = Runtime {
        lpc: &lpc,
        base,
        ec_space,
    };

    // 先做一次自检：读几个不该变的寄存器，确认端口映射可用
    let probe_addrs: [u16; 6] = [0x0000, 0x0001, 0x0020, 0x0021, 0x0055, 0x007F];
    let mut stable = true;
    for &a in &probe_addrs {
        let v1 = rt.read(a);
        let v2 = rt.read(a);
        if v1 != v2 {
            stable = false;
        }
        println!("      selftest[{a:#06X}] = 0x{v1:02X} (reread 0x{v2:02X})");
    }
    println!("      selftest stable = {stable}");

    // 全 bank 转储：16 个 bank x 256 寄存器，每行 16 字节
    println!("\n[4] raw runtime dump (bank: 16 bytes per row, offset 0x00-0x0F, 0x10-0x1F, ...)");
    for bank in 0u16..0x10 {
        for row in 0u16..0x10 {
            let start = (bank << 8) | (row << 4);
            let mut line = format!("      {start:04X}:");
            // 同一 bank 连续读，避免每字节都重写 bank select
            for k in 0..16u16 {
                line.push_str(&format!(" {:02X}", rt.read(start | k)));
            }
            println!("{line}");
        }
    }

    // 再读一遍做差分：区分「常量寄存器」与「会变的遥测」
    std::thread::sleep(std::time::Duration::from_millis(300));
    println!("\n[5] second pass, non-zero registers that CHANGED within 300ms");
    for bank in 0u16..0x10 {
        for row in 0u16..0x10 {
            let start = (bank << 8) | (row << 4);
            for k in 0..16u16 {
                let a = start | k;
                let v1 = rt.read(a);
                let v2 = rt.read(a);
                if v1 != v2 {
                    println!("      [{a:04X}] 0x{v1:02X} -> 0x{v2:02X}");
                }
            }
        }
    }
    println!("\n[done]");
}
