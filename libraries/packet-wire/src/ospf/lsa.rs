//! LSA 标识、摘要、请求和更新，以及独立于报文校验和的 Fletcher 校验。

use super::{Version, read_u16, read_u32};
use crate::PacketError;

const HEADER: usize = 20;
const REQUEST: usize = 12;

/// 在相应链路、区域或 AS 作用域内标识一条 LSA。
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct LsaKey {
    pub kind: u16,
    pub link_state_id: u32,
    pub advertising_router: u32,
}

/// 生成 LSA 时的公共字段。v3 不在公共头中携带选项。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LsaFields {
    pub key: LsaKey,
    pub age: u16,
    pub sequence: i32,
    pub v2_options: u8,
}

/// 可独立出现在数据库描述或确认中的 20 字节 LSA 公共头。
#[derive(Clone, Copy, Debug)]
pub struct LsaHeader<'a> {
    bytes: &'a [u8; HEADER],
    version: Version,
}

impl<'a> LsaHeader<'a> {
    /// 返回解释此摘要所用的 OSPF 版本。
    pub const fn version(self) -> Version {
        self.version
    }

    /// 读取独立摘要，所声明的长度可大于摘要本身。
    /// # Errors
    /// 输入不足 20 字节、额外数据或声明的 LSA 长度小于公共头。
    pub fn parse(version: Version, bytes: &'a [u8]) -> Result<Self, PacketError> {
        let header: &[u8; HEADER] = bytes.try_into().map_err(|_| {
            if bytes.len() < HEADER {
                PacketError::Truncated
            } else {
                PacketError::Malformed
            }
        })?;
        let header = Self {
            bytes: header,
            version,
        };
        if header.wire_len() < HEADER {
            return Err(PacketError::Malformed);
        }
        Ok(header)
    }

    /// 返回 LSA 的公共字段。序列号是有符号整数，不能以无符号顺序比较。
    pub fn fields(self) -> LsaFields {
        let (kind, options) = match self.version {
            Version::V2 => (u16::from(self.bytes[3]), self.bytes[2]),
            Version::V3 => (read_u16(self.bytes, 2), 0),
        };
        LsaFields {
            key: LsaKey {
                kind,
                link_state_id: read_u32(self.bytes, 4),
                advertising_router: read_u32(self.bytes, 8),
            },
            age: read_u16(self.bytes, 0),
            sequence: i32::from_be_bytes(
                self.bytes[12..16].try_into().expect("checked LSA header"),
            ),
            v2_options: options,
        }
    }

    /// 返回摘要中的 Fletcher 值。仅摘要无法重新验证该值。
    pub fn checksum(self) -> u16 {
        read_u16(self.bytes, 16)
    }

    /// 返回完整 LSA 的声明大小。
    pub fn wire_len(self) -> usize {
        usize::from(read_u16(self.bytes, 18))
    }

    /// 返回可用于数据库描述或确认的原始摘要字节。
    pub const fn bytes(self) -> &'a [u8; HEADER] {
        self.bytes
    }
}

/// 一条边界完整的 LSA，正文类型仍由协议服务或专用格式解码器解释。
#[derive(Clone, Copy, Debug)]
pub struct Lsa<'a> {
    header: LsaHeader<'a>,
    bytes: &'a [u8],
}

impl<'a> Lsa<'a> {
    pub(super) fn require_type(self, version: Version, kind: u16) -> Result<(), PacketError> {
        if self.header.version == version && self.header.fields().key.kind == kind {
            Ok(())
        } else {
            Err(PacketError::Unsupported)
        }
    }

    fn take(version: Version, input: &'a [u8]) -> Result<(Self, &'a [u8]), PacketError> {
        let header = LsaHeader::parse(version, input.get(..HEADER).ok_or(PacketError::Truncated)?)?;
        let (bytes, rest) = input
            .split_at_checked(header.wire_len())
            .ok_or(PacketError::Truncated)?;
        Ok((Self { header, bytes }, rest))
    }

    /// 读取一条完整 LSA。
    /// # Errors
    /// 长度非法、输入截断或含多余字节。
    pub fn parse(version: Version, bytes: &'a [u8]) -> Result<Self, PacketError> {
        let (lsa, rest) = Self::take(version, bytes)?;
        if !rest.is_empty() {
            return Err(PacketError::Malformed);
        }
        Ok(lsa)
    }

