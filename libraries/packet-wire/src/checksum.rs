//! RFC 1071 one's-complement checksum；64-bit 并行累加，支持任意奇偶分段，不分配或依赖地址对齐。

/// 校验和流的唯一累加状态，保留跨片段的奇数字节。
#[derive(Default)]
pub struct InternetChecksum {
    sum: u64,
    odd: Option<u8>,
}
impl InternetChecksum {
    /// 创建空流。
    pub const fn new() -> Self {
        Self { sum: 0, odd: None }
    }
    fn add_word(&mut self, word: u64) {
        let (sum, carry) = self.sum.overflowing_add(word);
        self.sum = sum.wrapping_add(u64::from(carry));
    }
    /// 追加网络字节序片段；空片段不会消耗上次留下的奇数字节。
    pub fn add(&mut self, mut bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        if let Some(high) = self.odd.take() {
            self.add_word(u64::from(u16::from_be_bytes([high, bytes[0]])));
            bytes = &bytes[1..];
        }
        let (words, remainder) = bytes.as_chunks::<8>();
        for word in words {
            self.add_word(u64::from_be_bytes(*word));
        }
        let (pairs, remainder) = remainder.as_chunks::<2>();
        for pair in pairs {
            self.add_word(u64::from(u16::from_be_bytes(*pair)));
        }
        self.odd = remainder.first().copied();
    }
    /// 折叠但不取反；用于伪头 seed 或验证完整报文（有效结果为 0xffff）。
    pub fn sum(&self) -> u16 {
        let mut sum = self.sum;
        if let Some(high) = self.odd {
            let (value, carry) = sum.overflowing_add(u64::from(high) << 8);
            sum = value.wrapping_add(u64::from(carry));
        }
        sum = (sum & 0xffff_ffff) + (sum >> 32);
        sum = (sum & 0xffff) + (sum >> 16);
        sum = (sum & 0xffff) + (sum >> 16);
        // 64→32 的结果可有第 33 位；最后一次 16-bit carry 也必须回卷。
        sum = (sum & 0xffff) + (sum >> 16);
        sum as u16
    }
    /// 返回取反后的 checksum；不会改变状态，后续仍可追加片段。
    pub fn checksum(&self) -> u16 {
        !self.sum()
    }
}
/// 计算单片段 checksum；包含既有 pseudo-header seed 时同样适用。
pub fn checksum(bytes: &[u8]) -> u16 {
    let mut state = InternetChecksum::new();
    state.add(bytes);
    state.checksum()
}

/// 传输校验和；IPv4/IPv6 伪头从实际地址族构造，不复制 payload 或分配。
/// # Errors
/// 地址族不一致或传输长度超出所选伪头的长度字段。
pub fn transport_checksum(
    source: core::net::IpAddr,
    destination: core::net::IpAddr,
    protocol: u8,
    bytes: &[u8],
) -> Result<u16, crate::PacketError> {
    transport_checksum_parts(
        source,
        destination,
        protocol,
        bytes.len(),
        core::iter::once(bytes),
    )
}

pub(crate) fn transport_checksum_parts<'a>(
    source: core::net::IpAddr,
    destination: core::net::IpAddr,
    protocol: u8,
    length: usize,
    parts: impl Iterator<Item = &'a [u8]>,
) -> Result<u16, crate::PacketError> {
    use core::net::IpAddr;
    let mut state = InternetChecksum::new();
    match (source, destination) {
        (IpAddr::V4(source), IpAddr::V4(destination)) => {
            let length = u16::try_from(length).map_err(|_| crate::PacketError::Malformed)?;
            state.add(&source.octets());
            state.add(&destination.octets());
            state.add(&[0, protocol]);
            state.add(&length.to_be_bytes());
        }
        (IpAddr::V6(source), IpAddr::V6(destination)) => {
            let length = u32::try_from(length).map_err(|_| crate::PacketError::Malformed)?;
            state.add(&source.octets());
            state.add(&destination.octets());
            state.add(&length.to_be_bytes());
            state.add(&[0, 0, 0, protocol]);
        }
        _ => return Err(crate::PacketError::Malformed),
    }
    for bytes in parts {
        state.add(bytes);
    }
    Ok(state.checksum())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rfc_vector_and_every_fragment_boundary_keep_byte_parity() {
        let data = [0x00, 0x01, 0xf2, 0x03, 0xf4, 0xf5, 0xf6, 0xf7];
        assert_eq!(checksum(&data), 0x220d);
        assert_eq!(checksum(&[0xff, 0xff, 0xff, 0xff, 0, 1, 0, 0]), 0xfffe);
        for length in 0..=data.len() {
            for split in 0..=length {
                let mut state = InternetChecksum::new();
                state.add(&data[..split]);
                state.add(&[]);
                state.add(&data[split..length]);
                assert_eq!(state.checksum(), checksum(&data[..length]));
            }
        }
        let mut state = InternetChecksum::new();
        state.add(&[0xff; 31]);
        state.add(&[0xff; 33]);
        assert_eq!(state.checksum(), 0);
    }
}
