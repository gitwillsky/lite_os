use core::ptr::null;
use spin::lock_api::Mutex;
use uart16550::Uart16550;

// OWNER: UART module owns the DTB-selected UART mapping after global initialization.
pub(crate) static UART: Mutex<Uart16550Map> = Mutex::new(Uart16550Map(null()));

pub(crate) fn init(base: usize) {
    *UART.lock() = Uart16550Map(base as _);
}

pub(crate) struct Uart16550Map(*const Uart16550<u8>);

// SAFETY: pointer is a permanent DTB MMIO mapping; all device access is serialized by UART Mutex.
unsafe impl Send for Uart16550Map {}
// SAFETY: shared references expose internally synchronized/volatile UART operations under Mutex.
unsafe impl Sync for Uart16550Map {}

impl Uart16550Map {
    #[inline]
    pub(crate) fn get(&self) -> &Uart16550<u8> {
        // SAFETY: init replaces null before use with a validated permanent UART MMIO base; the
        // enclosing mutex guard keeps the mapping and device transaction serialized.
        unsafe { &*self.0 }
    }
}
