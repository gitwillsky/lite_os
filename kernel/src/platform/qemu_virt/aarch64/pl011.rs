//! @description QEMU `virt` PL011 RX register adapter。

use alloc::sync::Arc;
use spin::Once;

use crate::drivers::{InterruptError, InterruptHandler, InterruptVector};

const DATA: usize = 0x00;
const FLAGS: usize = 0x18;
const LINE_CONTROL: usize = 0x2c;
const INTERRUPT_FIFO_LEVEL: usize = 0x34;
const INTERRUPT_MASK: usize = 0x38;
const INTERRUPT_CLEAR: usize = 0x44;
const RX_FIFO_EMPTY: u32 = 1 << 4;
const FIFO_ENABLE: u32 = 1 << 4;
const RX_FIFO_LEVEL_MASK: u32 = 0b111 << 3;
const RX_INTERRUPT: u32 = (1 << 4) | (1 << 6);
const HARDIRQ_RX_BUDGET: usize = 64;

struct Pl011 {
    base: usize,
    end: usize,
}

// OWNER: AArch64 platform owns the unique PL011 MMIO endpoint; generic driver owns only RX bytes.
static UART: Once<Pl011> = Once::new();

struct Pl011InterruptHandler;

impl Pl011 {
    fn read(&self, offset: usize) -> u32 {
        let address = self
            .base
            .checked_add(offset)
            .expect("PL011 address overflow");
        assert!(
            address + core::mem::size_of::<u32>() <= self.end,
            "PL011 register outside DTB range"
        );
        // SAFETY: address is an aligned PL011 register inside the permanent DTB MMIO mapping.
        unsafe { core::ptr::read_volatile(address as *const u32) }
    }

    fn write(&self, offset: usize, value: u32) {
        let address = self
            .base
            .checked_add(offset)
            .expect("PL011 address overflow");
        assert!(
            address + core::mem::size_of::<u32>() <= self.end,
            "PL011 register outside DTB range"
        );
        // SAFETY: same bounded PL011 MMIO ownership as read; volatile preserves device writes.
        unsafe { core::ptr::write_volatile(address as *mut u32, value) };
    }
}

impl InterruptHandler for Pl011InterruptHandler {
    fn handle_interrupt(&self, _vector: InterruptVector) -> Result<(), InterruptError> {
        let uart = UART.wait();
        let mut bytes = [0u8; HARDIRQ_RX_BUDGET];
        let mut count = 0usize;
        while count < bytes.len() && uart.read(FLAGS) & RX_FIFO_EMPTY == 0 {
            bytes[count] = uart.read(DATA) as u8;
            count += 1;
        }
        uart.write(INTERRUPT_CLEAR, RX_INTERRUPT);
        crate::drivers::publish_console_input(&bytes[..count]);
        Ok(())
    }
}

pub(super) fn initialize(
    base: usize,
    size: usize,
) -> Result<Arc<dyn InterruptHandler>, InterruptError> {
    let base = crate::arch::mmu::physical_to_virtual(base);
    let end = base
        .checked_add(size)
        .filter(|_| base != 0 && size >= INTERRUPT_CLEAR + core::mem::size_of::<u32>())
        .ok_or(InterruptError::InvalidVector)?;
    UART.call_once(|| Pl011 { base, end });
    Arc::try_new(Pl011InterruptHandler)
        .map(|handler| handler as Arc<dyn InterruptHandler>)
        .map_err(|_| InterruptError::NoMemory)
}

pub(super) fn enable_receive() {
    let uart = UART.wait();
    // PL011 resets in one-byte character mode. QEMU stdio has no hardware flow control, so a
    // multi-byte host write can overrun that holding register while another device IRQ runs.
    // Enable the architected 16-byte FIFO and select its lowest RX threshold before unmasking
    // receive/timeout IRQs; the hardirq still drains every available byte in one bounded pass.
    let line_control = uart.read(LINE_CONTROL);
    uart.write(LINE_CONTROL, line_control | FIFO_ENABLE);
    let fifo_level = uart.read(INTERRUPT_FIFO_LEVEL);
    uart.write(INTERRUPT_FIFO_LEVEL, fifo_level & !RX_FIFO_LEVEL_MASK);
    uart.write(INTERRUPT_CLEAR, u32::MAX);
    let enabled = uart.read(INTERRUPT_MASK);
    uart.write(INTERRUPT_MASK, enabled | RX_INTERRUPT);
}
