//! VirtIO-net 控制队列；一次只有一个命令 owner，ACK 与 descriptor 都完成后才允许复用 DMA。
use super::{DmaBuffer, VirtIODevice, VirtIONetworkDevice, VirtQueue};

/// 一个已配置控制 virtqueue 及其唯一请求/ACK DMA owner。
pub(super) struct ControlQueue {
    index: u32,
    queue: VirtQueue,
    // OWNER: [0..4] 是 class/command/pairs，byte 4 是 device-written ACK；used 前 CPU 不读取 ACK。
    bytes: DmaBuffer<8>,
}

impl ControlQueue {
    /// 为 device 的指定控制 queue index 准备 ring 与命令存储。
    /// # Returns
    /// 成功返回尚未提交命令的 owner；queue 不存在、配置或 DMA 分配失败时返回 None。
    pub(super) fn try_new(device: &VirtIODevice, index: u32) -> Option<Self> {
        Some(Self {
            index,
            queue: VirtIONetworkDevice::create_queue(device, index)?,
            bytes: DmaBuffer::try_zeroed(device.bti()).ok()?,
        })
    }

    /// VirtIO 1.4 §5.1.9.5.6：配置好的 queue pairs 经 ACK 后才可收发。
    /// # Parameters
    /// device 是该队列的 transport，pairs 已由调用者校验为已配置且不超过设备上限的活动 pair 数。
    /// # Returns
    /// 设备确认命令并归还 descriptor 后返回 Ok。
    /// # Errors
    /// 构造失败、错误/畸形 ACK 或设备一秒内不完成命令时返回 Err；调用者必须 reset，不能复用未完成 DMA。
    pub(super) fn set_pairs(&mut self, device: &VirtIODevice, pairs: u16) -> Result<(), ()> {
        let bytes = self.bytes.as_mut_slice();
        bytes[..2].copy_from_slice(&[4, 0]);
        bytes[2..4].copy_from_slice(&pairs.to_le_bytes());
        bytes[4] = u8::MAX;
        let request = self.bytes.readable(0..4).map_err(|_| ())?;
        let ack = self.bytes.writable(4..5).map_err(|_| ())?;
        let head = self.queue.add_dma(&[request, ack]).map_err(|_| ())?;
        self.queue.add_to_avail(head);
        device.notify_queue(self.index).map_err(|_| ())?;
        let deadline = native_runtime::system::deadline_after(1_000_000_000);
        loop {
            if let Some(completion) = self.queue.used()? {
                if completion.head() != head || completion.length() != 1 {
                    warn!(
                        "VirtIO-net MQ control completion invalid: head {} expected {}, length {}",
                        completion.head(),
                        head,
                        completion.length()
                    );
                    return Err(());
                }
                self.queue.recycle_used(completion)?;
                let ack = self.bytes.as_ref()[4];
                if ack != 0 {
                    warn!("VirtIO-net rejected {} queue pairs: ACK {}", pairs, ack);
                }
                return (ack == 0).then_some(()).ok_or(());
            }
            if native_runtime::system::monotonic() >= deadline {
                warn!("VirtIO-net MQ control timed out for {} queue pairs", pairs);
                return Err(());
            }
            // 冷初始化等待不忙轮询；IRQ 尚未注册，设备 DMA 与 timer 仍独立推进。
            native_runtime::system::sleep_until(native_runtime::system::deadline_after(50_000))
                .map_err(|_| ())?;
        }
    }
}
