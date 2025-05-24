use core::{
    fmt::{self, Write},
    str::FromStr,
};
use spin::Once;

pub trait Console: Sync {
    fn put_char(&self, c: u8);

    #[inline]
    fn put_str(&self, s: &str) {
        for c in s.bytes() {
            self.put_char(c);
        }
    }
}

static CONSOLE: Once<&'static dyn Console> = Once::new();

pub fn init_console(console: &'static dyn Console) {
    CONSOLE.call_once(|| console);

    log::set_logger(&Logger).unwrap();
}

pub fn set_log_level(env: Option<&str>) {
    use log::LevelFilter as Lv;

    log::set_max_level(env.and_then(|s| Lv::from_str(s).ok()).unwrap_or(Lv::Trace));
}

#[inline]
pub fn _print(args: fmt::Arguments) {
    Logger.write_fmt(args).unwrap();
}

struct Logger;

#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => {
        $crate::console::_print(core::format_args!($($arg)*));
    }
}

#[macro_export]
macro_rules! println {
    () => ($crate::print!("\n"));
    ($($arg:tt)*) => {{
        $crate::console::_print(core::format_args!($($arg)*));
        $crate::println!();
    }}
}

impl Write for Logger {
    #[inline]
    fn write_str(&mut self, s: &str) -> Result<(), fmt::Error> {
        let _ = CONSOLE.get().unwrap().put_str(s);
        Ok(())
    }
}

impl log::Log for Logger {
    #[inline]
    fn enabled(&self, _metadata: &log::Metadata) -> bool {
        true
    }

    #[inline]
    fn log(&self, record: &log::Record) {
        use log::Level::*;

        let color_code: u8 = match record.level() {
            Error => 31,
            Warn => 93,
            Info => 34,
            Debug => 32,
            Trace => 90,
        };

        println!(
            "\x1b[{color_code}m[{:>5}] {}\x1b[0m",
            record.level(),
            record.args(),
        )
    }

    fn flush(&self) {}
}
