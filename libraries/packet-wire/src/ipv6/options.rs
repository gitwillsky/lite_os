//! IPv6 Hop-by-Hop/Destination TLV 的已验证迭代与带 x*n+y 对齐的原位编码。
use super::{Ipv6Error, error};
use crate::PacketError;

/// 未识别 Option Type 高两位的标准动作；是否发送 ICMP 由协议 owner 结合地址及限速判断。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnknownOptionAction {
    Skip,
    Discard,
    DiscardAndIcmp,
    DiscardAndIcmpUnlessMulticast,
}

/// 已验证的单个 TLV（Pad1 没有长度字节）；offset 相对完整 IPv6 包起点。
pub struct Ipv6Option<'a> {
    pub kind: u8,
    pub data: &'a [u8],
    pub offset: usize,
}
impl Ipv6Option<'_> {
    /// 未识别该选项时应采用的动作，不在 codec 中直接丢包或发送 ICMP。
    pub fn unknown_action(&self) -> UnknownOptionAction {
        match self.kind >> 6 {
            0 => UnknownOptionAction::Skip,
            1 => UnknownOptionAction::Discard,
            2 => UnknownOptionAction::DiscardAndIcmp,
            _ => UnknownOptionAction::DiscardAndIcmpUnlessMulticast,
        }
    }
    /// Type 的 change-en-route 位；认证 owner 据此决定是否将选项数据视为可变字段。
    pub fn mutable(&self) -> bool {
        self.kind & 0x20 != 0
    }
}

/// 选项区的一次校验、线性游标；不拥有 payload 或重复校验已验证 TLV。
#[derive(Clone)]
pub struct Options<'a> {
    bytes: &'a [u8],
    offset: usize,
}
impl<'a> Options<'a> {
    /// 校验 `bytes` 的 TLV 边界；base 是首字节在完整 IPv6 包中的位置。
    /// # Errors
    /// 完整选项区的 TLV 却缺少长度/数据，返回 Malformed 与字段位置，不用另一分片补齐。
    pub fn parse(bytes: &'a [u8], base: usize) -> Result<Self, Ipv6Error> {
        base.checked_add(bytes.len())
            .ok_or_else(|| error(PacketError::Malformed, base))?;
        let mut offset = 0;
        while offset < bytes.len() {
            if bytes[offset] == 0 {
                offset += 1;
                continue;
            }
            let length = usize::from(
                *bytes
                    .get(offset + 1)
                    .ok_or_else(|| error(PacketError::Malformed, base + offset))?,
            );
            if length + 2 > bytes.len() - offset {
                return Err(error(PacketError::Malformed, base + offset + 1));
            }
            offset += length + 2;
        }
        Ok(Self {
            bytes,
            offset: base,
        })
    }
    pub(super) fn validated(bytes: &'a [u8], offset: usize) -> Self {
        Self { bytes, offset }
    }
}
impl<'a> Iterator for Options<'a> {
    type Item = Ipv6Option<'a>;
    fn next(&mut self) -> Option<Self::Item> {
        let (&kind, tail) = self.bytes.split_first()?;
        let length = if kind == 0 {
            1
        } else {
            2 + usize::from(tail[0])
        };
        let option = Ipv6Option {
            kind,
            data: if kind == 0 {
                &[]
            } else {
                &self.bytes[2..length]
            },
            offset: self.offset,
        };
        self.bytes = &self.bytes[length..];
        self.offset += length;
        Some(option)
    }
}

/// 输出 TLV 的数据与 x*n+y 对齐。Pad1 的 data 必须为空；PadN 的 data 必须为零。
pub struct OptionData<'a> {
    pub kind: u8,
    pub data: &'a [u8],
    /// 相对扩展头起点的 x*n+y；x 为 1..=128 的二次幂、y < x，Jumbo Payload 使用 (4, 2)。
    pub alignment: (u8, u8),
}
impl OptionData<'_> {
    fn padding(&self, offset: usize) -> Result<usize, PacketError> {
        let (modulus, remainder) = self.alignment;
        if modulus == 0
            || !modulus.is_power_of_two()
            || remainder >= modulus
            || self.data.len() > 255
            || self.kind == 0 && !self.data.is_empty()
            || self.kind == 1 && self.data.iter().any(|byte| *byte != 0)
        {
            return Err(PacketError::Malformed);
        }
        Ok(
            (usize::from(remainder) + usize::from(modulus) - offset % usize::from(modulus))
                % usize::from(modulus),
        )
    }
    fn length(&self) -> usize {
        if self.kind == 0 {
            1
        } else {
            2 + self.data.len()
        }
    }
}

/// 在 output 前部编码一个 Hop/Destination header，next_header 标明后继协议，返回 8-byte 对齐长度。
/// # Errors
/// 选项/对齐非法，头超过 2048 字节或输出不足；错误前不写入 output。
pub fn emit(
    output: &mut [u8],
    next_header: u8,
    options: &[OptionData<'_>],
) -> Result<usize, PacketError> {
    let mut length = 2usize;
    for option in options {
        length = length
            .checked_add(option.padding(length)?)
            .and_then(|length| length.checked_add(option.length()))
            .ok_or(PacketError::Capacity)?;
    }
    length = length.checked_add(7).ok_or(PacketError::Capacity)? & !7;
    if length > 2048 {
        return Err(PacketError::Capacity);
    }
    let output = output.get_mut(..length).ok_or(PacketError::Capacity)?;
    output[0] = next_header;
    output[1] = (length / 8 - 1) as u8;
    let mut offset = 2;
    for option in options {
        let gap = option
            .padding(offset)
            .expect("prevalidated option alignment");
        pad(&mut output[offset..offset + gap]);
        offset += gap;
        output[offset] = option.kind;
        if option.kind != 0 {
            output[offset + 1] = option.data.len() as u8;
            output[offset + 2..offset + option.length()].copy_from_slice(option.data);
        }
        offset += option.length();
    }
    pad(&mut output[offset..]);
    Ok(length)
}
fn pad(output: &mut [u8]) {
    match output.len() {
        0 => (),
        1 => output[0] = 0,
        length => {
            output[0] = 1;
            output[1] = (length - 2) as u8;
            output[2..].fill(0);
        }
    }
}
