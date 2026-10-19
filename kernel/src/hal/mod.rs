//! 硬件抽象与 platform/driver 共用的最小 seam：MMIO 总线、设备中断接口与串口 console。
//!
//! console RX ring 与只追加 registry 是共用 seam 的支撑机制，不是 ISA 或 machine 抽象；
//! 留在本层以保持同一状态 owner，并避免 platform 反向依赖 driver。
//!
//! 位于 `platform` 与 `drivers` 之下：platform 的中断控制器与 UART 只依赖这里的接口，驱动实现
//! 同一组接口；本 module 只依赖 `arch` 与 `sync`，因此 platform 不需要认识任何设备类或 DMA 内存。

mod bus;
pub(crate) mod console;
mod interrupt;
pub(crate) mod registry;

pub(crate) use bus::{BusError, MmioBus, before_mmio_write};
pub(crate) use interrupt::{
    InterruptError, InterruptHandler, InterruptVector, wait_for_external_interrupt,
};
