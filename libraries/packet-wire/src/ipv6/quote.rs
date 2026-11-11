//! ICMP 引用是原包的可见前缀，不能拿完整 IPv6 长度校验拒绝合法截断，也不能把它当作已重组的接收包。
use super::*;

/// 已验证固定头与可见完整扩展链的引用；只暴露引用事实，不交出可送入接收路径的完整包类型。
pub struct Ipv6Quote<'a> {
    bytes: &'a [u8],
    chain: Chain,
    length: usize,
}
impl<'a> Ipv6Quote<'a> {
    /// 解析原 IP 包的前缀，按原声明长度裁去额外字节；不验证不可见的正文/checksum。
    /// # Parameters
    /// `bytes` 是 ICMP owner 已分离扩展对象后的引用区。
    /// # Returns
    /// 可见源、目的与上层前缀。分片未重组或上层头被截断时不能取得完整传输信息。
    /// # Errors
    /// 固定头不足、非 IPv6、Jumbo/可见扩展链非法或不完整。
    pub fn parse(bytes: &'a [u8]) -> Result<Self, Ipv6Error> {
        if bytes.len() < 40 {
            return Err(error(PacketError::Truncated, bytes.len()));
        }
        if bytes[0] >> 4 != 6 {
            return Err(error(PacketError::Unsupported, 0));
        }
        let declared = usize::from(u16::from_be_bytes([bytes[4], bytes[5]]));
        let bounded = if declared == 0 {
            bytes
        } else {
            &bytes[..bytes.len().min(40 + declared)]
        };
        let hop = if bytes[6] == 0 {
            Some(hop_facts(bounded, 40, 0)?)
        } else {
            None
        };
        let length = match (declared, hop.as_ref().and_then(|facts| facts.jumbo)) {
            (0, Some((length, _))) => length as usize,
            (0, None) if bytes[6] == 0 => return Err(error(PacketError::Malformed, 4)),
            (_, Some((_, offset))) => return Err(error(PacketError::Malformed, offset)),
            (length, None) => length,
        };
        let end = 40usize
            .checked_add(length)
            .ok_or_else(|| error(PacketError::Malformed, 4))?;
        let bytes = &bytes[..bytes.len().min(end)];
        let chain = scan(bytes, bytes[6], end, hop)?;
        Ok(Self {
            bytes,
            chain,
            length: end,
        })
    }
    /// 原包的可见前缀，排除引用 padding。
    pub fn bytes(&self) -> &'a [u8] {
        self.bytes
    }
    /// 原上层报文的声明长度。与引用的可见长度无关。
    pub fn upper_length(&self) -> Option<usize> {
        (!self.chain.fragmented).then_some(self.length - self.chain.end)
    }
    /// 原包的固定头源地址。
    pub fn source(&self) -> Ipv6Addr {
        header_address(self.bytes, 8)
    }
    /// 原包当前目的；Routing/AH 等语义仍由协议 owner 决定。
    pub fn destination(&self) -> Ipv6Addr {
        header_address(self.bytes, 24)
    }
    /// 可见扩展链之后的协议。
    pub fn upper_protocol(&self) -> u8 {
        self.chain.protocol
    }
    /// 完整报文或首片引用的可见上层前缀；可以少于传输头，调用者必须检查实际需要的宽度。
    pub fn upper_layer(&self) -> Option<&'a [u8]> {
        (!self.chain.non_initial_fragment && self.chain.protocol != 59)
            .then_some(&self.bytes[self.chain.end..])
    }
    /// 原头声明的完整 IP 长度（含固定头与 Jumbo 扩展），不是引用中可见的字节数。
    pub fn original_length(&self) -> usize {
        self.length
    }
}
