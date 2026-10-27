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
static CONSOLE: crate::sync::IrqMutex<ConsoleWriter> =
    crate::sync::IrqMutex::new(ConsoleWriter::new());

pub(crate) fn _print_fmt(args: core::fmt::Arguments) {
    use core::fmt::Write;
    let mut writer = CONSOLE.lock();
    let _ = writer.write_fmt(args);
    let _ = writer.flush();
}

/// 在 console 序列化锁内把整段 tty 输出经批量缓冲写到 SBI DBCN。
///
/// 与 `_print_fmt` 共用 `CONSOLE`：缺少这把锁时，tty 字节会插进内核日志行的中间。
/// 经 BSS 批量缓冲而非直接传 `bytes`，因为 DBCN 要求 identity-mapped 物理地址，而调用者的
/// 缓冲可能在 kernel stack 上。本函数不得记录日志，否则会在持锁状态下重入同一把 `IrqMutex`。
///
/// # Parameters
///
/// - `bytes`: 一次 tty write 的输出；上界由 tty 层的单次写块（至多 512 字节）决定。
///
/// # Errors
///
/// 任一批 SBI DBCN 写失败时返回首个 firmware error；其余批次仍会尝试写出。
pub(crate) fn write_bytes(bytes: &[u8]) -> Result<(), FirmwareError> {
    let mut writer = CONSOLE.lock();
    let pushed = writer.push(bytes);
    // 前一批失败也要冲出剩余尾部；不能让错误短路而把缓冲留给下一次 tty write。
    let flushed = writer.flush();
    pushed.and(flushed)
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
