//! TCP TLV 的一次校验与借用迭代；MSS/WS/SACK/TS/MD5/AO 格式，不持有密钥或连接协商状态。
use crate::PacketError;

/// TCP 选项的字段值；Unknown 保留未实现选项，WindowScale 保留收到的原值供连接 owner 按规范截到 14。
pub enum TcpOption<'a> {
    End,
    Nop,
    MaximumSegmentSize(u16),
    WindowScale(u8),
    SackPermitted,
    Sack(SackBlocks<'a>),
    Timestamp {
        value: u32,
        echo: u32,
    },
    Md5(&'a [u8; 16]),
    Authentication {
        key_id: u8,
        next_key_id: u8,
        mac: &'a [u8],
    },
    Unknown {
        kind: u8,
        data: &'a [u8],
    },
}
/// 单个选项的字段与相对 TCP 段的 Type 字节位置，供认证 owner 定位 MAC 输入范围。
pub struct OptionView<'a> {
    pub offset: usize,
    pub value: TcpOption<'a>,
}
/// 所有 SACK blocks 的借用迭代，不按固定三块截断，也不将 wraparound 序号当作普通整数排序。
pub struct SackBlocks<'a> {
    bytes: &'a [u8],
}
impl Iterator for SackBlocks<'_> {
    type Item = (u32, u32);
    fn next(&mut self) -> Option<Self::Item> {
        if self.bytes.is_empty() {
            return None;
        }
        let block = &self.bytes[..8];
        self.bytes = &self.bytes[8..];
        Some((
            u32::from_be_bytes(block[..4].try_into().expect("validated SACK block")),
            u32::from_be_bytes(block[4..].try_into().expect("validated SACK block")),
        ))
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        let length = self.bytes.len() / 8;
        (length, Some(length))
    }
}
impl ExactSizeIterator for SackBlocks<'_> {}

/// 头部中已经校验的 TCP 选项游标；EOL 后的 padding 不再作为选项解释。
pub struct Options<'a> {
    bytes: &'a [u8],
    offset: usize,
}
impl<'a> Options<'a> {
    /// 验证长度至多 40 的 options 区；不强行限制未知选项、选项次序或 WS 的接收值。
    /// # Errors
    /// TLV 越界/已知选项宽度非法，MD5/AO 并存或重复认证选项。
    pub fn parse(bytes: &'a [u8]) -> Result<Self, PacketError> {
        if bytes.len() > 40 {
            return Err(PacketError::Malformed);
        }
        let mut rest = bytes;
        let mut authentication = None;
        while let Some((&kind, tail)) = rest.split_first() {
            if kind == 0 {
                break;
            }
            if kind == 1 {
                rest = tail;
                continue;
            }
            let length = usize::from(*tail.first().ok_or(PacketError::Malformed)?);
            if length < 2 || length > rest.len() {
                return Err(PacketError::Malformed);
            }
            validate_length(kind, length)?;
            if matches!(kind, 19 | 29) && authentication.replace(kind).is_some() {
                return Err(PacketError::Malformed);
            }
            rest = &rest[length..];
        }
        Ok(Self::validated(bytes))
    }
    pub(super) fn validated(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 20 }
    }
}
impl<'a> Iterator for Options<'a> {
    type Item = OptionView<'a>;
    fn next(&mut self) -> Option<Self::Item> {
        let (&kind, tail) = self.bytes.split_first()?;
        let length = if kind < 2 { 1 } else { usize::from(tail[0]) };
        let data = if kind < 2 {
            &[]
        } else {
            &self.bytes[2..length]
        };
        let value = match kind {
            0 => TcpOption::End,
            1 => TcpOption::Nop,
            2 => TcpOption::MaximumSegmentSize(u16::from_be_bytes(
                data.try_into().expect("validated MSS"),
            )),
            3 => TcpOption::WindowScale(data[0]),
            4 => TcpOption::SackPermitted,
            5 => TcpOption::Sack(SackBlocks { bytes: data }),
            8 => TcpOption::Timestamp {
                value: u32::from_be_bytes(data[..4].try_into().expect("validated timestamp")),
                echo: u32::from_be_bytes(data[4..].try_into().expect("validated timestamp")),
            },
            19 => TcpOption::Md5(data.try_into().expect("validated MD5 width")),
            29 => TcpOption::Authentication {
                key_id: data[0],
                next_key_id: data[1],
                mac: &data[2..],
            },
            _ => TcpOption::Unknown { kind, data },
        };
        let result = OptionView {
            offset: self.offset,
            value,
        };
        self.offset += length;
        self.bytes = if kind == 0 {
            &[]
        } else {
            &self.bytes[length..]
        };
        Some(result)
    }
}
fn validate_length(kind: u8, length: usize) -> Result<(), PacketError> {
    let valid = match kind {
        2 => length == 4,
        3 => length == 3,
        4 => length == 2,
        5 => length >= 10 && (length - 2).is_multiple_of(8),
        8 => length == 10,
        19 => length == 18,
        29 => length >= 4,
        _ => true,
    };
    if valid {
        Ok(())
    } else {
        Err(PacketError::Malformed)
    }
}
pub(super) fn validate_for_send(bytes: &[u8]) -> Result<(), PacketError> {
    for option in Options::parse(bytes)? {
        if matches!(option.value, TcpOption::End)
            && bytes[option.offset - 20 + 1..]
                .iter()
                .any(|byte| *byte != 0)
        {
            return Err(PacketError::Malformed);
        }
        if matches!(option.value, TcpOption::WindowScale(value) if value > 14) {
            return Err(PacketError::Malformed);
        }
    }
    Ok(())
}

