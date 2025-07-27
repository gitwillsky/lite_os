use crate::arch::sbi;
use crate::sync::spinlock::SpinLock;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// Per-CPU log buffer size (2KB per CPU)
const PER_CPU_BUFFER_SIZE: usize = 2048;
/// Maximum number of CPUs supported
const MAX_CPUS: usize = 32;
/// Flush threshold (flush when buffer is 75% full)
const FLUSH_THRESHOLD: usize = (PER_CPU_BUFFER_SIZE * 3) / 4;
/// Maximum line length
const MAX_LINE_LENGTH: usize = 256;

/// Lock-free ring buffer for per-CPU logging
#[derive(Debug)]
struct RingBuffer {
    /// Fixed-size character buffer
    buffer: [u8; PER_CPU_BUFFER_SIZE],
    /// Write position (atomic)
    write_pos: AtomicUsize,
    /// Read position (atomic)
    read_pos: AtomicUsize,
    /// Buffer overflow counter
    overflow_count: AtomicUsize,
    /// Line boundary markers (positions where lines end)
    line_ends: [AtomicUsize; 64], // Track up to 64 line boundaries
    /// Number of complete lines available
    complete_lines: AtomicUsize,
}

impl RingBuffer {
    const fn new() -> Self {
        const ATOMIC_ZERO: AtomicUsize = AtomicUsize::new(0);
        Self {
            buffer: [0; PER_CPU_BUFFER_SIZE],
            write_pos: AtomicUsize::new(0),
            read_pos: AtomicUsize::new(0),
            overflow_count: AtomicUsize::new(0),
            line_ends: [ATOMIC_ZERO; 64],
            complete_lines: AtomicUsize::new(0),
        }
    }

    /// Write a complete line to the buffer
    fn write_line(&self, line: &str) -> bool {
        let line_bytes = line.as_bytes();
        let line_len = line_bytes.len();

        if line_len == 0 || line_len > MAX_LINE_LENGTH {
            return false;
        }

        // Add newline if not present
        let needs_newline = !line.ends_with('\n');
        let total_len = if needs_newline { line_len + 1 } else { line_len };

        let write_start = self.write_pos.load(Ordering::Relaxed);
        let read_pos = self.read_pos.load(Ordering::Relaxed);

        // Check if we have enough space
        let available_space = if write_start >= read_pos {
            PER_CPU_BUFFER_SIZE - (write_start - read_pos)
        } else {
            read_pos - write_start
        };

        if available_space < total_len + 1 {
            // Buffer overflow - advance read position to make space
            let new_read_pos = (write_start + total_len + 1) % PER_CPU_BUFFER_SIZE;
            self.read_pos.store(new_read_pos, Ordering::Relaxed);
            self.overflow_count.fetch_add(1, Ordering::Relaxed);
        }

        // Write the line to buffer
        for (i, &byte) in line_bytes.iter().enumerate() {
            let pos = (write_start + i) % PER_CPU_BUFFER_SIZE;
            unsafe {
                let ptr = self.buffer.as_ptr() as *mut u8;
                ptr.add(pos).write(byte);
            }
        }

        // Add newline if needed
        if needs_newline {
            let pos = (write_start + line_len) % PER_CPU_BUFFER_SIZE;
            unsafe {
                let ptr = self.buffer.as_ptr() as *mut u8;
                ptr.add(pos).write(b'\n');
            }
        }

        // Update write position
        let new_write_pos = (write_start + total_len) % PER_CPU_BUFFER_SIZE;
        self.write_pos.store(new_write_pos, Ordering::Relaxed);

        // Mark line end
        let line_idx = self.complete_lines.load(Ordering::Relaxed) % 64;
        self.line_ends[line_idx].store(new_write_pos, Ordering::Relaxed);
        self.complete_lines.fetch_add(1, Ordering::Relaxed);

        true
    }

    /// Check if buffer should be flushed
    fn should_flush(&self) -> bool {
        let write_pos = self.write_pos.load(Ordering::Relaxed);
        let read_pos = self.read_pos.load(Ordering::Relaxed);

        let used_space = if write_pos >= read_pos {
            write_pos - read_pos
        } else {
            PER_CPU_BUFFER_SIZE - (read_pos - write_pos)
        };

        used_space >= FLUSH_THRESHOLD || self.complete_lines.load(Ordering::Relaxed) > 0
    }

