//! AArch64 QEMU `virt` GICv3 与 PL011 静态装配，以及向设备绑定层暴露的 machine facts。

use alloc::sync::Arc;

use super::{discovery, gicv3, pl011};
use crate::execution::interrupt::InterruptHandler;
use crate::info;

pub(crate) fn initialize() {
    let platform = discovery::info();
    gicv3::initialize(platform.gic).expect("GICv3 initialization failed");
    super::its::initialize(platform.its);
    initialize_pl011();
    info!("AArch64 device initialization completed");
}

fn initialize_pl011() {
    let platform = discovery::info();
    let handler = pl011::initialize(platform.uart.base_addr, platform.uart.size)
        .expect("PL011 RX initialization failed");
    register_device_interrupt(platform.uart.irq, handler, "pl011");
    pl011::enable_receive();
}

/// 把设备 handler 注册到 GICv3 并路由到 boot CPU。
///
/// # Panics
///
/// controller 拒绝时带 label/vector/原因 fail-stop；静默返回会让设备永远收不到 interrupt。
pub(super) fn register_device_interrupt(
    vector: u32,
    handler: Arc<dyn InterruptHandler>,
    label: &'static str,
) {
    let affinity = crate::execution::cpu::CpuSet::singleton(crate::execution::cpu::boot_id());
    gicv3::register_device(vector, handler, affinity)
        .unwrap_or_else(|error| panic!("{label} IRQ {vector} registration failed: {error}"));
}
