//! 有寿命的共享帧借用；不逐包分配或增减 Arc，不把 VMO 映射存在当作字节所有权。
use super::{NetworkChecksum, NetworkDevice, NetworkError, NetworkTcpSegmentation};

/// 唯一的 CPU RX 借用；释放或 Drop 后才能把该槽重新交给设备。
pub struct NetworkReceiveFrame<'a> {
    device: &'a dyn NetworkDevice,
    queue: u16,
    identity: Option<(u16, u64)>,
    address: usize,
    length: usize,
    checksum_valid: bool,
}
impl<'a> NetworkReceiveFrame<'a> {
    /// 从已经领取的槽建立借用；device 保活映射，slot/ticket 标识本次唯一所有权。
    /// # Safety
    /// address 非零，范围不超过 isize::MAX 且不跨映射边界，在 device 寿命内有效且已初始化；设备/其他 CPU 在 release_received
    /// 前不能写入或复用该范围。不能为同一所有权构造两个 token。
    /// checksum_valid 只能在设备已验证 transport checksum 或驱动补全后为 true；否则上层会跳过必要校验。
    pub unsafe fn new(
        device: &'a dyn NetworkDevice,
        queue: u16,
        slot: u16,
        ticket: u64,
        address: usize,
        length: usize,
        checksum_valid: bool,
    ) -> Self {
        Self {
            device,
            queue,
            identity: Some((slot, ticket)),
            address,
            length,
            checksum_valid,
        }
    }
    /// transport checksum 已由设备验证或在交付前补全；不涵盖网络头校验。
    pub fn checksum_valid(&self) -> bool {
        self.checksum_valid
    }
    /// 显式归还 RX，以便调用者记录错误；无论成功与否不再访问内容或重复归还。
    /// # Errors
    /// 协议或设备失败由实现返回；失败必须终止会话，不能重复归还同一 ticket。
    pub fn release(mut self) -> Result<(), NetworkError> {
        let (slot, ticket) = self.identity.take().expect("receive frame released twice");
        // SAFETY: 消费 self，所有从 Deref 借出的引用均已结束；本 token 唯一拥有该身份。
        unsafe { self.device.release_received(self.queue, slot, ticket) }
    }
}
impl core::ops::Deref for NetworkReceiveFrame<'_> {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        // SAFETY: 构造者保证映射/初始化与排他的设备→CPU 转移；借用止于 token，Drop 后不可访问。
        unsafe { core::slice::from_raw_parts(self.address as *const u8, self.length) }
    }
}
impl Drop for NetworkReceiveFrame<'_> {
    fn drop(&mut self) {
        if let Some((slot, ticket)) = self.identity.take() {
            // SAFETY: Drop 时没有仍可用的内容引用，本 token 不可复制且尚未交出 ownership。
            let _ = unsafe { self.device.release_received(self.queue, slot, ticket) };
        }
    }
}

/// 唯一的 CPU TX 借用；写入直接落到共享槽，提交成功后只能由真实 CQ 归还。
pub struct NetworkTransmit<'a> {
    device: &'a dyn NetworkDevice,
    queue: u16,
    identity: Option<(u16, u64)>,
    address: usize,
    capacity: usize,
}
impl<'a> NetworkTransmit<'a> {
    /// 从已预留的槽建立可写借用，capacity 是可用 payload 大小（不包含设备 header）。
    /// # Safety
    /// address 非零，范围不超过 isize::MAX 且不跨映射边界，在 device 寿命内有效、可写且已初始化；本 token 唯一持有该范围，
    /// submit/cancel 前没有设备或其他 CPU 读写。不能为同一 reservation 构造两个 token。
    pub unsafe fn new(
        device: &'a dyn NetworkDevice,
        queue: u16,
        slot: u16,
        ticket: u64,
        address: usize,
        capacity: usize,
    ) -> Self {
        Self {
            device,
            queue,
            identity: Some((slot, ticket)),
            address,
            capacity,
        }
    }
    /// 可写 payload 容量。
    pub fn capacity(&self) -> usize {
        self.capacity
    }
    /// 借出 CPU 写入区；引用的寿命只覆盖本次 &mut self，不能跨越消费 self 的 submit。
    pub fn buffer_mut(&mut self) -> &mut [u8] {
        // SAFETY: 构造者的排他范围证明由 token 保持；&mut self 阻止同时创建另一个可写引用。
        unsafe { core::slice::from_raw_parts_mut(self.address as *mut u8, self.capacity) }
    }
    /// 交出前 length 字节；本方法消费 token，不复制 payload。
    /// # Errors
    /// 长度越界、协议/设备失败时返回错误；未发表的 reservation 由 Drop 恰好取消一次。
    pub fn submit(
        mut self,
        length: usize,
        checksum: Option<NetworkChecksum>,
        segmentation: Option<NetworkTcpSegmentation>,
    ) -> Result<(), NetworkError> {
        if length == 0 || length > self.capacity {
            return Err(NetworkError::FrameTooLarge);
        }
        self.device
            .transmit_capabilities()
            .validate_packet(length, checksum, segmentation)?;
        let (slot, ticket) = self.identity.expect("transmit frame consumed twice");
        // SAFETY: 消费 self 后所有 buffer_mut 的引用均已结束，唯一 token 交出本次 reservation。
        unsafe {
            self.device
                .submit_transmit(self.queue, slot, ticket, length, checksum, segmentation)
        }?;
        self.identity.take();
        Ok(())
    }
}
impl Drop for NetworkTransmit<'_> {
    fn drop(&mut self) {
        if let Some((slot, ticket)) = self.identity.take() {
            // SAFETY: token 尚未交出 ownership，且没有仍可用的 payload 引用；失败提交由实现区分 Reserved/Submitted。
            unsafe { self.device.cancel_transmit(self.queue, slot, ticket) };
        }
    }
}
