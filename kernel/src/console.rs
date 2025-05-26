use crate::arch::sbi;

fn print_str(s: &str) {
    for c in s.bytes() {
        let _ = sbi::console_putchar(c);
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
