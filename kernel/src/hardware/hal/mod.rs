//! Hardware abstraction：MMIO 总线、设备中断接口、串口 console 与只追加设备注册表。
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
