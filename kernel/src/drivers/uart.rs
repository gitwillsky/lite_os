use alloc::{collections::VecDeque, sync::Arc};
use spin::Once;

use crate::sync::IrqMutex;

use super::{InterruptError, InterruptHandler, InterruptVector};

const RX_CAPACITY: usize = 1024;
const RECEIVE_BUFFER: usize = 0;
const INTERRUPT_ENABLE: usize = 1;
const LINE_STATUS: usize = 5;
const DATA_READY: u8 = 1;
const RECEIVED_DATA_INTERRUPT: u8 = 1;

struct UartState {
    base: usize,
    end: usize,
    rx: VecDeque<u8>,
}

// OWNER: UART driver uniquely owns the DTB MMIO endpoint and fixed-capacity RX ring.
static UART: Once<IrqMutex<UartState>> = Once::new();

struct UartInterruptHandler;

impl UartState {
    fn read_register(&self, offset: usize) -> u8 {
        assert!(
            self.base + offset < self.end,
            "UART register outside DTB range"
        );
        // SAFETY: offset is one of the bounded 16550 byte registers and the DTB MMIO range is
        // permanently identity-mapped; volatile preserves device access semantics.
        unsafe { core::ptr::read_volatile((self.base + offset) as *const u8) }
    }

    fn write_register(&self, offset: usize, value: u8) {
        assert!(
            self.base + offset < self.end,
            "UART register outside DTB range"
        );
        // SAFETY: same bounded permanent MMIO mapping as read_register; volatile prevents elision.
        unsafe { core::ptr::write_volatile((self.base + offset) as *mut u8, value) }
    }

    fn drain_receive_fifo(&mut self) {
        while self.read_register(LINE_STATUS) & DATA_READY != 0 {
            let byte = self.read_register(RECEIVE_BUFFER);
            if self.rx.len() < RX_CAPACITY {
                self.rx.push_back(byte);
            }
            // 满 ring 仍必须读取 RBR 以撤销 level IRQ；否则 interrupt controller 会持续重入。
        }
    }
}

impl InterruptHandler for UartInterruptHandler {
    fn handle_interrupt(&self, _vector: InterruptVector) -> Result<(), InterruptError> {
        UART.wait().lock().drain_receive_fifo();
        Ok(())
    }
}

/// @description 初始化 platform UART RX ring，并返回唯一 external-interrupt handler。
///
/// @param base DTB UART MMIO 起始地址。
/// @param size DTB UART MMIO 长度，必须覆盖 16550 前六个 byte register。
/// @return 成功返回 IRQ handler；范围或 ring 分配失败返回 `InvalidVector`。
pub(super) fn init(base: usize, size: usize) -> Result<Arc<dyn InterruptHandler>, InterruptError> {
    let end = base
        .checked_add(size)
        .filter(|_| base != 0 && size > LINE_STATUS)
        .ok_or(InterruptError::InvalidVector)?;
    let mut rx = VecDeque::new();
    rx.try_reserve_exact(RX_CAPACITY)
        .map_err(|_| InterruptError::InvalidVector)?;
    UART.call_once(|| IrqMutex::new(UartState { base, end, rx }));
    Arc::try_new(UartInterruptHandler)
        .map(|handler| handler as Arc<dyn InterruptHandler>)
        .map_err(|_| InterruptError::NoMemory)
}

/// @description 在 external handler/affinity 发布后使能 16550 received-data interrupt。
///
/// @return 无返回值；UART 尚未初始化表示启动不变量损坏并 fail-stop。
pub(super) fn enable_receive_interrupt() {
    let uart = UART.wait().lock();
    let enabled = uart.read_register(INTERRUPT_ENABLE);
    uart.write_register(INTERRUPT_ENABLE, enabled | RECEIVED_DATA_INTERRUPT);
}

/// @description 从 IRQ ring 非阻塞读取已有 console bytes。
///
/// @param bytes kernel-owned 输出缓冲区。
/// @return 当前可取得的字节数；零表示调用方必须进入统一 console wait。
pub(super) fn read(bytes: &mut [u8]) -> usize {
    let mut uart = UART.wait().lock();
    let count = bytes.len().min(uart.rx.len());
    for byte in &mut bytes[..count] {
        *byte = uart
            .rx
            .pop_front()
            .expect("UART RX length changed under lock");
    }
    count
}

/// @description 查询 RX ring 是否已有输入，供 wait owner lock 内封闭 read/enqueue race。
///
/// @return 至少一个 byte 可读时返回 true。
pub(super) fn input_ready() -> bool {
    !UART.wait().lock().rx.is_empty()
}
