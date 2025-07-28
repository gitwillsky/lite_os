use crate::arch::sbi;
use crate::sync::spinlock::SpinLock;
use core::sync::atomic::{AtomicBool, Ordering};


/// Simplified multi-CPU console with direct output
struct SimpleMultiCpuConsole {
    /// Global output lock for atomic output
    output_lock: SpinLock<()>,
    /// Emergency direct output flag
    emergency_mode: AtomicBool,
}

impl SimpleMultiCpuConsole {
    const fn new() -> Self {
        Self {
            output_lock: SpinLock::new(()),
            emergency_mode: AtomicBool::new(false),
        }
    }

    /// Direct output with locking for multi-core safety
    fn print_direct(&self, s: &str) {
        let _lock = self.output_lock.lock();
        for byte in s.bytes() {
            let _ = sbi::console_putchar(byte as usize);
        }
        // Add newline if not present
        if !s.ends_with('\n') {
            let _ = sbi::console_putchar(b'\n' as usize);
        }
    }

    /// Emergency output (bypass normal locking)
    fn emergency_output(&self, s: &str) {
        // Try to acquire lock quickly, fallback to direct output
        if let Some(_lock) = self.output_lock.try_lock() {
            for byte in s.bytes() {
                let _ = sbi::console_putchar(byte as usize);
            }
        } else {
            // Emergency: output without lock
            for byte in s.bytes() {
                let _ = sbi::console_putchar(byte as usize);
            }
        }
        
        if !s.ends_with('\n') {
            let _ = sbi::console_putchar(b'\n' as usize);
        }
    }

    /// Set emergency mode
    pub fn set_emergency_mode(&self, enabled: bool) {
        self.emergency_mode.store(enabled, Ordering::Relaxed);
    }

    /// Get emergency mode status
    pub fn is_emergency_mode(&self) -> bool {
        self.emergency_mode.load(Ordering::Relaxed)
    }
}


/// Global multi-CPU console instance
static CONSOLE: SimpleMultiCpuConsole = SimpleMultiCpuConsole::new();

/// Print formatted arguments
pub fn _print_fmt(args: core::fmt::Arguments) {
    let mut buffer = [0u8; 512];
    let mut cursor = 0;

    if let Some(s) = args.as_str() {
        CONSOLE.print_direct(s);
    } else {
        use core::fmt::Write;
        struct StackWriter<'a> {
            buffer: &'a mut [u8],
            cursor: &'a mut usize,
        }

        impl<'a> core::fmt::Write for StackWriter<'a> {
            fn write_str(&mut self, s: &str) -> core::fmt::Result {
                let bytes = s.as_bytes();
                let remaining = self.buffer.len() - *self.cursor;
                let to_copy = bytes.len().min(remaining);

                if to_copy > 0 {
                    self.buffer[*self.cursor..*self.cursor + to_copy]
                        .copy_from_slice(&bytes[..to_copy]);
                    *self.cursor += to_copy;
                }
                Ok(())
            }
        }

        let mut writer = StackWriter {
            buffer: &mut buffer,
            cursor: &mut cursor,
        };

        if writer.write_fmt(args).is_ok() && cursor > 0 {
            if let Ok(s) = core::str::from_utf8(&buffer[..cursor]) {
                CONSOLE.print_direct(s);
            }
        } else {
            CONSOLE.emergency_output("[FORMAT_ERROR]");
        }
    }
}

/// Direct print function for log system
pub fn print_direct(s: &str) {
    CONSOLE.print_direct(s);
}

/// Emergency direct print (bypasses normal locking)
pub fn emergency_print(s: &str) {
    CONSOLE.emergency_output(s);
}

/// Enable/disable emergency mode
pub fn set_emergency_console_mode(enabled: bool) {
    CONSOLE.set_emergency_mode(enabled);
}

/// Check if in emergency mode
pub fn is_emergency_mode() -> bool {
    CONSOLE.is_emergency_mode()
}

/// Console writer for compatibility
struct ConsoleWriter;

impl core::fmt::Write for ConsoleWriter {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        CONSOLE.print_direct(s);
        Ok(())
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

#[macro_export]
macro_rules! emergency_println {
    ($($arg:tt)*) => {
        $crate::console::emergency_print(&{
            let mut buffer = [0u8; 512];
            let mut cursor = 0;
            use core::fmt::Write;
            struct StackWriter<'a> {
                buffer: &'a mut [u8],
                cursor: &'a mut usize,
            }

            impl<'a> core::fmt::Write for StackWriter<'a> {
                fn write_str(&mut self, s: &str) -> core::fmt::Result {
                    let bytes = s.as_bytes();
                    let remaining = self.buffer.len() - *self.cursor;
                    let to_copy = bytes.len().min(remaining);

                    if to_copy > 0 {
                        self.buffer[*self.cursor..*self.cursor + to_copy]
                            .copy_from_slice(&bytes[..to_copy]);
                        *self.cursor += to_copy;
                    }
                    Ok(())
                }
            }

            let mut writer = StackWriter {
                buffer: &mut buffer,
                cursor: &mut cursor,
            };

            let _ = writer.write_fmt(format_args!($($arg)*));

            core::str::from_utf8(&buffer[..cursor]).unwrap_or("[FORMAT_ERROR]")
        });
    };
}
