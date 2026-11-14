//! 有寿命的共享帧借用；不逐包分配或增减 Arc，不把 VMO 映射存在当作字节所有权。
use super::{NetworkChecksum, NetworkDevice, NetworkError, NetworkTcpSegmentation};
use alloc::sync::Arc;
use packet_wire::batch::{IngressIp, NetworkPacket};

/// 同一原始接收帧的传输层校验事实。协议仍检查头部结构与接收资格。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ReceiveChecksum {
    /// 未检查或需要解释扩展头/重组。
    Unchecked,
    /// 由设备验证或补全，保留现有硬件卸载语义。
    Device,
    /// 接收队列用软件验证通过。
    Valid,
    /// 接收队列已验证，发现错误。
    Invalid,
}

impl ReceiveChecksum {
    /// 本帧的传输层 checksum 已知有效。
    pub fn verified(self) -> bool {
        matches!(self, Self::Device | Self::Valid)
    }
}

/// 消费者为本帧请求的接收准备工作。标注和校验结果均留在客户端。
#[derive(Default)]
pub struct ReceivePreparation {
    /// 消费者根据该帧生成的随帧值，默认全零。设备不解释这些值。
    pub annotation: [u64; 10],
    /// 设备未提供有效校验时，是否校验可直接定位的传输层数据。
    pub verify_transport: bool,
}

/// 已由设备队列检查的 RX 缓冲区描述。只持地址和结构事实，不能借出报文字节。
pub struct NetworkReceiveBuffer {
    address: usize,
    length: usize,
    checksum: ReceiveChecksum,
    ip: Option<IngressIp>,
    annotation: [u64; 10],
}

impl NetworkReceiveBuffer {
    /// 在持有 RX 槽期间检查帧并记录结构事实。格式错误保留原帧给后续入口处理。
    /// 调用方须确认硬件实际校验过传输层，才可将 checksum_valid 置为 true。
    pub fn inspect(bytes: &[u8], checksum_valid: bool) -> Self {
        Self::prepare(bytes, checksum_valid, |_| ReceivePreparation::default())
    }

    /// 检查结构并在同一个 RX 槽内生成标注与可选校验事实。`checksum_valid` 仍只由设备确认。
    pub fn prepare(
        bytes: &[u8],
        checksum_valid: bool,
        prepare: impl FnOnce(NetworkPacket<'_>) -> ReceivePreparation,
    ) -> Self {
        let packet = NetworkPacket::direct_ip(bytes);
        let ip = packet.and_then(NetworkPacket::ingress_ip);
        let preparation = packet.map_or_else(ReceivePreparation::default, prepare);
        let checksum = if checksum_valid {
            ReceiveChecksum::Device
        } else if preparation.verify_transport {
            match packet.and_then(NetworkPacket::verify_direct_transport) {
                Some(true) => ReceiveChecksum::Valid,
                Some(false) => ReceiveChecksum::Invalid,
                None => ReceiveChecksum::Unchecked,
            }
        } else {
            ReceiveChecksum::Unchecked
        };
        Self {
            address: bytes.as_ptr() as usize,
            length: bytes.len(),
            checksum,
            ip,
            annotation: preparation.annotation,
        }
    }
}

/// 保活接收映射并接管票据归还的队列端。
pub trait NetworkReceiveOwner: Send + Sync {
    /// 给仍在借用的票据增加一个只读持有者。None 表示队列只支持同步借用。
    /// # Errors
    /// 票据无效、设备退出或超出队列的持有上限。
    /// # Safety
    /// 原 RX 借用在调用时必须仍有效。成功返回的 owner 须独立保活映射，最后一个持有者归还后才能复用槽。
    unsafe fn retain_received(
        &self,
        _queue: u16,
        _slot: u16,
        _ticket: u64,
    ) -> Result<Option<Arc<dyn NetworkReceiveOwner>>, NetworkError> {
        Ok(None)
    }

