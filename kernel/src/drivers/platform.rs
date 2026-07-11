use alloc::boxed::Box;

use crate::arch::dtb::{BoardInfo, board_info};
use crate::drivers::block::register_block_device;
use crate::drivers::{
    InterruptController, InterruptHandler, MmioBus, PlicInterruptController, VirtIOBlockDevice,
};
use crate::sync::IrqMutex;

/// PLIC 是外部中断的唯一权威控制器，不经过通用设备 registry。
// OWNER: platform layer owns the unique interrupt controller discovered from DTB.
static INTERRUPT_CONTROLLER: spin::Once<IrqMutex<Box<dyn InterruptController>>> = spin::Once::new();

fn interrupt_controller() -> Option<&'static IrqMutex<Box<dyn InterruptController>>> {
    INTERRUPT_CONTROLLER.get()
}

fn init_interrupt_controller() {
    let Some(plic) = board_info().plic_device.as_ref() else {
        return;
    };
    match PlicInterruptController::new(
        plic.base_addr,
        plic.size,
        1024,
        crate::arch::hart::possible_hart_mask(),
        crate::arch::hart::max_hart_id(),
    ) {
        Ok(controller) => {
            INTERRUPT_CONTROLLER
                .call_once(|| IrqMutex::new(Box::new(controller) as Box<dyn InterruptController>));
        }
        Err(error) => error!("[Platform] PLIC initialization failed: {:?}", error),
    }
}

/// 系统初始化入口点
pub(super) fn init() {
    init_interrupt_controller();
    init_uart_console();
    // 扫描和初始化设备
    scan_and_init_devices();
    info!("[Platform] Device initialization completed");
}

/// 扫描并初始化所有设备
fn scan_and_init_devices() {
    let board_info = board_info();

    // 初始化VirtIO设备
    init_virtio_devices(board_info);
}

/// 初始化VirtIO设备
fn init_virtio_devices(board_info: &BoardInfo) {
    info!(
        "[Platform] Scanning {} VirtIO devices",
        board_info.virtio_count
    );
    info!("[Platform] Board info debug:\n{}", board_info);

    for i in 0..board_info.virtio_count {
        if let Some(virtio_dev) = &board_info.virtio_devices[i] {
            let base_addr = virtio_dev.base_addr;
            info!(
                "[Platform] Attempting to probe VirtIO device {} at {:#x}, size={:#x}",
                i, base_addr, virtio_dev.size
            );
            info!(
                "[Platform] Processing VirtIO device {}/{}",
                i + 1,
                board_info.virtio_count
            );

            let Some(device_id) = read_virtio_device_id(base_addr, virtio_dev.size) else {
                warn!("[Platform] Invalid VirtIO MMIO window at {:#x}", base_addr);
                continue;
            };
            info!(
                "[Platform] VirtIO device {} has device ID: {:#x}",
                i, device_id
            );

            match device_id {
                2 => init_virtio_blk_device(board_info, virtio_dev.irq, base_addr),
                _ => info!(
                    "[Platform] Unrecognized VirtIO device ID {:#x} at {:#x}",
                    device_id, base_addr
                ),
            }
        }
    }
}

#[inline]
fn read_virtio_device_id(base_addr: usize, size: usize) -> Option<u32> {
    MmioBus::new(base_addr, size).ok()?.read_u32(0x08).ok()
}

fn maybe_register_irq(
    board_info: &BoardInfo,
    irq: u32,
    handler: alloc::sync::Arc<dyn InterruptHandler>,
    label: &str,
) -> bool {
    if board_info.plic_device.is_none() || irq == 0 {
        return false;
    }

    if let Some(controller) = interrupt_controller() {
        let mut ctrl = controller.lock();
        let res = if let Err(e) = ctrl.register_handler(irq, handler.clone()) {
            error!(
                "[Platform] Failed to register {} IRQ handler: {:?}",
                label, e
            );
            Err(())
        } else if let Err(e) = ctrl.set_priority(irq) {
            error!("[Platform] Failed to set {} IRQ priority: {:?}", label, e);
            Err(())
        } else if ctrl.supports_cpu_affinity() {
            let boot_hart = crate::arch::hart::boot_hart_id();
            if let Err(e) = ctrl.set_affinity(irq, 1usize << boot_hart) {
                warn!("[Platform] Failed to set {} IRQ affinity: {:?}", label, e);
            } else {
                info!(
                    "[Platform] Set {} IRQ affinity to boot hart {}",
                    label, boot_hart
                );
            }
            if let Err(e) = ctrl.enable_interrupt(irq) {
                error!("[Platform] Failed to enable {} IRQ {}: {:?}", label, irq, e);
                Err(())
            } else {
                info!(
                    "[Platform] Registered {} IRQ handler on vector {}",
                    label, irq
                );
                Ok(())
            }
        } else if let Err(e) = ctrl.enable_interrupt(irq) {
            error!("[Platform] Failed to enable {} IRQ {}: {:?}", label, irq, e);
            Err(())
        } else {
            info!(
                "[Platform] Registered {} IRQ handler on vector {}",
                label, irq
            );
            Ok(())
        };
        drop(ctrl);
        return res.is_ok();
    }
    false
}

fn init_uart_console() {
    let board = board_info();
    let size = board.uart.end.saturating_sub(board.uart.start);
    let handler =
        super::uart::init(board.uart.start, size).expect("boot requires a valid DTB UART console");
    assert!(
        maybe_register_irq(board, board.uart_irq, handler, "uart"),
        "boot requires a registered UART IRQ"
    );
    super::uart::enable_receive_interrupt();
}

fn init_virtio_blk_device(board_info: &BoardInfo, irq: u32, base_addr: usize) {
    info!("[Platform] Creating VirtIOBlockDevice at {:#x}", base_addr);
    if let Some(virtio_block) = VirtIOBlockDevice::new(base_addr) {
        let virtio_arc = virtio_block.clone();
        match register_block_device(virtio_arc.clone()) {
            Ok(device_id) => {
                info!(
                    "[Platform] VirtIO Block device #{} registered at {:#x}",
                    device_id, base_addr
                );
            }
            Err(e) => {
                error!("[Platform] Failed to register block device: {:?}", e);
            }
        }
        let _ = maybe_register_irq(board_info, irq, virtio_block.irq_handler_for(), "blk");
    } else {
        warn!(
            "[Platform] Failed to create VirtIO Block device at {:#x}",
            base_addr
        );
    }
}

/// 处理外部中断
pub(super) fn handle_external_interrupt() {
    // 先短暂获取控制器引用，再释放设备管理器锁，避免在中断回调中重入造成死锁
    if let Some(controller) = interrupt_controller() {
        let result = controller.lock().handle_pending_interrupts();
        if let Err(e) = result {
            #[cfg(debug_assertions)]
            debug!("[Platform] Interrupt handling failed: {:?}", e);
        }
    }
}