    /// 返回与此正文绑定的公共头。
    pub const fn header(self) -> LsaHeader<'a> {
        self.header
    }

    /// 返回 LSA 的原始字节，包括公共头。
    pub const fn bytes(self) -> &'a [u8] {
        self.bytes
    }

    /// 返回公共头之后的正文。
    pub fn body(self) -> &'a [u8] {
        &self.bytes[HEADER..]
    }

    /// 核对排除 LS age 的 Fletcher 校验，零值无效。
    /// # Errors
    /// 校验和不符。
    pub fn verify_checksum(self) -> Result<(), PacketError> {
        if self.header.checksum() == 0 || fletcher([&self.bytes[2..]]) != (0, 0) {
            return Err(PacketError::Checksum);
        }
        Ok(())
    }

    /// 在缓冲区写入 LSA 公共头并返回完整声明长度。正文由调用者填写。
    /// # Errors
    /// 长度、输出空间或版本字段不匹配。失败时不修改缓冲区。
    pub fn write_header(
        output: &mut [u8],
        version: Version,
        fields: LsaFields,
        body_len: usize,
    ) -> Result<usize, PacketError> {
        let kind = match version {
            Version::V2 => [
                fields.v2_options,
                u8::try_from(fields.key.kind).map_err(|_| PacketError::Malformed)?,
            ],
            Version::V3 if fields.v2_options == 0 => fields.key.kind.to_be_bytes(),
            Version::V3 => return Err(PacketError::Malformed),
        };
        let len = HEADER
            .checked_add(body_len)
            .and_then(|length| u16::try_from(length).ok())
            .ok_or(PacketError::Capacity)?;
        if output.len() < usize::from(len) {
            return Err(PacketError::Capacity);
        }
        output[..2].copy_from_slice(&fields.age.to_be_bytes());
        output[2..4].copy_from_slice(&kind);
        output[4..8].copy_from_slice(&fields.key.link_state_id.to_be_bytes());
        output[8..12].copy_from_slice(&fields.key.advertising_router.to_be_bytes());
        output[12..16].copy_from_slice(&fields.sequence.to_be_bytes());
        output[16..18].fill(0);
        output[18..HEADER].copy_from_slice(&len.to_be_bytes());
        Ok(usize::from(len))
    }

    /// 在原位填充 Fletcher 校验和，保留 LS age 和正文。
    /// # Errors
    /// LSA 的声明长度与输入不符。
    pub fn write_checksum(version: Version, bytes: &mut [u8]) -> Result<(), PacketError> {
        Lsa::parse(version, bytes)?;
        let (c0, c1) = fletcher([&bytes[2..16], &[0, 0], &bytes[18..]]);
        // RFC 2328 §12.1.7 所引用的 Fletcher 算法。第二个字节之后仍有 len - 18 字节。
        let weight = i32::try_from(bytes.len() - 17).expect("16-bit LSA length");
        let x = (weight * c0 - c1).rem_euclid(255);
        let y = (-c0 - x).rem_euclid(255);
        bytes[16..18].copy_from_slice(&[nonzero_residue(x), nonzero_residue(y)]);
        Ok(())
    }
}

fn fletcher<'a>(parts: impl IntoIterator<Item = &'a [u8]>) -> (i32, i32) {
    let (mut c0, mut c1) = (0u32, 0u32);
    for part in parts {
        // 每 4096 字节回卷，最坏 c1 仍低于 u32 上限，循环内无需除法。
        for block in part.chunks(4096) {
            for &byte in block {
                c0 += u32::from(byte);
                c1 += c0;
            }
            c0 %= 255;
            c1 %= 255;
        }
    }
    (c0 as i32, c1 as i32)
}

fn nonzero_residue(value: i32) -> u8 {
    if value == 0 { 255 } else { value as u8 }
}

/// 数据库描述与链路状态确认使用同一组定宽摘要。
#[derive(Clone, Copy, Debug)]
pub struct LsaSummaries<'a> {
    rows: &'a [[u8; HEADER]],
    version: Version,
}

impl<'a> LsaSummaries<'a> {
    pub(super) fn parse(version: Version, bytes: &'a [u8]) -> Result<Self, PacketError> {
        let (rows, remainder) = bytes.as_chunks::<HEADER>();
        if !remainder.is_empty()
            || rows
                .iter()
                .any(|row| usize::from(read_u16(row, 18)) < HEADER)
        {
            return Err(PacketError::Malformed);
        }
        Ok(Self { rows, version })
    }

    /// 按线序借用每条摘要，无需分配。
    pub fn iter(self) -> impl ExactSizeIterator<Item = LsaHeader<'a>> + 'a {
        self.rows.iter().map(move |bytes| LsaHeader {
            bytes,
            version: self.version,
        })
    }

    /// 写入一组同版本摘要，用于确认或数据库描述正文。
    /// # Errors
    /// 版本不一致或输出空间不足。失败时不修改缓冲区。
    pub fn write(
        output: &mut [u8],
        version: Version,
        headers: &[LsaHeader<'_>],
    ) -> Result<usize, PacketError> {
        let length = headers
            .len()
            .checked_mul(HEADER)
            .ok_or(PacketError::Capacity)?;
        if headers.iter().any(|header| header.version != version) {
            return Err(PacketError::Malformed);
        }
        let bytes = output.get_mut(..length).ok_or(PacketError::Capacity)?;
        for (row, header) in bytes.as_chunks_mut::<HEADER>().0.iter_mut().zip(headers) {
            row.copy_from_slice(header.bytes);
        }
        Ok(length)
    }
}

/// 链路状态请求包含一组 LSA 标识，不指定具体实例。
#[derive(Clone, Copy, Debug)]
pub struct LinkStateRequest<'a> {
    rows: &'a [[u8; REQUEST]],
    version: Version,
}

