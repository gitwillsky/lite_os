use crate::arch::sbi;

fn print_str(s: &str) {
    for byte in s.bytes() {
        let _ = sbi::console_putchar(byte as usize);
    }
}

#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => {
        $crate::console::_print_fmt(format_args!($($arg)*));
    };
}

#[macro_export]
macro_rules! println {
    ($($arg:tt)*) => {
        $crate::print!("{}\n", format_args!($($arg)*));
    };
}

static CONSOLE: spin::Mutex<ConsoleWriter> = spin::Mutex::new(ConsoleWriter);

pub fn _print_fmt(args: core::fmt::Arguments) {
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
// Panic 直写通道：无锁、直接轮询写 UART（SBI console_putchar）
// 注意：仅在 panic 路径中调用，避免与正常日志互相打乱
//=============================================================================

pub fn panic_print_fmt(args: core::fmt::Arguments) {
    use core::fmt::Write;
    let mut w = PanicConsoleWriter;
    let _ = w.write_fmt(args);
}

pub fn panic_println_fmt(args: core::fmt::Arguments) {
    panic_print_fmt(format_args!("{}\n", args));
}

struct PanicConsoleWriter;
impl core::fmt::Write for PanicConsoleWriter {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        // 直接轮询输出，避免拿锁
        for b in s.bytes() {
            let _ = sbi::console_putchar(b as usize);
        }
        Ok(())
    }
}
