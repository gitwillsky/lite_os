//! 与具体 UART register ABI 无关的 console RX ring owner。

use alloc::collections::VecDeque;
use spin::Once;

use crate::sync::IrqMutex;

use super::InterruptError;

const RX_CAPACITY: usize = 1024;

struct ConsoleInput {
    rx: VecDeque<u8>,
}

// OWNER: generic console input domain uniquely owns the fixed-capacity console RX ring. Concrete platform
// handlers only publish already-drained bytes and never retain a second software queue.
static CONSOLE_INPUT: Once<IrqMutex<ConsoleInput>> = Once::new();

/// 初始化唯一 console RX ring。
pub(super) fn init() -> Result<(), InterruptError> {
    let mut rx = VecDeque::new();
    rx.try_reserve_exact(RX_CAPACITY)
        .map_err(|_| InterruptError::NoMemory)?;
    CONSOLE_INPUT.call_once(|| IrqMutex::new(ConsoleInput { rx }));
    Ok(())
}

/// 发布 concrete platform handler 已从 hardware FIFO drain 的 byte batch。
///
/// ring 满时丢弃 batch 尾部；hardware FIFO 已由 platform drain，因此不会维持 level IRQ。
pub(super) fn publish_received(bytes: &[u8]) {
    let mut input = CONSOLE_INPUT.wait().lock();
    let available = RX_CAPACITY.saturating_sub(input.rx.len());
    input
        .rx
        .extend(bytes[..bytes.len().min(available)].iter().copied());
}

/// 从唯一 console RX ring 非阻塞读取已有输入。
pub(super) fn read(bytes: &mut [u8]) -> usize {
    let mut input = CONSOLE_INPUT.wait().lock();
    let count = bytes.len().min(input.rx.len());
    for byte in &mut bytes[..count] {
        *byte = input
            .rx
            .pop_front()
            .expect("console RX length changed under lock");
    }
    count
}

pub(super) fn input_ready() -> bool {
    !CONSOLE_INPUT.wait().lock().rx.is_empty()
}

pub(super) fn discard_input() -> usize {
    let mut input = CONSOLE_INPUT.wait().lock();
    let count = input.rx.len();
    input.rx.clear();
    count
}
