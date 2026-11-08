//! QEMU `virt` PL011 RX register adapter。

use alloc::sync::Arc;
use mmio::MmioBus;
use spin::Once;

use crate::{
    execution::interrupt::InterruptError, execution::interrupt::InterruptHandler,
    execution::interrupt::InterruptVector,
};

const DATA: usize = 0x00;
const FLAGS: usize = 0x18;
const LINE_CONTROL: usize = 0x2c;
const INTERRUPT_FIFO_LEVEL: usize = 0x34;
const INTERRUPT_MASK: usize = 0x38;
const INTERRUPT_CLEAR: usize = 0x44;
const RX_FIFO_EMPTY: u32 = 1 << 4;
const FIFO_ENABLE: u32 = 1 << 4;
const RX_FIFO_LEVEL_MASK: u32 = 0b111 << 3;
const RX_TIMEOUT_INTERRUPT: u32 = 1 << 6;
const RX_INTERRUPT: u32 = (1 << 4) | RX_TIMEOUT_INTERRUPT;
const HARDIRQ_RX_BUDGET: usize = 64;

struct Pl011(MmioBus);

// OWNER: AArch64 platform owns the unique PL011 MMIO endpoint; generic driver owns only RX bytes.
static UART: Once<Pl011> = Once::new();

struct Pl011InterruptHandler;

impl Pl011 {
    fn read(&self, offset: usize) -> u32 {
        self.0
            .read_u32(offset)
            .expect("PL011 register outside DTB range")
    }

    fn write(&self, offset: usize, value: u32) {
        self.0
            .write_u32(offset, value)
            .expect("PL011 register outside DTB range");
    }
}

impl InterruptHandler for Pl011InterruptHandler {
    fn handle_interrupt(&self, _vector: InterruptVector) -> Result<(), InterruptError> {
        let uart = UART.wait();
        // 1. 只确认 timeout；RX 电平由 DATA 读取使 FIFO 降到阈值以下时自动撤销。
        // 2. 每次读 DATA 都可能让宿主继续填 FIFO。预算耗尽时必须保留 RX 电平，否则 FIFO
        //    仍满却没有新的阈值跳变，剩余输入永久滞留；EOI 后由电平再次触发下一批读取。
        // 3. timeout 在读取前确认，避免读空后到达的字节所产生的新 timeout 被一并清掉。
        uart.write(INTERRUPT_CLEAR, RX_TIMEOUT_INTERRUPT);
        let mut bytes = [0u8; HARDIRQ_RX_BUDGET];
        let mut count = 0usize;
        while count < bytes.len() && uart.read(FLAGS) & RX_FIFO_EMPTY == 0 {
            bytes[count] = uart.read(DATA) as u8;
            count += 1;
        }
        crate::serial::publish_received(&bytes[..count]);
        Ok(())
    }
}

pub(super) fn initialize(
    base: usize,
    size: usize,
) -> Result<Arc<dyn InterruptHandler>, InterruptError> {
    if size < INTERRUPT_CLEAR + core::mem::size_of::<u32>() {
        return Err(InterruptError::InvalidVector);
    }
    let bus = MmioBus::new(crate::arch::mmu::physical_to_virtual(base), size)
        .map_err(|_| InterruptError::InvalidVector)?;
    UART.call_once(|| Pl011(bus));
    Arc::try_new(Pl011InterruptHandler)
        .map(|handler| handler as Arc<dyn InterruptHandler>)
        .map_err(|_| InterruptError::NoMemory)
}

pub(super) fn enable_receive() {
    let uart = UART.wait();
    // PL011 resets in one-byte character mode. QEMU stdio has no hardware flow control, so a
    // multi-byte host write can overrun that holding register while another device IRQ runs.
    // Enable the architected 16-byte FIFO and select its lowest RX threshold before unmasking
    // receive/timeout IRQs; a nonempty FIFO retains RX across bounded hardirq passes.
    let line_control = uart.read(LINE_CONTROL);
    uart.write(LINE_CONTROL, line_control | FIFO_ENABLE);
    let fifo_level = uart.read(INTERRUPT_FIFO_LEVEL);
    uart.write(INTERRUPT_FIFO_LEVEL, fifo_level & !RX_FIFO_LEVEL_MASK);
    uart.write(INTERRUPT_CLEAR, u32::MAX);
    let enabled = uart.read(INTERRUPT_MASK);
    uart.write(INTERRUPT_MASK, enabled | RX_INTERRUPT);
}
