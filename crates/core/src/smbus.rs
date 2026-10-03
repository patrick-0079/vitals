#![cfg(windows)]
//! SMBus 传输层（AMD FCH / PIIX4 兼容），移植自 PawnIO 模块 `SmbusPIIX4.p`
//! 与 RAMSPDToolkit 的 `I2CSMBus` 实现。
//!
//! # 它解决什么问题
//!
//! 内存条的温度（以及 SPD 里的型号/序列号/厂商）挂在 SMBus 上，从机地址
//! `0x50..=0x57`（DDR4/DDR5 的 SPD）或 `0x18|(slot&7)`（DDR4/DDR5 的 TSOD）。
//! 读它们必须先有「一次 SMBus 事务」这个原语，本模块就是这一层。
//!
//! # 协议链
//!
//! ```text
//! 我们 ──ioctl_smbus_xfer──▶ SmbusPIIX4.bin ──PIIX4 寄存器──▶ AMD FCH SMBus ──▶ DIMM
//! ```
//!
//! 模块导出（从 .bin 提取的 ASCII 名）：`ioctl_identity`、`ioctl_piix4_port_sel`、
//! `ioctl_smbus_xfer`、`ioctl_clock_freq`、`ioctl_set_sleep_mode`。本模块只用前三个。
//!
//! # 关键契约（照抄 PawnIO 模块，写错就静默读错数据）
//!
//! * `ioctl_identity`：声明 `DEFINE_IOCTL_SIZED(ioctl_identity, 0, 3)` —— 输入长度不
//!   校验、**输出必须正好 3 个 cell**。`out[0]` = 设备名（8 字节打包的 ASCII，如
//!   `"PIIX4"`），`out[1]` = I/O 基址（本机 `0x0B00`），`out[2]` 低 32 位 =
//!   `vendor|device<<16`、高 32 位 = `subsysVendor|subsysDevice<<16`。
//! * `ioctl_piix4_port_sel`：声明 `(1,1)` —— **输入必须正好 1 个 cell**（port，-1..4），
//!   输出 1 个 cell = 切换前的 port。
//! * `ioctl_smbus_xfer`：用 `DEFINE_IOCTL`（不校验尺寸），函数内自查。
//!   `in[0]`=7 位从机地址、`in[1]`=read(1)/write(0)、`in[2]`=SMBus 命令字节、
//!   `in[3]`=协议码、`in[4..]`=写数据。读 BYTE/BYTE_DATA 回 `out[0]` 低字节，
//!   读 WORD_DATA 回 `out[0]`（`DAT0 | DAT1<<8`）。块读是
//!   `pack_bytes_le(out_data, out, 33)`：`out[0]`=长度，其后 33 字节按小端塞进
//!   cell —— 字节 i 在 `out[i/8]` 的第 `(i%8)*8` 位。
//! * 模块注释要求调用前持有 `\BaseNamedObjects\Access_SMBUS.HTP.Method`，即
//!   [`crate::pawnio::SmbusGuard`]（与 LHM 等同样读 SPD 的工具串行化）。
//!
//! # 纠错（超出 LHM/RAMSPDToolkit 的部分）
//!
//! RAMSPDToolkit 的 `i2c_smbus_proc_call` 在非 Intel 厂商上**返回成功但不发任何
//! 事务**（`SMBusInterface.cs:236-255`），DDR5 的写保护绕过分支正好走它 —— 后果是
//! 页从未切换、后续读到旧页数据且无任何错误码。本模块的 [`Piix4::proc_call`] 在非
//! Intel 设备上显式返回 [`ENOTSUP`]，宁可失败也不静默给错数据。

use crate::pawnio::{PawnIo, SmbusGuard};

/// PawnIO 字节码模块（与 LHM `Resources\PawnIo\SmbusPIIX4.bin` 逐字节一致）。
pub const MODULE: &[u8] = include_bytes!("../../../drivers/pawnio/SmbusPIIX4.bin");

/// 模块要求的 SMBus 世界互斥锁等待上限（毫秒）。
const SMBUS_LOCK_TIMEOUT_MS: u32 = 1000;

// ── 错误码（照抄 RAMSPDToolkit `I2CSMBus\Interop\Shared\SharedConstants.cs:18-40`）──
// 约定与 C# 一致：函数返回负值表示错误，这里用 `Result<_, i32>`，`Err` 里存**正的** errno。

