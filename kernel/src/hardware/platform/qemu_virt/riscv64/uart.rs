//! QEMU `virt` 16550 RX register adapter。

use alloc::sync::Arc;
use spin::Once;

use crate::hal::{InterruptError, InterruptHandler, InterruptVector, MmioBus};

const RECEIVE_BUFFER: usize = 0;
const INTERRUPT_ENABLE: usize = 1;
const LINE_STATUS: usize = 5;
const DATA_READY: u8 = 1;
const RECEIVED_DATA_INTERRUPT: u8 = 1;
const HARDIRQ_RX_BUDGET: usize = 64;

struct Uart16550(MmioBus);

// OWNER: RISC-V platform owns the unique 16550 MMIO endpoint; generic driver owns only RX bytes.
static UART: Once<Uart16550> = Once::new();

struct UartInterruptHandler;

impl Uart16550 {
    fn read(&self, offset: usize) -> u8 {
        self.0
            .read_u8(offset)
            .expect("16550 register outside DTB range")
    }

    fn write(&self, offset: usize, value: u8) {
        self.0
            .write_u8(offset, value)
            .expect("16550 register outside DTB range");
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
        crate::hal::console::publish_received(&bytes[..count]);
        Ok(())
    }
}

pub(super) fn initialize(
    base: usize,
    size: usize,
) -> Result<Arc<dyn InterruptHandler>, InterruptError> {
    if size <= LINE_STATUS {
        return Err(InterruptError::InvalidVector);
    }
    let bus = MmioBus::new(base, size).map_err(|_| InterruptError::InvalidVector)?;
    UART.call_once(|| Uart16550(bus));
    Arc::try_new(UartInterruptHandler)
        .map(|handler| handler as Arc<dyn InterruptHandler>)
        .map_err(|_| InterruptError::NoMemory)
}

pub(super) fn enable_receive() {
    let uart = UART.wait();
    let enabled = uart.read(INTERRUPT_ENABLE);
    uart.write(INTERRUPT_ENABLE, enabled | RECEIVED_DATA_INTERRUPT);
}
