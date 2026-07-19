//! @description 与具体 UART register ABI 无关的 console RX ring owner。

use alloc::collections::VecDeque;
use spin::Once;

use crate::sync::IrqMutex;

use super::InterruptError;

const RX_CAPACITY: usize = 1024;

struct UartState {
    rx: VecDeque<u8>,
}

// OWNER: generic UART domain uniquely owns the fixed-capacity console RX ring. Concrete platform
// handlers only publish already-drained bytes and never retain a second software queue.
static UART: Once<IrqMutex<UartState>> = Once::new();

/// @description 初始化唯一 console RX ring。
pub(super) fn init() -> Result<(), InterruptError> {
    let mut rx = VecDeque::new();
    rx.try_reserve_exact(RX_CAPACITY)
        .map_err(|_| InterruptError::NoMemory)?;
    UART.call_once(|| IrqMutex::new(UartState { rx }));
    Ok(())
}

/// @description 发布 concrete platform handler 已从 hardware FIFO drain 的 byte batch。
///
/// ring 满时丢弃 batch 尾部；hardware FIFO 已由 platform drain，因此不会维持 level IRQ。
pub(super) fn publish_received(bytes: &[u8]) {
    let mut uart = UART.wait().lock();
    let available = RX_CAPACITY.saturating_sub(uart.rx.len());
    uart.rx
        .extend(bytes[..bytes.len().min(available)].iter().copied());
}

/// @description 从唯一 UART RX ring 非阻塞读取已有输入。
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

pub(super) fn input_ready() -> bool {
    !UART.wait().lock().rx.is_empty()
}

pub(super) fn discard_input() -> usize {
    let mut uart = UART.wait().lock();
    let count = uart.rx.len();
    uart.rx.clear();
    count
}
