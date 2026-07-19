//! @description QEMU `virt` 16550 RX register adapter。

use alloc::sync::Arc;
use spin::Once;

use crate::drivers::{InterruptError, InterruptHandler, InterruptVector};

const RECEIVE_BUFFER: usize = 0;
const INTERRUPT_ENABLE: usize = 1;
const LINE_STATUS: usize = 5;
const DATA_READY: u8 = 1;
const RECEIVED_DATA_INTERRUPT: u8 = 1;
const HARDIRQ_RX_BUDGET: usize = 64;

struct Uart16550 {
    base: usize,
    end: usize,
}

// OWNER: RISC-V platform owns the unique 16550 MMIO endpoint; generic driver owns only RX bytes.
static UART: Once<Uart16550> = Once::new();

struct UartInterruptHandler;

impl Uart16550 {
    fn read(&self, offset: usize) -> u8 {
        assert!(
            self.base + offset < self.end,
            "16550 register outside DTB range"
        );
        // SAFETY: offset is a bounded 16550 byte register inside the permanent DTB MMIO mapping.
        unsafe { core::ptr::read_volatile((self.base + offset) as *const u8) }
    }

    fn write(&self, offset: usize, value: u8) {
        assert!(
            self.base + offset < self.end,
            "16550 register outside DTB range"
        );
        // SAFETY: same bounded 16550 MMIO ownership as read; volatile preserves device writes.
        unsafe { core::ptr::write_volatile((self.base + offset) as *mut u8, value) };
    }
}

impl InterruptHandler for UartInterruptHandler {
    fn handle_interrupt(&self, _vector: InterruptVector) -> Result<(), InterruptError> {
        let uart = UART.wait();
        let mut bytes = [0u8; HARDIRQ_RX_BUDGET];
        let mut count = 0usize;
        while count < bytes.len() && uart.read(LINE_STATUS) & DATA_READY != 0 {
            bytes[count] = uart.read(RECEIVE_BUFFER);
            count += 1;
        }
        crate::drivers::publish_console_input(&bytes[..count]);
        Ok(())
    }
}

pub(super) fn initialize(
    base: usize,
    size: usize,
) -> Result<Arc<dyn InterruptHandler>, InterruptError> {
    let end = base
        .checked_add(size)
        .filter(|_| base != 0 && size > LINE_STATUS)
        .ok_or(InterruptError::InvalidVector)?;
    UART.call_once(|| Uart16550 { base, end });
    Arc::try_new(UartInterruptHandler)
        .map(|handler| handler as Arc<dyn InterruptHandler>)
        .map_err(|_| InterruptError::NoMemory)
}

pub(super) fn enable_receive() {
    let uart = UART.wait();
    let enabled = uart.read(INTERRUPT_ENABLE);
    uart.write(INTERRUPT_ENABLE, enabled | RECEIVED_DATA_INTERRUPT);
}
