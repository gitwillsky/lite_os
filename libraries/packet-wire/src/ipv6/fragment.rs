//! RFC 8200 源端分片。每片直接写入发送窗口，保留原载荷借用。
use super::{Fragment, Ipv6Packet};
use crate::PacketError;
use core::ops::Range;

/// 分片头的插入位置及首片所需的完整头长度。
#[derive(Clone, Copy)]
pub struct Layout {
    prefix: usize,
    previous: usize,
    first_required: usize,
}
impl Layout {
    /// 无扩展头的 UDP 使用固定布局，首片必须包含完整 UDP 头。
    pub const fn udp() -> Self {
        Self {
            prefix: 40,
            previous: 6,
            first_required: 8,
        }
    }

    /// 根据已校验原包确定逐片携带的头及上层头边界。
    /// # Errors
    /// 已经带分片头或使用 Jumbo 的报文不能再次分片。已知上层头截断也拒绝。
    pub fn for_packet(packet: Ipv6Packet<'_>) -> Result<Self, PacketError> {
        if packet.bytes().len() > 40 + usize::from(u16::MAX) {
            return Err(PacketError::Unsupported);
        }
        let mut prefix = 40;
        let mut previous = 6;
        for extension in packet.extensions() {
            match extension.kind() {
                44 => return Err(PacketError::Unsupported),
                0 | 43 => {
                    previous = extension.offset();
                    prefix = previous + extension.bytes().len();
                }
                _ => {}
            }
        }
        let upper = packet.upper_offset();
        let upper_bytes = &packet.bytes()[upper..];
        let required = match packet.upper_protocol() {
            6 => {
                let length =
                    usize::from(*upper_bytes.get(12).ok_or(PacketError::Truncated)? >> 4) * 4;
                if length < 20 {
                    return Err(PacketError::Malformed);
                }
                length
            }
            17 | 50 | 58 => 8,
            4 => usize::from(upper_bytes.first().ok_or(PacketError::Truncated)? & 15).max(5) * 4,
            41 => 40,
            _ => 0,
        };
        if upper_bytes.len() < required {
            return Err(PacketError::Truncated);
        }
        Ok(Self {
            prefix,
            previous,
            first_required: upper - prefix + required,
        })
    }

    /// 原包中可分片部分的起点。
    pub fn payload_start(self) -> usize {
        self.prefix
    }

    /// 将一片的固定头写入 `header`，返回设备窗口中的扩展头和载荷布局。
    /// `original` 至少含逐片携带的原头。 `payload_len` 为原固定头后的长度。
    /// `progress` 是已经提交的载荷字节数和原数据报标识。
    /// # Errors
    /// 报文长度、偏移、MTU 或首片所需空间不合法时返回错误，保留原固定头输出。
    pub fn piece<'a>(
        self,
        original: &'a [u8],
        payload_len: usize,
        progress: (usize, u32),
        mtu: usize,
        header: &mut [u8; 40],
    ) -> Result<Piece<'a>, PacketError> {
        let prefix = original.get(..self.prefix).ok_or(PacketError::Truncated)?;
        let fixed_overhead = self.prefix + 8;
        let limit = mtu.min(40 + usize::from(u16::MAX));
        let capacity = limit
            .checked_sub(fixed_overhead)
            .ok_or(PacketError::Capacity)?;
        let total = payload_len
            .checked_sub(self.prefix - 40)
            .ok_or(PacketError::Malformed)?;
        let (offset, identification) = progress;
        if payload_len > usize::from(u16::MAX) || offset >= total || !offset.is_multiple_of(8) {
            return Err(PacketError::Malformed);
        }
        let remaining = total - offset;
        let length = if remaining <= capacity {
            remaining
        } else {
            capacity & !7
        };
        if length == 0 || offset == 0 && length < self.first_required {
            return Err(PacketError::Capacity);
        }
        let end = offset + length;
        let mut fragment = [0; 8];
        Fragment {
            next_header: prefix[self.previous],
            offset,
            more: end < total,
            identification,
        }
        .emit(&mut fragment)?;
        header.copy_from_slice(&prefix[..40]);
        let length = (fixed_overhead + length - 40) as u16;
        header[4..6].copy_from_slice(&length.to_be_bytes());
        let previous = self.previous.checked_sub(40);
        if previous.is_none() {
            header[6] = 44;
        }
        Ok(Piece {
            range: offset..end,
            extensions: &prefix[40..],
            previous,
            fragment,
        })
    }
}

/// 一个待提交分片。载荷区间相对于原可分片部分起点。
pub struct Piece<'a> {
    pub range: Range<usize>,
    extensions: &'a [u8],
    previous: Option<usize>,
    fragment: [u8; 8],
}
impl Piece<'_> {
    /// 固定 IPv6 头之后需要的设备窗口大小。
    pub fn body_len(&self) -> usize {
        self.extensions.len() + self.fragment.len() + self.range.len()
    }
    /// 写逐片扩展头和分片头，返回由调用方填写的载荷窗口。
    pub fn emit_prefix<'a>(&self, output: &'a mut [u8]) -> &'a mut [u8] {
        let (extensions, tail) = output.split_at_mut(self.extensions.len());
        extensions.copy_from_slice(self.extensions);
        if let Some(previous) = self.previous {
            extensions[previous] = 44;
        }
        let (fragment, payload) = tail.split_at_mut(self.fragment.len());
        fragment.copy_from_slice(&self.fragment);
        payload
    }
}