    /// Read and consume all complete lines
    fn drain_lines(&self, output_buffer: &mut [u8; PER_CPU_BUFFER_SIZE]) -> usize {
        let write_pos = self.write_pos.load(Ordering::Relaxed);
        let read_pos = self.read_pos.load(Ordering::Relaxed);

        if read_pos == write_pos {
            return 0; // No data
        }

        let mut copied = 0;
        if write_pos > read_pos {
            // Simple case: no wraparound
            let len = write_pos - read_pos;
            for i in 0..len {
                output_buffer[i] = unsafe {
                    *self.buffer.as_ptr().add(read_pos + i)
                };
            }
            copied = len;
        } else {
            // Wraparound case
            let len1 = PER_CPU_BUFFER_SIZE - read_pos;
            let len2 = write_pos;

            for i in 0..len1 {
                output_buffer[i] = unsafe {
                    *self.buffer.as_ptr().add(read_pos + i)
                };
            }
            for i in 0..len2 {
                output_buffer[len1 + i] = unsafe {
                    *self.buffer.as_ptr().add(i)
                };
            }
            copied = len1 + len2;
        }

        // Update read position
        self.read_pos.store(write_pos, Ordering::Relaxed);
        self.complete_lines.store(0, Ordering::Relaxed);

        copied
    }

    fn get_stats(&self) -> (usize, usize, usize) {
        let write_pos = self.write_pos.load(Ordering::Relaxed);
        let read_pos = self.read_pos.load(Ordering::Relaxed);
        let overflow = self.overflow_count.load(Ordering::Relaxed);

        let used = if write_pos >= read_pos {
            write_pos - read_pos
        } else {
            PER_CPU_BUFFER_SIZE - (read_pos - write_pos)
        };

        (used, overflow, self.complete_lines.load(Ordering::Relaxed))
    }
}

/// Multi-CPU safe console system using fixed buffers
struct FixedMultiCpuConsole {
    /// Per-CPU ring buffers
    per_cpu_buffers: [RingBuffer; MAX_CPUS],
    /// Global output lock for atomic line output
    output_lock: SpinLock<()>,
    /// Emergency direct output flag
    emergency_mode: AtomicBool,
    /// Total flush operations counter
    flush_count: AtomicUsize,
    /// Background flush interval
    last_background_flush: AtomicUsize,
}

impl FixedMultiCpuConsole {
    const fn new() -> Self {
        const INIT_BUFFER: RingBuffer = RingBuffer::new();
        Self {
            per_cpu_buffers: [INIT_BUFFER; MAX_CPUS],
            output_lock: SpinLock::new(()),
            emergency_mode: AtomicBool::new(false),
            flush_count: AtomicUsize::new(0),
            last_background_flush: AtomicUsize::new(0),
        }
    }

    /// Write a line to the appropriate CPU buffer
    fn write_line(&self, line: &str) {
        // In emergency mode, output directly
        if self.emergency_mode.load(Ordering::Relaxed) {
            self.direct_output(line);
            return;
        }

        let cpu_id = self.get_cpu_id();
        let buffer = &self.per_cpu_buffers[cpu_id];

        // Try to write to buffer
        if buffer.write_line(line) {
            // Check if we need to flush
            if buffer.should_flush() {
                self.flush_cpu_buffer(cpu_id);
            }
        } else {
            // Fallback to direct output if buffer write fails
            self.direct_output(line);
        }
    }

