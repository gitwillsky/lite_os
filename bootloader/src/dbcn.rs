use crate::uart16550;
use core::ops::Range;
use rustsbi::{Console, Physical, SbiRet};
use spin::Once;

/// 调试控制提扩展(Debug Console Extension,DBCN)
pub(crate) struct DBCN(Range<usize>);

// OWNER: DBCN module owns the unique SBI debug-console adapter.
static INSTANCE: Once<DBCN> = Once::new();

pub(crate) fn init(memory: Range<usize>) {
    INSTANCE.call_once(|| DBCN(memory));
}

pub(crate) fn get() -> &'static DBCN {
    INSTANCE.wait()
}

impl Console for DBCN {
    fn write(&self, bytes: Physical<&[u8]>) -> SbiRet {
        let Some((start, end)) = self.valid_range(&bytes) else {
            return SbiRet::invalid_param();
        };
        if start == end {
            SbiRet::success(0)
        } else {
            // SAFETY: valid_range proves start..end lies in DRAM and its checked length equals
            // bytes.num_bytes(); the immutable slice lives only for this synchronous call.
            let buf = unsafe { core::slice::from_raw_parts(start as *const u8, bytes.num_bytes()) };
            SbiRet::success(uart16550::UART.lock().get().write(buf))
        }
    }

    fn read(&self, bytes: Physical<&mut [u8]>) -> SbiRet {
        let Some((start, end)) = self.valid_range(&bytes) else {
            return SbiRet::invalid_param();
        };
        if start == end {
            SbiRet::success(0)
        } else {
            // SAFETY: valid_range proves start..end lies in DRAM and exclusively represents the
            // supervisor output buffer for this synchronous SBI call.
            let buf =
                unsafe { core::slice::from_raw_parts_mut(start as *mut u8, bytes.num_bytes()) };
            SbiRet::success(uart16550::UART.lock().get().read(buf))
        }
    }

    #[inline]
    fn write_byte(&self, byte: u8) -> SbiRet {
        let uart = uart16550::UART.lock();
        loop {
            if uart.get().write(&[byte]) == 1 {
                return SbiRet::success(0);
            }
        }
    }
}

impl DBCN {
    fn valid_range<T>(&self, bytes: &Physical<T>) -> Option<(usize, usize)> {
        if bytes.phys_addr_hi() != 0 {
            return None;
        }
        let start = bytes.phys_addr_lo();
        let end = start.checked_add(bytes.num_bytes())?;
        if start < self.0.start || end > self.0.end {
            return None;
        }
        Some((start, end))
    }
}
