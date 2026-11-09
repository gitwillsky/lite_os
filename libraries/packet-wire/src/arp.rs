//! RFC 826 ARP 的可变宽地址格式；不拥有邻居缓存、学习、超时或响应策略。
use crate::PacketError;

/// 按 hlen/plen 验证并裁去链路 padding 的 ARP 借用；未知 opcode 与地址类型保留原值。
#[derive(Clone, Copy)]
pub struct ArpPacket<'a> {
    bytes: &'a [u8],
    hardware: usize,
    protocol: usize,
}
impl<'a> ArpPacket<'a> {
    /// 验证 `bytes` 的固定头与四个地址范围；Ethernet/IPv4 的类型检查由使用者执行。
    /// # Errors
    /// 头或地址区截断，或地址宽度为零。
    pub fn parse(bytes: &'a [u8]) -> Result<Self, PacketError> {
        if bytes.len() < 8 {
            return Err(PacketError::Truncated);
        }
        let hardware = usize::from(bytes[4]);
        let protocol = usize::from(bytes[5]);
        if hardware == 0 || protocol == 0 {
            return Err(PacketError::Malformed);
        }
        let length = 8 + 2 * (hardware + protocol);
        let bytes = bytes.get(..length).ok_or(PacketError::Truncated)?;
        Ok(Self {
            bytes,
            hardware,
            protocol,
        })
    }
    /// 硬件类型，Ethernet 为 1。
    pub fn hardware_type(self) -> u16 {
        u16::from_be_bytes([self.bytes[0], self.bytes[1]])
    }
    /// 协议地址类型，IPv4 为 0x0800。
    pub fn protocol_type(self) -> u16 {
        u16::from_be_bytes([self.bytes[2], self.bytes[3]])
    }
    /// opcode 的网络字段（request=1，reply=2），不把其他 opcode 当作回复。
    pub fn operation(self) -> u16 {
        u16::from_be_bytes([self.bytes[6], self.bytes[7]])
    }
    /// 发送者硬件地址。
    pub fn sender_hardware(self) -> &'a [u8] {
        &self.bytes[8..8 + self.hardware]
    }
    /// 发送者协议地址。
    pub fn sender_protocol(self) -> &'a [u8] {
        &self.bytes[8 + self.hardware..8 + self.hardware + self.protocol]
    }
    /// 目标硬件地址。
    pub fn target_hardware(self) -> &'a [u8] {
        let start = 8 + self.hardware + self.protocol;
        &self.bytes[start..start + self.hardware]
    }
    /// 目标协议地址。
    pub fn target_protocol(self) -> &'a [u8] {
        &self.bytes[8 + 2 * self.hardware + self.protocol..]
    }
    /// 完整的 ARP 字节，排除链路 padding。
    pub fn bytes(self) -> &'a [u8] {
        self.bytes
    }
}

/// 要发送的 ARP 字段；两端硬件地址与两端协议地址须各自等宽且为 1..=255 字节。
pub struct ArpHeader<'a> {
    pub hardware_type: u16,
    pub protocol_type: u16,
    pub operation: u16,
    pub sender_hardware: &'a [u8],
    pub sender_protocol: &'a [u8],
    pub target_hardware: &'a [u8],
    pub target_protocol: &'a [u8],
}
impl ArpHeader<'_> {
    /// 验证地址宽度并返回完整消息长度。
    /// # Errors
    /// 地址宽度不匹配、为零或超出单字节长度字段。
    pub fn length(&self) -> Result<usize, PacketError> {
        let hardware = self.sender_hardware.len();
        let protocol = self.sender_protocol.len();
        if !(1..=255).contains(&hardware)
            || !(1..=255).contains(&protocol)
            || self.target_hardware.len() != hardware
            || self.target_protocol.len() != protocol
        {
            return Err(PacketError::Malformed);
        }
        Ok(8 + 2 * (hardware + protocol))
    }
    /// 向 `output` 前部编码 ARP，返回精确长度，不写以太网 padding。
    /// # Errors
    /// 地址宽度错误或输出容量不足；失败不改变输出。
    pub fn emit(&self, output: &mut [u8]) -> Result<usize, PacketError> {
        let length = self.length()?;
        let output = output.get_mut(..length).ok_or(PacketError::Capacity)?;
        output[..2].copy_from_slice(&self.hardware_type.to_be_bytes());
        output[2..4].copy_from_slice(&self.protocol_type.to_be_bytes());
        output[4] = self.sender_hardware.len() as u8;
        output[5] = self.sender_protocol.len() as u8;
        output[6..8].copy_from_slice(&self.operation.to_be_bytes());
        let mut offset = 8;
        for bytes in [
            self.sender_hardware,
            self.sender_protocol,
            self.target_hardware,
            self.target_protocol,
        ] {
            output[offset..offset + bytes.len()].copy_from_slice(bytes);
            offset += bytes.len();
        }
        Ok(length)
    }
}
