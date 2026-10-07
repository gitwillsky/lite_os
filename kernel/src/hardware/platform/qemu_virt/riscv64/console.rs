const CONSOLE_BATCH_BYTES: usize = 256;

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

// print 宏可在中断上下文使用；IRQ-safe lock 防止 task 输出被打断后同 hart 再入。
// OWNER: console module owns the unique kernel console endpoint.
static CONSOLE: crate::sync::IrqMutex<ConsoleWriter> =
    crate::sync::IrqMutex::new(ConsoleWriter::new());

pub(crate) fn _print_fmt(args: core::fmt::Arguments) {
    use core::fmt::Write;
    let mut writer = CONSOLE.lock();
    let _ = writer.write_fmt(args);
    writer.flush();
}

struct ConsoleWriter {
    bytes: [u8; CONSOLE_BATCH_BYTES],
    length: usize,
}

impl ConsoleWriter {
    const fn new() -> Self {
        Self {
            bytes: [0; CONSOLE_BATCH_BYTES],
            length: 0,
        }
    }

    fn flush(&mut self) {
        if self.length == 0 {
            return;
        }
        // OWNER: CONSOLE guard uniquely owns this BSS buffer. Kernel image mappings are identity
        // mapped, satisfying SBI DBCN's physical-address contract for the synchronous call.
        let _ = super::debug_console_write_bytes(&self.bytes[..self.length]);
        self.length = 0;
    }
}

impl core::fmt::Write for ConsoleWriter {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        let mut bytes = text.as_bytes();
        while !bytes.is_empty() {
            let count = bytes.len().min(CONSOLE_BATCH_BYTES - self.length);
            self.bytes[self.length..self.length + count].copy_from_slice(&bytes[..count]);
            self.length += count;
            bytes = &bytes[count..];
            if self.length == CONSOLE_BATCH_BYTES {
                self.flush();
            }
        }
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
    panic_print_fmt(format_args!("{args}\n"));
}

struct PanicConsoleWriter;
impl core::fmt::Write for PanicConsoleWriter {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        // 直接轮询输出，避免拿锁
        for b in s.bytes() {
            let _ = super::debug_console_write(b);
        }
        Ok(())
    }
}
