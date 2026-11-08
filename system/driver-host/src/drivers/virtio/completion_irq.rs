//! VirtIO queue IRQ ack、transport failure publication 与 deferred routing policy。

use core::sync::atomic::{AtomicBool, Ordering};

use super::{VIRTIO_MMIO_INT_CONFIG, VIRTIO_MMIO_INT_VRING, VirtIODevice};

/// Hardirq 与 safe-point completion owner 之间的 transport error latch。
pub(super) struct VirtIOCompletionIrq(AtomicBool);

impl VirtIOCompletionIrq {
    pub(super) const fn new() -> Self {
        Self(AtomicBool::new(false))
    }

    /// 读取/ack transport status；失败锁存后返回 None，成功返回原始 status。
    ///
    /// OWNER: IRQ handler 只发布 error latch，完成 owner 以 swap 消费。调用者必须在 None 时
    /// 请求完成分派；block/RNG/sound 每次都分派，网络只在错误时分派，正常路径直接通知客户端。
    /// 若 status/ack error 被吞掉，控制器已消费的唯一 edge 后已提交 request 可能永久睡眠。
    pub(super) fn acknowledge(&self, device: &VirtIODevice) -> Option<u32> {
        let status = match device.interrupt_status() {
            Ok(status) => {
                let acknowledged = status & (VIRTIO_MMIO_INT_VRING | VIRTIO_MMIO_INT_CONFIG);
                if acknowledged == 0 || device.interrupt_ack(acknowledged).is_ok() {
                    Some(status)
                } else {
                    None
                }
            }
            Err(_) => None,
        };
        if status.is_none() {
            self.0.store(true, Ordering::Release);
        }
        status
    }

    /// 只观察错误锁存，避免正常完成分派进入设备的冷配置锁；不能代替 take 消费。
    pub(super) fn has_transport_error(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    /// safe point 原子消费 hardirq 观察到的 transport error。
    pub(super) fn take_transport_error(&self) -> bool {
        self.0.swap(false, Ordering::AcqRel)
    }
}
