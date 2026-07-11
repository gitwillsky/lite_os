fn print_str(s: &str) {
    for byte in s.bytes() {
        let _ = super::sbi::console_putchar(byte);
    }
}

#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => {
        $crate::arch::console::_print_fmt(format_args!($($arg)*));
    };
}

#[macro_export]
macro_rules! println {
    ($($arg:tt)*) => {
        $crate::print!("{}\n", format_args!($($arg)*));
    };
}

// print 宏可在中断上下文使用；IRQ-safe lock 防止 task 输出被打断后同 hart 再入。
// OWNER: console module owns the unique kernel console endpoint.
static CONSOLE: crate::sync::IrqMutex<ConsoleWriter> = crate::sync::IrqMutex::new(ConsoleWriter);

pub(crate) fn _print_fmt(args: core::fmt::Arguments) {
    use core::fmt::Write;
    let mut writer = CONSOLE.lock();

    match writer.write_fmt(args) {
        Ok(_) => {}
        Err(_) => {
            print_str("Error: ");
            print_str(args.as_str().unwrap_or("Unknown error"));
        }
    }
}

struct ConsoleWriter;
impl core::fmt::Write for ConsoleWriter {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        print_str(s);
        Ok(())
    }
}

//=============================================================================
// Panic 直写通道：无锁、通过 SBI DBCN 单字节接口输出。
// 注意：仅在 panic 路径中调用，避免与正常日志互相打乱
//=============================================================================

pub(crate) fn panic_print_fmt(args: core::fmt::Arguments) {
    use core::fmt::Write;
    let mut w = PanicConsoleWriter;
    let _ = w.write_fmt(args);
}

pub(crate) fn panic_println_fmt(args: core::fmt::Arguments) {
    panic_print_fmt(format_args!("{}\n", args));
}

struct PanicConsoleWriter;
impl core::fmt::Write for PanicConsoleWriter {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        // 直接轮询输出，避免拿锁
        for b in s.bytes() {
            let _ = super::sbi::console_putchar(b);
        }
        Ok(())
    }
}
