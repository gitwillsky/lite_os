//! 单帧部分校验和请求；不推断 RAW 内容，也不包含具体设备 header 布局。
use super::NetworkError;

/// 由发送者准备好伪头和 seed 的单帧 checksum 区域，偏移相对 Ethernet 帧首字节。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetworkChecksum {
    /// 从此字节到帧末尾计算 one's-complement checksum。
    pub start: u16,
    /// 相对 start 的两字节结果位置；提交前已包含伪头 seed。
    pub offset: u16,
    /// 含 L2/L3/L4 的完整 header 末尾，用于设备的 header-length hint。
    pub header_length: u16,
}
impl NetworkChecksum {
    /// 确保区域和结果字段完全位于已初始化的 length 字节中。
    /// # Errors
    /// 越界或缺少完整两字节字段返回 InvalidConfiguration。
    pub fn validate(self, length: usize) -> Result<(), NetworkError> {
        if usize::from(self.start) + usize::from(self.offset) + 2 > usize::from(self.header_length)
            || usize::from(self.header_length) > length
        {
            return Err(NetworkError::InvalidConfiguration);
        }
        Ok(())
    }
}