    /// 消费 `queue` 中 `slot` 的 `ticket`，接管归还义务。
    /// # Errors
    /// 设备或协议失败时终止本次会话，不得复用该票据。
    /// # Safety
    /// 只能消费本次持有，调用前必须结束从该持有借出的引用，不得重复调用。
    unsafe fn release_received(
        &self,
        queue: u16,
        slot: u16,
        ticket: u64,
    ) -> Result<(), NetworkError>;
}

/// 原始 CPU RX 借用。只有原始借用和保留的只读载荷均释放后，设备才能复用槽。
pub struct NetworkReceiveFrame<'a> {
    owner: &'a dyn NetworkReceiveOwner,
    queue: u16,
    identity: Option<(u16, u64)>,
    buffer: NetworkReceiveBuffer,
}
impl<'a> NetworkReceiveFrame<'a> {
    /// 从已领取的槽建立借用。`owner` 保活接收映射，`queue`、`slot` 和 `ticket` 标识唯一所有权。
    /// # Safety
    /// buffer 是同一槽及票据的描述，检查后一直保持不变，且在 owner 寿命内仍有效。设备和其他 CPU
    /// 在全部持有释放前不能写入或复用该范围。不能为同一所有权构造两个 token。
    /// 若 buffer 表示设备校验和有效，设备必须已验证或补全传输层校验和。软件事实与同一槽的字节绑定。
    pub unsafe fn new(
        owner: &'a dyn NetworkReceiveOwner,
        queue: u16,
        slot: u16,
        ticket: u64,
        buffer: NetworkReceiveBuffer,
    ) -> Self {
        Self {
            owner,
            queue,
            identity: Some((slot, ticket)),
            buffer,
        }
    }
    /// transport checksum 已由设备验证或在交付前补全；不涵盖软件预查或网络头校验。
    pub fn checksum_valid(&self) -> bool {
        self.buffer.checksum == ReceiveChecksum::Device
    }
    /// 返回原帧的传输层校验状态，供选择相同校验范围的消费方使用。
    pub fn transport_checksum(&self) -> ReceiveChecksum {
        self.buffer.checksum
    }
    /// 领取时从原帧提取的 IP 结构事实。协议仍须执行自己的接收检查。
    pub fn ingress_ip(&self) -> Option<IngressIp> {
        self.buffer.ip
    }
    /// 借用原帧已检查边界的 IPv4 或 IPv6 载荷。畸形、VLAN 或非 IP 帧返回 None。
    /// 调用者仍须按协议校验 IPv4 头校验和和报文准入。
    pub fn direct_ip(&self) -> Option<NetworkPacket<'_>> {
        // SAFETY: 构造 token 时的槽和票据证明绑定到同一份原始 bytes；
        // 设备在 release 前不能改写。输出视图借用 self，不能跨过 release。
        self.buffer
            .ip
            .as_ref()
            .map(|ip| unsafe { ip.borrow_direct(self) })
    }
    /// 返回队列接收者为本帧提供的值，未设置时为全零。
    pub fn annotation(&self) -> &[u64; 10] {
        &self.buffer.annotation
    }
    /// 返回持有该 RX 借用的硬件队列编号，用于保持收发队列亲和。
    pub fn queue(&self) -> u16 {
        self.queue
    }
    /// 保留原帧中 `bytes` 指定的连续载荷，允许其在出口队列线程中读取。
    /// # Errors
    /// 范围不属于原帧，或 RX owner 无法继续持有该票据。
    pub fn retain_payload(&self, bytes: &[u8]) -> Result<Option<NetworkReceivePayload>, NetworkError> {
        let address = bytes.as_ptr() as usize;
        let offset = address
            .checked_sub(self.buffer.address)
            .filter(|&offset| offset <= self.buffer.length)
            .ok_or(NetworkError::InvalidConfiguration)?;
        if bytes.len() > self.buffer.length - offset {
            return Err(NetworkError::InvalidConfiguration);
        }
        let (slot, ticket) = self.identity.expect("receive frame released twice");
        // SAFETY: self 保有同一槽的票据且 bytes 在其范围内；新 owner 接管一个独立只读持有。
        let owner = unsafe { self.owner.retain_received(self.queue, slot, ticket) }?;
        Ok(owner.map(|owner| NetworkReceivePayload {
            owner,
            queue: self.queue,
            slot,
            ticket,
            address,
            length: bytes.len(),
        }))
    }
    /// 交还 RX 借用，由设备接管归还义务；无论成功与否都结束报文字节访问。
    /// # Errors
    /// 接管时的协议或设备失败由实现返回；失败必须终止会话，不能重复归还同一 ticket。
    pub fn release(mut self) -> Result<(), NetworkError> {
        let (slot, ticket) = self.identity.take().expect("receive frame released twice");
        // SAFETY: 消费 self，所有从 Deref 借出的引用均已结束；本 token 唯一拥有该身份。
        unsafe { self.owner.release_received(self.queue, slot, ticket) }
    }
}
impl core::ops::Deref for NetworkReceiveFrame<'_> {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        // SAFETY: 构造者保证映射/初始化与排他的设备→CPU 转移；借用止于 token，Drop 后不可访问。
        unsafe { core::slice::from_raw_parts(self.buffer.address as *const u8, self.buffer.length) }
    }
}
impl Drop for NetworkReceiveFrame<'_> {
    fn drop(&mut self) {
        if let Some((slot, ticket)) = self.identity.take() {
            // SAFETY: Drop 时没有仍可用的内容引用，本 token 不可复制且尚未交出 ownership。
            let _ = unsafe { self.owner.release_received(self.queue, slot, ticket) };
        }
    }
}