/// 在调用者提供的至多 40-byte 选项区原位编码；每次追加前预留四字节对齐的收尾空间。
pub struct OptionsBuilder<'a> {
    bytes: &'a mut [u8],
    end: usize,
    // OWNER: 本 builder 唯一发表的认证选项种类；缺少该记录会允许重复 AO 或 MD5/AO 同段。
    authentication: Option<u8>,
}
impl<'a> OptionsBuilder<'a> {
    /// 借用 output 的选项存储，不清零未使用空间。
    pub fn new(output: &'a mut [u8]) -> Self {
        Self {
            bytes: output,
            end: 0,
            authentication: None,
        }
    }
    /// 追加 NOP，可用于调用者希望的选项对齐。
    /// # Errors
    /// 容量不足，失败不写入。
    pub fn nop(&mut self) -> Result<(), PacketError> {
        let end = self.reserve(1)?;
        self.bytes[self.end] = 1;
        self.end = end;
        Ok(())
    }
    /// 追加 MSS；默认值、最小值和 IPv6 jumbo 的 65535 语义属于连接 owner。
    /// # Errors
    /// 容量不足。
    pub fn maximum_segment_size(&mut self, value: u16) -> Result<(), PacketError> {
        self.put(2, &value.to_be_bytes())
    }
    /// 追加发送端 WS，值必须不超过 14。
    /// # Errors
    /// 超限或容量不足。
    pub fn window_scale(&mut self, value: u8) -> Result<(), PacketError> {
        if value > 14 {
            return Err(PacketError::Malformed);
        }
        self.put(3, &[value])
    }
    /// 追加 SACK permitted。
    /// # Errors
    /// 容量不足。
    pub fn sack_permitted(&mut self) -> Result<(), PacketError> {
        self.put(4, &[])
    }
    /// 追加完整有序 blocks，不剪掉末尾块；范围的 serial/window 合法性由连接 owner 决定。
    /// # Errors
    /// blocks 为空/超过 TCP 选项容量，或剩余空间不足。
    pub fn sack(&mut self, blocks: &[(u32, u32)]) -> Result<(), PacketError> {
        if blocks.is_empty() || blocks.len() > 4 {
            return Err(PacketError::Malformed);
        }
        let length = 2 + blocks.len() * 8;
        let end = self.reserve(length)?;
        self.bytes[self.end..self.end + 2].copy_from_slice(&[5, length as u8]);
        for (output, &(left, right)) in self.bytes[self.end + 2..end]
            .as_chunks_mut::<8>()
            .0
            .iter_mut()
            .zip(blocks)
        {
            output[..4].copy_from_slice(&left.to_be_bytes());
            output[4..].copy_from_slice(&right.to_be_bytes());
        }
        self.end = end;
        Ok(())
    }
    /// 追加 TSval/TSecr。
    /// # Errors
    /// 容量不足。
    pub fn timestamp(&mut self, value: u32, echo: u32) -> Result<(), PacketError> {
        let mut data = [0; 8];
        data[..4].copy_from_slice(&value.to_be_bytes());
        data[4..].copy_from_slice(&echo.to_be_bytes());
        self.put(8, &data)
    }
    /// 追加 MD5 选项字节，不在此计算或验证 digest。
    /// # Errors
    /// 已有认证选项或容量不足。
    pub fn md5(&mut self, digest: &[u8; 16]) -> Result<(), PacketError> {
        self.put(19, digest)
    }
    /// 追加 AO 的 KeyID/RNextKeyID/MAC 原值；MAC 长度由算法 owner 选择和核验。
    /// # Errors
    /// 已有认证选项、MAC/剩余选项区过大。
    pub fn authentication(
        &mut self,
        key_id: u8,
        next_key_id: u8,
        mac: &[u8],
    ) -> Result<(), PacketError> {
        if self.authentication.is_some() {
            return Err(PacketError::Malformed);
        }
        let length = mac.len().checked_add(4).ok_or(PacketError::Capacity)?;
        let end = self.reserve(length)?;
        self.bytes[self.end..self.end + 4].copy_from_slice(&[
            29,
            length as u8,
            key_id,
            next_key_id,
        ]);
        self.bytes[self.end + 4..end].copy_from_slice(mac);
        self.end = end;
        self.authentication = Some(29);
        Ok(())
    }
    /// 追加非已知 kind 的原始 TLV；已知字段必须走相应构造方法以保持同一校验规则。
    /// # Errors
    /// kind 已知、数据太长或容量不足。
    pub fn unknown(&mut self, kind: u8, data: &[u8]) -> Result<(), PacketError> {
        if matches!(kind, 0..=5 | 8 | 19 | 29) {
            return Err(PacketError::Malformed);
        }
        self.put(kind, data)
    }
    /// 用 EOL/零 padding 收尾到四字节；空间已由每次成功追加预留，不会在此分配或失败。
    pub fn finish(self) -> &'a mut [u8] {
        let end = (self.end + 3) & !3;
        self.bytes[self.end..end].fill(0);
        &mut self.bytes[..end]
    }
    fn reserve(&self, length: usize) -> Result<usize, PacketError> {
        let end = self.end.checked_add(length).ok_or(PacketError::Capacity)?;
        if end > 40 || (end + 3) & !3 > self.bytes.len() {
            return Err(PacketError::Capacity);
        }
        Ok(end)
    }
    fn put(&mut self, kind: u8, data: &[u8]) -> Result<(), PacketError> {
        let length = data.len().checked_add(2).ok_or(PacketError::Capacity)?;
        validate_length(kind, length)?;
        if matches!(kind, 19 | 29) && self.authentication.is_some() {
            return Err(PacketError::Malformed);
        }
        let end = self.reserve(length)?;
        self.bytes[self.end..self.end + 2].copy_from_slice(&[kind, length as u8]);
        self.bytes[self.end + 2..end].copy_from_slice(data);
        self.end = end;
        if matches!(kind, 19 | 29) {
            self.authentication = Some(kind);
        }
        Ok(())
    }
}