impl<'a> LinkStateRequest<'a> {
    pub(super) fn parse(version: Version, bytes: &'a [u8]) -> Result<Self, PacketError> {
        let (rows, remainder) = bytes.as_chunks::<REQUEST>();
        if !remainder.is_empty()
            || version == Version::V2
                && rows.iter().any(|row| read_u32(row, 0) > u32::from(u8::MAX))
        {
            return Err(PacketError::Malformed);
        }
        Ok(Self { rows, version })
    }

    /// 按线序遍历请求的 LSA 标识。
    pub fn iter(self) -> impl ExactSizeIterator<Item = LsaKey> + 'a {
        self.rows.iter().map(move |row| LsaKey {
            kind: match self.version {
                Version::V2 => u16::from(row[3]),
                Version::V3 => read_u16(row, 2),
            },
            link_state_id: read_u32(row, 4),
            advertising_router: read_u32(row, 8),
        })
    }

    /// 写入请求正文，v3 每行的保留字段为零。
    /// # Errors
    /// v2 类型字段超宽或输出空间不足。失败时不修改缓冲区。
    pub fn write(
        output: &mut [u8],
        version: Version,
        keys: &[LsaKey],
    ) -> Result<usize, PacketError> {
        let length = keys
            .len()
            .checked_mul(REQUEST)
            .ok_or(PacketError::Capacity)?;
        if version == Version::V2 && keys.iter().any(|key| key.kind > u16::from(u8::MAX)) {
            return Err(PacketError::Malformed);
        }
        let bytes = output.get_mut(..length).ok_or(PacketError::Capacity)?;
        for (row, key) in bytes.as_chunks_mut::<REQUEST>().0.iter_mut().zip(keys) {
            row[..4].copy_from_slice(&u32::from(key.kind).to_be_bytes());
            row[4..8].copy_from_slice(&key.link_state_id.to_be_bytes());
            row[8..12].copy_from_slice(&key.advertising_router.to_be_bytes());
        }
        Ok(length)
    }
}

/// 链路状态更新的已分帧 LSA 集合。LSA 校验和单独验证。
#[derive(Clone, Copy, Debug)]
pub struct LinkStateUpdate<'a> {
    bytes: &'a [u8],
    count: usize,
    version: Version,
}

impl<'a> LinkStateUpdate<'a> {
    pub(super) fn parse(version: Version, bytes: &'a [u8]) -> Result<Self, PacketError> {
        let (count, records) = bytes.split_at_checked(4).ok_or(PacketError::Truncated)?;
        let count = usize::try_from(read_u32(count, 0)).map_err(|_| PacketError::Malformed)?;
        if count > records.len() / HEADER {
            return Err(PacketError::Truncated);
        }
        let mut rest = records;
        for _ in 0..count {
            (_, rest) = Lsa::take(version, rest)?;
        }
        if !rest.is_empty() {
            return Err(PacketError::Malformed);
        }
        Ok(Self {
            bytes: records,
            count,
            version,
        })
    }

    /// 遍历已校验边界的 LSA，无需收集临时索引。
    pub fn iter(self) -> impl ExactSizeIterator<Item = Lsa<'a>> + 'a {
        let mut bytes = self.bytes;
        (0..self.count).map(move |_| {
            let (lsa, rest) = Lsa::take(self.version, bytes).expect("validated update boundaries");
            bytes = rest;
            lsa
        })
    }

    /// 写入更新正文及完整 LSA。调用者应先验证或填写各 LSA 的校验和。
    /// # Errors
    /// LSA 版本不一致、数量超宽或空间不足。失败时不修改缓冲区。
    pub fn write(
        output: &mut [u8],
        version: Version,
        records: &[Lsa<'_>],
    ) -> Result<usize, PacketError> {
        let count = u32::try_from(records.len()).map_err(|_| PacketError::Capacity)?;
        let length = records
            .iter()
            .try_fold(4usize, |size, record| size.checked_add(record.bytes.len()))
            .ok_or(PacketError::Capacity)?;
        if records
            .iter()
            .any(|record| record.header.version != version)
        {
            return Err(PacketError::Malformed);
        }
        let bytes = output.get_mut(..length).ok_or(PacketError::Capacity)?;
        bytes[..4].copy_from_slice(&count.to_be_bytes());
        let mut tail = &mut bytes[4..];
        for record in records {
            let (row, rest) = tail.split_at_mut(record.bytes.len());
            row.copy_from_slice(record.bytes);
            tail = rest;
        }
        Ok(length)
    }
}
