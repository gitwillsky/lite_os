use crate::arch::sbi;
use crate::drivers::{virtio_console_write, is_virtio_console_available};

fn print_str(s: &str) {
    // 优先使用VirtIO Console，如果不可用或失败则回退到SBI
    if is_virtio_console_available() {
        match virtio_console_write(s.as_bytes()) {
            Ok(_) => return,
            Err(msg) => {
                msg.as_bytes().iter().for_each(|b| {
                    let _ = sbi::console_putchar(*b as usize);
                });
            }
        }
    }

    "fallback to sbi::console_putchar".as_bytes().iter().for_each(|b| {
        let _ = sbi::console_putchar(*b as usize);
    });

    print_str_legacy(s);
}

pub fn print_str_legacy(s: &str) {
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
