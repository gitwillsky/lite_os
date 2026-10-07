//! QEMU `virt` PL011 early/runtime output endpoint。

const EARLY_PL011_BASE: usize = 0x0900_0000;
/// early console 只访问 DATA/FLAG 两个寄存器；QEMU `virt` 的 PL011 window 为一页。
const EARLY_PL011_SIZE: usize = 0x1000;
const DATA_REGISTER: usize = 0x00;
const FLAG_REGISTER: usize = 0x18;
const TRANSMIT_FIFO_FULL: u32 = 1 << 5;

#[derive(Debug, Clone, Copy)]
pub(crate) struct ConsoleError;

#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => {
        $crate::platform::console::_print_fmt(format_args!($($arg)*));
    };
}

#[macro_export]
macro_rules! println {
    ($($arg:tt)*) => {
        $crate::print!("{}\n", format_args!($($arg)*));
    };
}

// OWNER: console module owns the unique formatted PL011 output serialization lock.
static CONSOLE: crate::sync::IrqMutex<ConsoleWriter> = crate::sync::IrqMutex::new(ConsoleWriter);

pub(crate) fn _print_fmt(arguments: core::fmt::Arguments) {
    use core::fmt::Write;
    let _ = CONSOLE.lock().write_fmt(arguments);
}

pub(crate) fn panic_print_fmt(arguments: core::fmt::Arguments) {
    use core::fmt::Write;
    let _ = PanicConsoleWriter.write_fmt(arguments);
}

pub(crate) fn panic_println_fmt(arguments: core::fmt::Arguments) {
    panic_print_fmt(format_args!("{arguments}\n"));
}

struct ConsoleWriter;

impl core::fmt::Write for ConsoleWriter {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        for byte in text.bytes() {
            write_byte(byte).map_err(|_| core::fmt::Error)?;
        }
        Ok(())
    }
}

struct PanicConsoleWriter;

impl core::fmt::Write for PanicConsoleWriter {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        for byte in text.bytes() {
            let _ = write_byte(byte);
        }
        Ok(())
    }
}

/// 轮询 PL011 TX FIFO 写出一个 byte。
///
/// discovery publication 前使用 QEMU `virt` 固定 early base；publication 后只消费已验证
/// DTB base。若 early base 与 DTB 不一致，platform initialize 会 fail-stop，避免继续向未知 MMIO 写入。
pub(crate) fn write_byte(byte: u8) -> Result<(), ConsoleError> {
    let (base, size) = super::discovery::info_if_initialized()
        .map(|info| (info.uart.base_addr, info.uart.size))
        .unwrap_or((EARLY_PL011_BASE, EARLY_PL011_SIZE));
    let bus = crate::hal::MmioBus::new(crate::arch::mmu::physical_to_virtual(base), size)
        .map_err(|_| ConsoleError)?;
    // QEMU virt 固定 early PL011 或 discovery 已验证的永久 direct-mapped PL011；console lock 保证
    // 正常输出不会交错。窗口访问失败说明 DTB 与寄存器布局不符，向调用者报告而不是继续写未知 MMIO。
    while bus.read_u32(FLAG_REGISTER).map_err(|_| ConsoleError)? & TRANSMIT_FIFO_FULL != 0 {
        core::hint::spin_loop();
    }
    bus.write_u32(DATA_REGISTER, u32::from(byte))
        .map_err(|_| ConsoleError)?;
    Ok(())
}

pub(super) fn validate_discovered_base() {
    assert_eq!(
        super::discovery::info().uart.base_addr,
        EARLY_PL011_BASE,
        "QEMU virt early PL011 base differs from DTB"
    );
}
