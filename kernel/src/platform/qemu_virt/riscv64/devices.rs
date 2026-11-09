//! RISC-V QEMU virt 的 APLIC/IMSIC 与 16550 装配。
use super::{aia, discovery, uart};

/// 初始化全局控制器及 boot CPU 的文件，然后启用内核串口。
pub(crate) fn initialize() {
    aia::initialize();
    let board = discovery::info();
    let handler = uart::initialize(board.uart.start, board.uart.end - board.uart.start)
        .expect("16550 UART initialization failed");
    aia::bind_interrupt(board.uart_irq, handler, true, || {})
        .unwrap_or_else(|error| panic!("UART IRQ registration failed: {error}"));
    uart::enable_receive();
    crate::info!("AIA device initialization completed");
}
