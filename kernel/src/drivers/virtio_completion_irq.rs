//! @description VirtIO queue IRQ ack、transport failure publication 与 deferred routing policy。

use core::sync::atomic::{AtomicBool, Ordering};

use super::{VIRTIO_MMIO_INT_CONFIG, VIRTIO_MMIO_INT_VRING, VirtIODevice};

/// @description Hardirq 与 safe-point completion owner 之间的 transport error latch。
pub(super) struct VirtIoCompletionIrq(AtomicBool);

impl VirtIoCompletionIrq {
    pub(super) const fn new() -> Self {
        Self(AtomicBool::new(false))
    }

    /// @description 读取/ack transport status，并始终发布一次 deferred reclaim。
    ///
    /// OWNER: IRQ handler 只发布 error latch，safe point 以 swap 消费。若 status/ack error
    /// 被吞掉，PLIC 已消费的唯一 edge 后已提交 request 可能永久睡眠。
    pub(super) fn acknowledge_and_defer(&self, device: &VirtIODevice) {
        let failed = match device.interrupt_status() {
            Ok(status) => {
                let acknowledged = status & (VIRTIO_MMIO_INT_VRING | VIRTIO_MMIO_INT_CONFIG);
                acknowledged != 0 && device.interrupt_ack(acknowledged).is_err()
            }
            Err(_) => true,
        };
        if failed {
            self.0.store(true, Ordering::Release);
        }
        // Spurious/config vectors and failed reads still get one bounded safe-point pass.
        crate::cpu::raise_deferred(crate::cpu::DeferredWork::DriverIo);
    }

    /// @description safe point 原子消费 hardirq 观察到的 transport error。
    pub(super) fn take_transport_error(&self) -> bool {
        self.0.swap(false, Ordering::AcqRel)
    }
}
