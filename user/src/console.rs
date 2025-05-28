use crate::syscall;

fn print_str(s: &str) {
    syscall::sys_write(1, s.as_bytes());
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

pub fn _print_fmt(args: core::fmt::Arguments) {
    use core::fmt::Write;

    match ConsoleWriter.write_fmt(args) {
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
