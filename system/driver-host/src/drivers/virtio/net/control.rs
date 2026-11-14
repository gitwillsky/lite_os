//! VirtIO-net 控制队列；一个命令独占 DMA，ACK/used 完成后才能复用。
use super::super::queue::DmaSlice;
use super::{VirtIODevice, VirtIONetworkDevice, VirtQueue};
use alloc::vec::Vec;
use device_model::DeviceBacking;
use device_model::network::{NetworkError, NetworkRssCapabilities, NetworkRssConfiguration};
use native_runtime::native_abi::PAGE_SIZE;

/// 预先配置的单命令控制队列，拥有可重用的输入与 ACK DMA 存储。
pub(super) struct ControlQueue {
    index: u32,
    queue: VirtQueue,
    // OWNER: 动态大小、预先固定的连续存储；[header,payload] 是设备输入，尾字节是设备 ACK。
    // RSS 最长表可能超过一页；按真实能力分配，连续 backing 保证小控制队列也只需两个 descriptor。
    bytes: DeviceBacking,
}
impl ControlQueue {
    /// 按已经校验的 RSS 硬件上限准备控制存储；None 表示队列、连续页或 pin 无法建立。
    pub(super) fn try_new(
        device: &VirtIODevice,
        index: u32,
        rss: Option<NetworkRssCapabilities>,
    ) -> Option<Self> {
        let maximum_payload = rss.map_or(2, |cap| {
            let table = 1usize << cap.maximum_table_entries.ilog2();
            11 + table * 2 + usize::from(cap.maximum_key_bytes)
        });
        let bytes = DeviceBacking::try_allocate_contiguous(
            (2 + maximum_payload + 1).div_ceil(PAGE_SIZE),
            device.bti(),
        )?;
        bytes.pin(device.bti()).ok()?;
        Some(Self {
            index,
            queue: VirtIONetworkDevice::create_queue(device, index, None)?,
            bytes,
        })
    }
    /// VirtIO 1.4 §5.1.9.5.6：无显式 RSS 时采用标准自动分流。
    /// # Errors
    /// 命令失败后调用者必须 reset，不能复用未完成 DMA。
    pub(super) fn set_pairs(&mut self, device: &VirtIONetworkDevice, pairs: u16) -> Result<(), ()> {
        self.command(device, 0, &pairs.to_le_bytes())
    }
    /// 用标准 RSS_CONFIG 替换 Toeplitz 参数，并启用 pairs 个 TX 队列。
    /// config 已由提供者按队列数及硬件能力 validate；bootstrap 使用合法的 hash=0/table=[0]。
    /// 无法假定设备拒绝命令时仍保留旧配置，因此任何设备错误都必须 reset。
    /// # Errors
    /// 编码分配失败返回 NoMemory；设备拒绝、畸形完成或超时返回 Device，调用者必须 reset。
    pub(super) fn set_rss(
        &mut self,
        device: &VirtIONetworkDevice,
        pairs: u16,
        config: &NetworkRssConfiguration,
    ) -> Result<(), NetworkError> {
        let mut payload = Vec::new();
        payload
            .try_reserve_exact(11 + config.indirection.len() * 2 + config.key.len())
            .map_err(|_| NetworkError::NoMemory)?;
        payload.extend_from_slice(&config.hash_types.to_le_bytes());
        payload.extend_from_slice(&((config.indirection.len() - 1) as u16).to_le_bytes());
        payload.extend_from_slice(&config.unclassified_queue.to_le_bytes());
        for queue in &config.indirection {
            payload.extend_from_slice(&queue.to_le_bytes());
        }
        payload.extend_from_slice(&pairs.to_le_bytes());
        payload.push(config.key.len() as u8);
        payload.extend_from_slice(&config.key);
        self.command(device, 1, &payload)
            .map_err(|()| NetworkError::Device)
    }
    fn command(
        &mut self,
        device: &VirtIONetworkDevice,
        command: u8,
        payload: &[u8],
    ) -> Result<(), ()> {
        let length = 2 + payload.len();
        if length >= self.bytes.pages() * PAGE_SIZE {
            return Err(());
        }
        self.bytes.write(0, &[4, command]).map_err(|_| ())?;
        self.bytes.write(2, payload).map_err(|_| ())?;
        self.bytes.write(length, &[u8::MAX]).map_err(|_| ())?;
        let request = DmaSlice::backing(&self.bytes, 0..length, false).map_err(|_| ())?;
        let ack = DmaSlice::backing(&self.bytes, length..length + 1, true).map_err(|_| ())?;
        let head = self.queue.add_dma(&[request, ack]).map_err(|_| ())?;
        self.queue.add_to_avail(head);
        device
            .notify_available(self.index, &self.queue, 1)
            .map_err(|_| ())?;
        let deadline = native_runtime::system::deadline_after(1_000_000_000);
        loop {
            if let Some(completion) = self.queue.used()? {
                if completion.head() != head || completion.length() != 1 {
                    warn!(
                        "VirtIO-net control {} returned malformed completion",
                        command
                    );
                    return Err(());
                }
                self.queue.recycle_used(completion)?;
                let mut ack = [u8::MAX];
                self.bytes.read(length, &mut ack).map_err(|_| ())?;
                if ack[0] != 0 {
                    warn!("VirtIO-net rejected control {}: ACK {}", command, ack[0]);
                }
                return (ack[0] == 0).then_some(()).ok_or(());
            }
            if native_runtime::system::monotonic() >= deadline {
                warn!("VirtIO-net control {} timed out", command);
                return Err(());
            }
            // 冷控制等待不占数据队列锁；used/ACK 由 DMA 推进，不依赖 IRQ 服务线程。
            native_runtime::system::sleep_until(native_runtime::system::deadline_after(50_000))
                .map_err(|_| ())?;
        }
    }
}
