//! @description QEMU `virt` PL011 early/runtime output endpoint。

const EARLY_PL011_BASE: usize = 0x0900_0000;
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

/// @description 轮询 PL011 TX FIFO 写出一个 byte。
///
/// discovery publication 前使用 QEMU `virt` 固定 early base；publication 后只消费已验证
/// DTB base。若 early base 与 DTB 不一致，platform initialize 会 fail-stop，避免继续向未知 MMIO 写入。
pub(crate) fn write_byte(byte: u8) -> Result<(), ConsoleError> {
    let base = super::discovery::info_if_initialized()
        .map(|info| info.uart.base_addr)
        .unwrap_or(EARLY_PL011_BASE);
    let base = crate::arch::mmu::physical_to_virtual(base);
    // SAFETY: QEMU virt 固定 early PL011 或 discovery 已验证的永久 direct-mapped PL011；
    // volatile 访问维持 device semantics，console lock 保证正常输出不会交错。
    unsafe {
        while core::ptr::read_volatile((base + FLAG_REGISTER) as *const u32) & TRANSMIT_FIFO_FULL
            != 0
        {
            core::hint::spin_loop();
        }
        core::ptr::write_volatile((base + DATA_REGISTER) as *mut u32, byte as u32);
    }
    Ok(())
}

pub(super) fn validate_discovered_base() {
    assert_eq!(
        super::discovery::info().uart.base_addr,
        EARLY_PL011_BASE,
        "QEMU virt early PL011 base differs from DTB"
    );
}