/// 独立保活原 RX 槽中一段不可变字节的发送载荷。析构归还本次只读持有。
pub struct NetworkReceivePayload {
    owner: Arc<dyn NetworkReceiveOwner>,
    queue: u16,
    slot: u16,
    ticket: u64,
    address: usize,
    length: usize,
}

impl core::ops::Deref for NetworkReceivePayload {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        // SAFETY: retain_payload 验证范围并取得独立的持有，owner 保活映射且槽不可写。
        unsafe { core::slice::from_raw_parts(self.address as *const u8, self.length) }
    }
}

impl Drop for NetworkReceivePayload {
    fn drop(&mut self) {
        // SAFETY: 所有载荷引用随本对象结束；这里只消费本次 retain 得到的持有。
        let _ = unsafe { self.owner.release_received(self.queue, self.slot, self.ticket) };
    }
}

/// TX 槽已初始化的数据及出口队列仍须写入的内容。
pub enum NetworkTransmitData {
    /// 槽内完整数据。校验和与分段要求由生产者定义。
    Ready(Option<NetworkChecksum>, Option<NetworkTcpSegmentation>),
    /// 已编码前缀之后的尾部由出口队列从原 RX 载荷复制，保留原传输层校验和。
    ReceivePayload(NetworkReceivePayload),
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
    /// 实际预留的硬件队列编号。
    pub fn queue(&self) -> u16 {
        self.queue
    }
    /// 借出 CPU 写入区；引用的寿命只覆盖本次 &mut self，不能跨越消费 self 的 submit。
    pub fn buffer_mut(&mut self) -> &mut [u8] {
        // SAFETY: 构造者的排他范围证明由 token 保持；&mut self 阻止同时创建另一个可写引用。
        unsafe { core::slice::from_raw_parts_mut(self.address as *mut u8, self.capacity) }
    }
    /// 将前 length 字节交给发送队列。本方法消费 token，不复制 payload。
    /// # Errors
    /// 长度越界或队列接管失败时返回错误。尚未交出的 reservation 由 Drop 取消。
    pub fn submit(
        self,
        length: usize,
        checksum: Option<NetworkChecksum>,
        segmentation: Option<NetworkTcpSegmentation>,
    ) -> Result<(), NetworkError> {
        self.submit_data(length, NetworkTransmitData::Ready(checksum, segmentation))
    }

    /// 提交已编码的 `prefix` 字节及独立持有的原 RX 载荷。出口在发表 TX 描述符前完成复制。
    /// # Errors
    /// 总长度越界或队列接管失败。失败时按原路径取消 reservation，并归还源载荷。
    pub fn submit_payload(
        self,
        prefix: usize,
        payload: NetworkReceivePayload,
    ) -> Result<(), NetworkError> {
        let length = prefix
            .checked_add(payload.len())
            .ok_or(NetworkError::FrameTooLarge)?;
        self.submit_data(length, NetworkTransmitData::ReceivePayload(payload))
    }

    fn submit_data(mut self, length: usize, data: NetworkTransmitData) -> Result<(), NetworkError> {
        if length == 0 || length > self.capacity {
            return Err(NetworkError::FrameTooLarge);
        }
        if let NetworkTransmitData::Ready(checksum, segmentation) = &data {
            self.device
                .transmit_capabilities()
                .validate_packet(length, *checksum, *segmentation)?;
        }
        let (slot, ticket) = self.identity.expect("transmit frame consumed twice");
        // SAFETY: 消费 self 后所有 buffer_mut 的引用均已结束，唯一 token 交出本次 reservation。
        unsafe {
            self.device
                .submit_transmit(self.queue, slot, ticket, length, data)
        }?;
        self.identity.take();
        Ok(())
    }
}
impl Drop for NetworkTransmit<'_> {
    fn drop(&mut self) {
        if let Some((slot, ticket)) = self.identity.take() {
            // SAFETY: 已结束全部 payload 引用；设备只取消仍由此 token 预留的票据。
            unsafe { self.device.cancel_transmit(self.queue, slot, ticket) };
        }
    }
}