pub const ENOACK: i32 = 4;
pub const EIO: i32 = 5;
pub const ENXIO: i32 = 6;
pub const EBADF: i32 = 9;
pub const EAGAIN: i32 = 11;
pub const EBUSY: i32 = 16;
pub const EINVAL: i32 = 22;
pub const ENOTSUP: i32 = 129;
pub const EOPNOTSUPP: i32 = 130;
pub const EPROTO: i32 = 134;
pub const ETIMEDOUT: i32 = 138;

/// RAMSPDToolkit 用来表示超时的 .NET 专有值（`SharedConstants.S_TIMEOUT`）。
/// 它超出 `i32` 范围，本模块的 errno 空间无法表示，仅作记录：与其等价的瞬时错误
/// 在本模块里落到 [`EBUSY`] / [`ETIMEDOUT`] 上。
pub const S_TIMEOUT_HRESULT: i64 = 2_147_024_775;

/// SPD 从机地址范围（`SPDConstants.cs:22-47`）。
pub const SPD_BEGIN: u8 = 0x50;
pub const SPD_END: u8 = 0x57;

/// SPD 读重试次数（`SPDConstants`）。
pub const SPD_TS_RETRIES: u32 = 12;
pub const SPD_DATA_RETRIES: u32 = 5;
pub const SPD_CFG_RETRIES: u32 = 50;

// ── SMBus 协议码（`I2CConstants.cs:17-34`）──

pub const I2C_SMBUS_QUICK: u8 = 0;
pub const I2C_SMBUS_BYTE: u8 = 1;
pub const I2C_SMBUS_BYTE_DATA: u8 = 2;
pub const I2C_SMBUS_WORD_DATA: u8 = 3;
pub const I2C_SMBUS_PROC_CALL: u8 = 4;
pub const I2C_SMBUS_BLOCK_DATA: u8 = 5;
pub const I2C_SMBUS_I2C_BLOCK_DATA: u8 = 8;

pub const I2C_SMBUS_READ: u8 = 1;
pub const I2C_SMBUS_WRITE: u8 = 0;

const I2C_SMBUS_ADDR_MAX: u8 = 0x7F;

/// 把 PawnIO 返回的 Win32 错误（模块把 NTSTATUS 映射成它）归一到本模块 errno。
///
/// 注意 `ERROR_INVALID_FUNCTION(1)`：模块对不认识的协议码就是回这个（实测与
/// `IOCTL_STORAGE_QUERY_PROPERTY` 传错属性 ID 时一样），所以它算「不支持」而不是
/// 「参数错」。
pub fn map_win_err(code: u32) -> i32 {
    match code {
        1 => ENOTSUP,                 // ERROR_INVALID_FUNCTION
        5 => EBADF,                   // ERROR_ACCESS_DENIED
        87 => EINVAL,                 // ERROR_INVALID_PARAMETER
        170 => EBUSY,                 // ERROR_BUSY
        1167 => ENOTSUP,              // ERROR_DEV_NOT_EXIST
        1460 => ETIMEDOUT,            // ERROR_TIMEOUT
        0x8007_0057 => EINVAL,        // HRESULT_FROM_WIN32(ERROR_INVALID_PARAMETER)
        0xC000_000D => EINVAL,        // STATUS_INVALID_PARAMETER
        0xC000_0010 => ENOTSUP,       // STATUS_INVALID_DEVICE_REQUEST
        0xC000_00A2 => EBUSY,         // STATUS_DEVICE_BUSY
        0xC000_00BB => EOPNOTSUPP,    // STATUS_NOT_SUPPORTED
        _ => EPROTO,
    }
}

/// 这个 errno 是否值得重试（`SPDAccessor.cs` 只在忙/超时上重试）。
pub fn is_retryable(errno: i32) -> bool {
    matches!(errno, EBUSY | ETIMEDOUT | EIO)
}

