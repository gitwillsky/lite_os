//! RFC 1035 的非压缩 DNS 名称；供 DHCPv6/DNSSL 共用，不包含 DNS 消息、缓存或 resolver 策略。
use crate::PacketError;

/// 已验证普通 label 长度、root 结尾与 255-byte 总长的非压缩名称。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UncompressedName<'a> {
    bytes: &'a [u8],
}
impl<'a> UncompressedName<'a> {
    /// 解析 bytes 开头的一个完整名称，返回名称与其后的未消费输入；允许 root 名称。
    /// # Errors
    /// label/root 截断、label 超过 63、出现压缩/扩展 label 编码，或名称超过 255 字节。
    pub fn parse_prefix(bytes: &'a [u8]) -> Result<(Self, &'a [u8]), PacketError> {
        let mut offset = 0usize;
        loop {
            let length = usize::from(*bytes.get(offset).ok_or(PacketError::Truncated)?);
            if length > 63 {
                return Err(PacketError::Malformed);
            }
            offset += 1 + length;
            if offset > 255 {
                return Err(PacketError::Malformed);
            }
            if offset > bytes.len() {
                return Err(PacketError::Truncated);
            }
            if length == 0 {
                return Ok((
                    Self {
                        bytes: &bytes[..offset],
                    },
                    &bytes[offset..],
                ));
            }
        }
    }
    /// 完整原编码，包含末尾 root；不改变大小写或假定 label 为 UTF-8。
    pub fn bytes(self) -> &'a [u8] {
        self.bytes
    }
    /// 借用遍历各非 root label；显示、IDNA、域名适用策略由调用者负责。
    pub fn labels(self) -> Labels<'a> {
        Labels { bytes: self.bytes }
    }
}

/// 仅由已校验非压缩名称创建的 label 迭代器。
pub struct Labels<'a> {
    bytes: &'a [u8],
}
impl<'a> Iterator for Labels<'a> {
    type Item = &'a [u8];
    fn next(&mut self) -> Option<Self::Item> {
        let &length = self.bytes.first()?;
        if length == 0 {
            self.bytes = &[];
            return None;
        }
        let end = 1 + usize::from(length);
        let label = &self.bytes[1..end];
        self.bytes = &self.bytes[end..];
        Some(label)
    }
}

/// 完整普通名称序列；不包含 DNSSL 的末尾对齐 padding，不分配 label 或字符串副本。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UncompressedNames<'a> {
    bytes: &'a [u8],
}
impl<'a> UncompressedNames<'a> {
    /// 校验全部 bytes 是完整名称序列；空序列由所属选项决定是否允许。
    /// # Errors
    /// 任一名称的长度、编码或 root 结尾非法。
    pub fn parse(bytes: &'a [u8]) -> Result<Self, PacketError> {
        let mut rest = bytes;
        while !rest.is_empty() {
            rest = UncompressedName::parse_prefix(rest)?.1;
        }
        Ok(Self { bytes })
    }
    /// 校验 DNSSL 的名称序列，剥除最后一个完整名称之后至多七字节的零 padding。
    /// # Errors
    /// 列表为空、名称损坏，或最后的 padding 超过八字节对齐所需长度。
    /// 只在已消费一个完整名称后识别 padding，不能剥掉名称自身的 root 结尾。
    pub fn parse_padded(bytes: &'a [u8]) -> Result<Self, PacketError> {
        if bytes.is_empty() {
            return Err(PacketError::Malformed);
        }
        let mut rest = bytes;
        loop {
            rest = UncompressedName::parse_prefix(rest)?.1;
            if rest.is_empty() {
                return Ok(Self { bytes });
            }
            if rest[0] == 0 && rest.iter().all(|&byte| byte == 0) {
                if rest.len() > 7 {
                    return Err(PacketError::Malformed);
                }
                return Ok(Self {
                    bytes: &bytes[..bytes.len() - rest.len()],
                });
            }
        }
    }
    /// 尚未消费名称的精确编码，不包含已剥离的 padding。
    pub fn bytes(self) -> &'a [u8] {
        self.bytes
    }
}
impl<'a> Iterator for UncompressedNames<'a> {
    type Item = UncompressedName<'a>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.bytes.is_empty() {
            return None;
        }
        let (name, rest) =
            UncompressedName::parse_prefix(self.bytes).expect("validated name sequence");
        self.bytes = rest;
        Some(name)
    }
}

/// 编码 labels 并追加 root，返回精确名称字节数；label 是原始字节，不隐式执行文本/IDNA 转换。
/// # Errors
/// 非 root label 为空或超过 63 字节、名称超过 255 字节或 output 不足；失败不写 output。
pub fn emit(output: &mut [u8], labels: &[&[u8]]) -> Result<usize, PacketError> {
    // 1. 各 label 和总长度全部预检；空 labels 表示合法 root 名称。
    let mut length = 1usize;
    for label in labels {
        if label.is_empty() || label.len() > 63 {
            return Err(PacketError::Malformed);
        }
        length = length
            .checked_add(1 + label.len())
            .ok_or(PacketError::Capacity)?;
        if length > 255 {
            return Err(PacketError::Malformed);
        }
    }
    if output.len() < length {
        return Err(PacketError::Capacity);
    }
    // 2. 只写精确编码范围；所有 label 数据都在调用者借用中，不另建字符串。
    let mut offset = 0;
    for label in labels {
        output[offset] = label.len() as u8;
        output[offset + 1..offset + 1 + label.len()].copy_from_slice(label);
        offset += 1 + label.len();
    }
    output[offset] = 0;
    Ok(length)
}
