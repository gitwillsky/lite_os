use super::firmware::FirmwareError;

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
static CONSOLE: crate::execution::sync::IrqMutex<ConsoleWriter> =
    crate::execution::sync::IrqMutex::new(ConsoleWriter::new());

pub(crate) fn _print_fmt(args: core::fmt::Arguments) {
    use core::fmt::Write;
    let mut writer = CONSOLE.lock();
    let _ = writer.write_fmt(args);
    let _ = writer.flush();
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

    fn flush(&mut self) -> Result<(), FirmwareError> {
        if self.length == 0 {
            return Ok(());
        }
        // OWNER: CONSOLE guard uniquely owns this BSS buffer. Kernel image mappings are identity
        // mapped, satisfying SBI DBCN's physical-address contract for the synchronous call.
        let result = super::debug_console_write_bytes(&self.bytes[..self.length]);
        self.length = 0;
        result
    }

    /// Copies `bytes` through the batch buffer, flushing whenever it fills.
    ///
    /// A failed flush drops that batch but the copy continues, so best-effort logging keeps its
    /// existing behavior; the first error is returned for callers that must report it.
    fn push(&mut self, mut bytes: &[u8]) -> Result<(), FirmwareError> {
        let mut result = Ok(());
        while !bytes.is_empty() {
            let count = bytes.len().min(CONSOLE_BATCH_BYTES - self.length);
            self.bytes[self.length..self.length + count].copy_from_slice(&bytes[..count]);
            self.length += count;
            bytes = &bytes[count..];
            if self.length == CONSOLE_BATCH_BYTES {
                // 每批都必须复位 length；错误短路 flush 会让满缓冲的下一轮 count=0，永远不能推进。
                let flushed = self.flush();
                result = result.and(flushed);
            }
        }
        result
    }
}

impl core::fmt::Write for ConsoleWriter {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        let _ = self.push(text.as_bytes());
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