    /// Flush specific CPU buffer
    fn flush_cpu_buffer(&self, cpu_id: usize) {
        if cpu_id >= MAX_CPUS {
            return;
        }

        let buffer = &self.per_cpu_buffers[cpu_id];
        let mut output_buffer = [0u8; PER_CPU_BUFFER_SIZE];

        let len = buffer.drain_lines(&mut output_buffer);
        if len > 0 {
            let _output_guard = self.output_lock.lock();

            // Output the buffered data
            for i in 0..len {
                let _ = sbi::console_putchar(output_buffer[i] as usize);
            }

            self.flush_count.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Direct output bypassing buffers
    fn direct_output(&self, s: &str) {
        let _output_guard = self.output_lock.lock();
        for byte in s.bytes() {
            let _ = sbi::console_putchar(byte as usize);
        }
    }

    /// Flush all CPU buffers
    pub fn flush_all_buffers(&self) {
        for cpu_id in 0..MAX_CPUS {
            self.flush_cpu_buffer(cpu_id);
        }
    }

    /// Get current CPU ID safely
    fn get_cpu_id(&self) -> usize {
        // Get CPU ID, clamp to valid range
        // During early boot, this might return 0 if tp register not set
        let cpu_id = crate::smp::current_cpu_id();
        cpu_id.min(MAX_CPUS - 1)
    }

    /// Enable emergency mode (direct output only)
    pub fn set_emergency_mode(&self, enabled: bool) {
        if enabled {
            // Flush all buffers before entering emergency mode
            self.flush_all_buffers();
        }
        self.emergency_mode.store(enabled, Ordering::Relaxed);
    }

    /// Get statistics
    pub fn get_stats(&self) -> ConsoleStats {
        let mut total_buffered = 0;
        let mut total_overflows = 0;
        let mut total_lines = 0;

        for cpu_id in 0..MAX_CPUS {
            let (used, overflow, lines) = self.per_cpu_buffers[cpu_id].get_stats();
            total_buffered += used;
            total_overflows += overflow;
            total_lines += lines;
        }

        ConsoleStats {
            total_buffered_bytes: total_buffered,
            total_overflows,
            flush_operations: self.flush_count.load(Ordering::Relaxed),
            emergency_mode: self.emergency_mode.load(Ordering::Relaxed),
            pending_lines: total_lines,
        }
    }

    /// Periodic background flush (should be called from timer interrupt)
    pub fn background_flush(&self) {
        // Simple throttling - only flush every ~100ms worth of calls
        let current_time = self.flush_count.load(Ordering::Relaxed);
        let last_flush = self.last_background_flush.load(Ordering::Relaxed);

        if current_time - last_flush > 10 {
            self.flush_all_buffers();
            self.last_background_flush.store(current_time, Ordering::Relaxed);
        }
    }
}

/// Console usage statistics
#[derive(Debug, Clone, Copy)]
pub struct ConsoleStats {
    pub total_buffered_bytes: usize,
    pub total_overflows: usize,
    pub flush_operations: usize,
    pub emergency_mode: bool,
    pub pending_lines: usize,
}

/// Global multi-CPU console instance
static MULTI_CPU_CONSOLE: FixedMultiCpuConsole = FixedMultiCpuConsole::new();

/// Print formatted arguments
pub fn _print_fmt(args: core::fmt::Arguments) {
    // We need to format without heap allocation
    // Use a stack buffer for formatting
    let mut buffer = [0u8; MAX_LINE_LENGTH];
    let mut cursor = 0;

    // Simple formatting - just extract the string if possible
    if let Some(s) = args.as_str() {
        MULTI_CPU_CONSOLE.write_line(s);
    } else {
        // Fallback: try to format into stack buffer
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

                if to_copy < bytes.len() {
                    Err(core::fmt::Error)
                } else {
                    Ok(())
                }
            }
        }

        let mut writer = StackWriter {
            buffer: &mut buffer,
            cursor: &mut cursor,
        };

        if writer.write_fmt(args).is_ok() && cursor > 0 {
            if let Ok(s) = core::str::from_utf8(&buffer[..cursor]) {
                MULTI_CPU_CONSOLE.write_line(s);
            }
        } else {
            // Last resort - direct output
            MULTI_CPU_CONSOLE.direct_output("[FORMAT_ERROR]");
        }
    }
}

/// Flush all console buffers (should be called periodically)
pub fn flush_console_buffers() {
    MULTI_CPU_CONSOLE.flush_all_buffers();
}

/// Force flush current CPU's buffer
pub fn flush_current_cpu_buffer() {
    let cpu_id = MULTI_CPU_CONSOLE.get_cpu_id();
    MULTI_CPU_CONSOLE.flush_cpu_buffer(cpu_id);
}

/// Enable/disable emergency mode (direct output)
pub fn set_emergency_console_mode(enabled: bool) {
    MULTI_CPU_CONSOLE.set_emergency_mode(enabled);
}

/// Get console statistics
pub fn get_console_stats() -> ConsoleStats {
    MULTI_CPU_CONSOLE.get_stats()
}

/// Emergency direct print (bypasses all buffering)
pub fn emergency_print(s: &str) {
    MULTI_CPU_CONSOLE.direct_output(s);
}

/// Write a log line directly (optimized for log system integration)
pub fn write_log_line(s: &str) {
    MULTI_CPU_CONSOLE.write_line(s);
}

/// Background flush (called from timer)
pub fn background_flush_console() {
    MULTI_CPU_CONSOLE.background_flush();
}

/// Legacy console writer for compatibility
struct ConsoleWriter;

impl core::fmt::Write for ConsoleWriter {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        MULTI_CPU_CONSOLE.write_line(s);
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
            let mut buffer = [0u8; 256];
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
            let _ = writer.write_str("\n");

            core::str::from_utf8(&buffer[..cursor]).unwrap_or("[FORMAT_ERROR]")
        });
    };
}
