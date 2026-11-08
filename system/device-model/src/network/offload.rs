//! 发送能力与逐包分段意图；不包含设备寄存器或私有 header 编码。
use super::{NetworkChecksum, NetworkError};

/// 本次会话可接受的发送能力；容量必须由实际池兑现，不能只发布 feature 位。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetworkTransmitCapabilities {
    pub checksum: bool,
    pub tcp_ipv4: bool,
    pub tcp_ipv6: bool,
    pub tcp_ecn: bool,
    /// 无分段时的最大 Ethernet 帧长度。
    pub maximum_frame_length: u32,
    /// 本会话每个 TX 槽可容纳的最大完整大包长度。
    pub maximum_buffer_length: u32,
}
impl NetworkTransmitCapabilities {
    /// 验证能力依赖和非零容量，畸形组合返回 InvalidConfiguration。
    pub fn validate(self) -> Result<(), NetworkError> {
        if self.maximum_frame_length < 14
            || self.maximum_buffer_length < self.maximum_frame_length
            || (self.tcp_ipv4 || self.tcp_ipv6) && !self.checksum
            || self.tcp_ecn && !(self.tcp_ipv4 || self.tcp_ipv6)
            || self.maximum_buffer_length > self.maximum_frame_length
                && !(self.tcp_ipv4 || self.tcp_ipv6)
        {
            return Err(NetworkError::InvalidConfiguration);
        }
        Ok(())
    }
    /// 在交出槽前验证长度、checksum 与 TSO 的完整依赖；不读取或修改 payload。
    /// # Errors
    /// 容量/普通 MTU 越界返回 FrameTooLarge，未协商能力或畸形元数据返回 InvalidConfiguration。
    pub fn validate_packet(
        self,
        length: usize,
        checksum: Option<NetworkChecksum>,
        segmentation: Option<NetworkTcpSegmentation>,
    ) -> Result<(), NetworkError> {
        if length == 0 || length > self.maximum_buffer_length as usize {
            return Err(NetworkError::FrameTooLarge);
        }
        if let Some(checksum) = checksum {
            if !self.checksum {
                return Err(NetworkError::InvalidConfiguration);
            }
            checksum.validate(length)?;
        }
        let Some(segmentation) = segmentation else {
            return if length <= self.maximum_frame_length as usize {
                Ok(())
            } else {
                Err(NetworkError::FrameTooLarge)
            };
        };
        let checksum = checksum.ok_or(NetworkError::InvalidConfiguration)?;
        let supported = match segmentation.version {
            NetworkTcpVersion::Ipv4 => self.tcp_ipv4,
            NetworkTcpVersion::Ipv6 => self.tcp_ipv6,
        };
        let tcp_header = usize::from(checksum.header_length) - usize::from(checksum.start);
        if !supported
            || segmentation.ecn && !self.tcp_ecn
            || segmentation.maximum_segment_size == 0
            || checksum.offset != 16
            || !(20..=60).contains(&tcp_header)
            || !tcp_header.is_multiple_of(4)
            || usize::from(checksum.header_length) >= length
            || u32::from(checksum.header_length) + u32::from(segmentation.maximum_segment_size)
                > self.maximum_frame_length
        {
            return Err(NetworkError::InvalidConfiguration);
        }
        Ok(())
    }
}

/// 所分段 TCP 包的网络地址族；不支持 UDP/UFO 或隧道分段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkTcpVersion {
    Ipv4,
    Ipv6,
}

/// 调用方已构造完整 TCP 大包和伪头 seed，要求设备按 MSS 复制头并分段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetworkTcpSegmentation {
    pub version: NetworkTcpVersion,
    pub maximum_segment_size: u16,
    /// 分段时保留 TCP ECN 语义；需要独立协商相应设备能力。
    pub ecn: bool,
}

/// 一次独占 OPEN 的资源请求；队列数和大帧容量由实际发送者明确选择。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetworkQueueConfiguration {
    pub queue_pairs: u16,
    pub transmit_bytes: u32,
}
impl NetworkQueueConfiguration {
    /// 验证请求不超出物理能力，失败不改变当前租约或分配资源。
    /// # Errors
    /// 零/超限队列，或不能容纳普通帧/超出硬件发送上限时返回 InvalidConfiguration。
    pub fn validate(
        self,
        maximum_pairs: u16,
        capabilities: NetworkTransmitCapabilities,
    ) -> Result<(), NetworkError> {
        capabilities.validate()?;
        if self.queue_pairs == 0
            || self.queue_pairs > maximum_pairs
            || self.transmit_bytes < capabilities.maximum_frame_length
            || self.transmit_bytes > capabilities.maximum_buffer_length
        {
            return Err(NetworkError::InvalidConfiguration);
        }
        Ok(())
    }
}