/// 拆 `ioctl_identity` 的 `out[0]`：8 字节小端打包的 ASCII，去掉尾部 NUL。
pub fn unpack_identity(cell: i64) -> String {
    let bytes: Vec<u8> = (0..8).map(|i| ((cell >> (8 * i)) & 0xFF) as u8).collect();
    let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

/// 拆 `ioctl_identity` 的 `out[2]`：(vendor, device, subsys_vendor, subsys_device)。
pub fn decode_pci_identity(cell: i64) -> (u16, u16, u16, u16) {
    let v = cell as u64;
    (
        (v & 0xFFFF) as u16,
        ((v >> 16) & 0xFFFF) as u16,
        ((v >> 32) & 0xFFFF) as u16,
        ((v >> 48) & 0xFFFF) as u16,
    )
}

/// SMBus WORD 数据的字节交换（`SMBusInterface.cs:199-211`）。
pub fn swap_word(w: u16) -> u16 {
    ((w & 0xFF00) >> 8) | ((w & 0x00FF) << 8)
}

/// 按协议码决定 `ioctl_smbus_xfer` 的输出 cell 数（模块校验 `out_size` 0..5）。
pub fn xfer_out_len(protocol: u8) -> usize {
    match protocol {
        I2C_SMBUS_QUICK => 0,
        I2C_SMBUS_WORD_DATA | I2C_SMBUS_PROC_CALL => 2,
        I2C_SMBUS_BLOCK_DATA | I2C_SMBUS_I2C_BLOCK_DATA => 5,
        _ => 1,
    }
}

/// 组装 `ioctl_smbus_xfer` 的输入 cell 数组；越界返回 `None`。
///
/// 顺序即模块契约：`[addr, read_write, command, protocol, data..]`。模块要求
/// `in_size` 在 4..=9 之间。
pub fn build_xfer_input(
    addr: u8,
    read_write: u8,
    command: u8,
    protocol: u8,
    data: &[i64],
) -> Option<Vec<i64>> {
    if addr > I2C_SMBUS_ADDR_MAX || read_write > 1 {
        return None;
    }
    let mut input = vec![
        addr as i64,
        read_write as i64,
        command as i64,
        protocol as i64,
    ];
    input.extend_from_slice(data);
    if input.len() > 9 {
        return None;
    }
    Some(input)
}

/// 解块读输出：`out[0]` 是长度，其后字节按小端塞进 cell。
pub fn unpack_block(out: &[i64]) -> Vec<u8> {
    if out.is_empty() {
        return Vec::new();
    }
    let len = (out[0] as u64 & 0xFF) as usize;
    let mut bytes = Vec::with_capacity(len);
    for i in 0..len {
        let cell = 1 + i / 8;
        let shift = (i % 8) * 8;
        let v = match out.get(cell) {
            Some(v) => *v as u64,
            None => break,
        };
        bytes.push(((v >> shift) & 0xFF) as u8);
    }
    bytes
}

/// 一条 AMD FCH SMBus（PIIX4 兼容）总线。
///
/// 生命周期内持有一个 PawnIO 模块句柄；每次事务临时获取 SMBus 世界互斥锁
/// （与 RAMSPDToolkit 的 `WorldMutexGuard` 粒度一致），避免长时间饿死 FanControl。
pub struct Piix4 {
    pawn: PawnIo,
    base: u16,
    identity: String,
    port: i32,
    vendor: u16,
    device: u16,
}

impl Piix4 {
    /// 打开总线并选中 PIIX4 端口。`port` 为 0（基址 0x0B00）或 1（0x0B20），
    /// 传 `None` 用模块默认端口。
    ///
    /// 失败（无权限、模块加载失败、没有 PIIX4 控制器）返回 `None`。
    pub fn open(port: Option<i32>) -> Option<Piix4> {
        let pawn = PawnIo::open(MODULE)?;
        if let Some(p) = port {
            // 声明 (1,1)：输入必须正好 1 个 cell。
            pawn.execute("ioctl_piix4_port_sel", &[p as i64], 1)?;
        }
        let raw = pawn.execute("ioctl_identity", &[], 3)?;
        if raw.len() < 3 {
            return None;
        }
        let (vendor, device, _, _) = decode_pci_identity(raw[2]);
        Some(Piix4 {
            pawn,
            base: raw[1] as u16,
            identity: unpack_identity(raw[0]),
            port: port.unwrap_or(0),
            vendor,
            device,
        })
    }

    /// 模块自报的设备名（本机为 `"PIIX4"`）。
    pub fn identity(&self) -> &str {
        &self.identity
    }

    /// 控制器 I/O 基址（本机 `0x0B00`）。
    pub fn base(&self) -> u16 {
        self.base
    }

    /// 当前选中的 PIIX4 端口。
    pub fn port(&self) -> i32 {
        self.port
    }

    /// 控制器的 PCI vendor/device ID（本机 `0x1022` / `0x790B`）。
    pub fn pci_ids(&self) -> (u16, u16) {
        (self.vendor, self.device)
    }

    /// 控制器是否为 Intel（决定 `proc_call` 是否真能发事务）。
    pub fn is_intel(&self) -> bool {
        self.vendor == 0x8086
    }

    /// 一次原始 SMBus 事务。`data` 只在写方向使用。
    fn xfer(
        &self,
        addr: u8,
        read_write: u8,
        command: u8,
        protocol: u8,
        data: &[i64],
    ) -> Result<Vec<i64>, i32> {
        let input = build_xfer_input(addr, read_write, command, protocol, data).ok_or(EINVAL)?;
        let _guard = SmbusGuard::wait(SMBUS_LOCK_TIMEOUT_MS).ok_or(EBUSY)?;
        self.pawn
            .execute_hr("ioctl_smbus_xfer", &input, xfer_out_len(protocol))
            .map_err(map_win_err)
    }

    /// `i2c_smbus_read_byte_data`：读从机 `addr` 的寄存器 `command`。
    pub fn read_byte_data(&self, addr: u8, command: u8) -> Result<u8, i32> {
        let out = self.xfer(addr, I2C_SMBUS_READ, command, I2C_SMBUS_BYTE_DATA, &[])?;
        out.first().map(|v| *v as u8).ok_or(EPROTO)
    }

    /// `i2c_smbus_write_byte_data`：写从机 `addr` 的寄存器 `command`。
    pub fn write_byte_data(&self, addr: u8, command: u8, value: u8) -> Result<(), i32> {
        self.xfer(
            addr,
            I2C_SMBUS_WRITE,
            command,
            I2C_SMBUS_BYTE_DATA,
            &[value as i64],
        )?;
        Ok(())
    }

    /// `i2c_smbus_read_word_data`：小端 16 位读。
    pub fn read_word_data(&self, addr: u8, command: u8) -> Result<u16, i32> {
        let out = self.xfer(addr, I2C_SMBUS_READ, command, I2C_SMBUS_WORD_DATA, &[])?;
        out.first().map(|v| *v as u16).ok_or(EPROTO)
    }

    /// `i2c_smbus_read_word_data_swapped`：字节序交换后的 16 位读（DDR4 TSOD 用）。
    pub fn read_word_data_swapped(&self, addr: u8, command: u8) -> Result<u16, i32> {
        self.read_word_data(addr, command).map(swap_word)
    }

    /// 原始字节读（QUICK 之外的 `BYTE` 协议，`i2c_smbus_read_byte`）。
    pub fn read_byte(&self, addr: u8) -> Result<u8, i32> {
        let out = self.xfer(addr, I2C_SMBUS_READ, 0, I2C_SMBUS_BYTE, &[])?;
        out.first().map(|v| *v as u8).ok_or(EPROTO)
    }

    /// `i2c_smbus_proc_call`。**非 Intel 厂商上显式失败**，见模块文档的「纠错」一节。
    pub fn proc_call(&self, addr: u8, read_write: u8, command: u8, value: u16) -> Result<u16, i32> {
        if !self.is_intel() {
            return Err(ENOTSUP);
        }
        let out = self.xfer(
            addr,
            read_write,
            command,
            I2C_SMBUS_PROC_CALL,
            &[value as i64],
        )?;
        out.first().map(|v| *v as u16).ok_or(EPROTO)
    }

    /// 带重试的字节读（只重试忙/超时，间隔 1 ms，照抄 `SPDAccessor.RetryReadByteData`）。
    pub fn read_byte_data_retry(
        &self,
        addr: u8,
        command: u8,
        retries: u32,
    ) -> Result<u8, i32> {
        retry(retries, || self.read_byte_data(addr, command))
    }

    /// 带重试的字读。
    pub fn read_word_data_retry(
        &self,
        addr: u8,
        command: u8,
        retries: u32,
    ) -> Result<u16, i32> {
        retry(retries, || self.read_word_data(addr, command))
    }

    /// 带重试的交换字读（DDR4 TSOD）。
    pub fn read_word_data_swapped_retry(
        &self,
        addr: u8,
        command: u8,
        retries: u32,
    ) -> Result<u16, i32> {
        retry(retries, || self.read_word_data_swapped(addr, command))
    }

    /// 块读。**PIIX4 上没有真正的块读**：RASMPDToolkit 把
    /// `i2c_smbus_read_block_data_compat` 覆写为逐字读（`SMBusInterface.cs:390-439`），
    /// 这里照做。`command + length > 256` 直接判参数错。
    pub fn read_block(&self, addr: u8, command: u8, length: usize) -> Result<Vec<u8>, i32> {
        if length == 0 {
            return Ok(Vec::new());
        }
        if command as usize + length > 256 {
            return Err(EINVAL);
        }
        let mut values = vec![0u8; length];
        let mut index = 0usize;
        while index + 2 <= length {
            let w = self.read_word_data(addr, command.wrapping_add(index as u8))?;
            values[index] = (w & 0xFF) as u8;
            values[index + 1] = (w >> 8) as u8;
            index += 2;
        }
        if index < length {
            values[index] = self.read_byte_data(addr, command.wrapping_add(index as u8))?;
        }
        Ok(values)
    }
}

/// 只对忙/超时重试的循环；间隔 1 ms（`SPDConstants.SPD_IO_DELAY`）。
fn retry<T>(retries: u32, mut f: impl FnMut() -> Result<T, i32>) -> Result<T, i32> {
    let mut last = EPROTO;
    for attempt in 0..retries.max(1) {
        match f() {
            Ok(v) => return Ok(v),
            Err(e) => {
                last = e;
                if !is_retryable(e) {
                    return Err(e);
                }
                if attempt + 1 < retries {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
            }
        }
    }
    Err(last)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_unpacking() {
        // 模块把 5 个字符按小端塞进一个 cell。
        let mut cell: i64 = 0;
        for (i, b) in b"PIIX4".iter().enumerate() {
            cell |= (*b as i64) << (8 * i);
        }
        assert_eq!(unpack_identity(cell), "PIIX4");
        assert_eq!(unpack_identity(0), "");
    }

    #[test]
    fn pci_identity_bit_fields() {
        // vendor=0x1022 device=0x790B subsysVendor=0x1043 subsysDevice=0x8877
        let cell: i64 =
            (0x1022u64 | (0x790Bu64 << 16) | (0x1043u64 << 32) | (0x8877u64 << 48)) as i64;
        assert_eq!(decode_pci_identity(cell), (0x1022, 0x790B, 0x1043, 0x8877));
    }

    #[test]
    fn word_swap_matches_lhm() {
        assert_eq!(swap_word(0x1234), 0x3412);
        assert_eq!(swap_word(0x00FF), 0xFF00);
    }

    #[test]
    fn xfer_input_layout_and_bounds() {
        assert_eq!(
            build_xfer_input(0x50, I2C_SMBUS_READ, 0x0B, I2C_SMBUS_BYTE_DATA, &[]),
            Some(vec![0x50, 1, 0x0B, 2])
        );
        assert_eq!(
            build_xfer_input(0x50, I2C_SMBUS_WRITE, 0x0B, I2C_SMBUS_BYTE_DATA, &[0]),
            Some(vec![0x50, 0, 0x0B, 2, 0])
        );
        // 地址超 0x7F、读写标志非 0/1、输入超过 9 个 cell 都要拒绝。
        assert!(build_xfer_input(0x80, 1, 0, 2, &[]).is_none());
        assert!(build_xfer_input(0x50, 2, 0, 2, &[]).is_none());
        assert!(build_xfer_input(0x50, 0, 0, 5, &[0; 6]).is_none());
    }

    #[test]
    fn out_len_per_protocol() {
        assert_eq!(xfer_out_len(I2C_SMBUS_QUICK), 0);
        assert_eq!(xfer_out_len(I2C_SMBUS_BYTE), 1);
        assert_eq!(xfer_out_len(I2C_SMBUS_BYTE_DATA), 1);
        assert_eq!(xfer_out_len(I2C_SMBUS_WORD_DATA), 2);
        assert_eq!(xfer_out_len(I2C_SMBUS_BLOCK_DATA), 5);
    }

    #[test]
    fn block_output_unpacking() {
        // 长度 3，数据 0xAA 0xBB 0xCC 塞进第一个数据 cell 的低 3 字节。
        let packed = 0xAAu64 | (0xBBu64 << 8) | (0xCCu64 << 16);
        assert_eq!(unpack_block(&[3, packed as i64]), vec![0xAA, 0xBB, 0xCC]);
        assert_eq!(unpack_block(&[0]), Vec::<u8>::new());
        assert_eq!(unpack_block(&[]), Vec::<u8>::new());
    }

    #[test]
    fn win_err_mapping() {
        assert_eq!(map_win_err(1), ENOTSUP);
        assert_eq!(map_win_err(87), EINVAL);
        assert_eq!(map_win_err(0xC000_00A2), EBUSY);
        assert_eq!(map_win_err(12345), EPROTO);
        assert!(is_retryable(EBUSY));
        assert!(!is_retryable(ENOTSUP));
    }
}
