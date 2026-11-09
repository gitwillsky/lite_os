//! DHCPv6 的 16-bit TLV；本层 framing 与类型内容分别验证，子容器不会递归解码。
use super::Dhcpv6Packet;
use crate::{PacketError, dns_name::UncompressedNames};
use core::net::Ipv6Addr;

/// framing 已校验的本层选项序列；保留重复实例、未知 code 和空值选项。
#[derive(Clone, Copy)]
pub struct Options<'a> {
    bytes: &'a [u8],
    offset: usize,
}
impl<'a> Options<'a> {
    /// 检查完整 bytes 中每个四字节 TLV 头及其 data 范围，不解析子选项内容。
    /// # Errors
    /// TLV 头或所声明的值截断；零长度值仍消费四字节头，不会形成死循环。
    pub fn parse(bytes: &'a [u8]) -> Result<Self, PacketError> {
        let mut rest = bytes;
        while !rest.is_empty() {
            if rest.len() < 4 {
                return Err(PacketError::Truncated);
            }
            let length = usize::from(u16::from_be_bytes([rest[2], rest[3]]));
            rest = rest.get(4 + length..).ok_or(PacketError::Truncated)?;
        }
        Ok(Self::validated(bytes))
    }
    pub(super) fn validated(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }
    /// 尚未消费部分的原 TLV 字节，可用于构造父容器，不包含该容器的固定字段。
    pub fn bytes(self) -> &'a [u8] {
        self.bytes
    }
    /// 取得本层唯一的 code；None 表示不存在，不替调用者规定所有选项都是 singleton。
    /// # Errors
    /// 同层存在重复 code；绝不拼接多个实例的数据区。
    pub fn single(self, code: u16) -> Result<Option<Dhcpv6Option<'a>>, PacketError> {
        let mut found = None;
        for option in self {
            if option.code() == code {
                if found.is_some() {
                    return Err(PacketError::Malformed);
                }
                found = Some(option);
            }
        }
        Ok(found)
    }
}
impl<'a> Iterator for Options<'a> {
    type Item = Dhcpv6Option<'a>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.bytes.is_empty() {
            return None;
        }
        let length = usize::from(u16::from_be_bytes([self.bytes[2], self.bytes[3]])) + 4;
        let (bytes, rest) = self.bytes.split_at(length);
        let offset = self.offset;
        self.offset += length;
        self.bytes = rest;
        Some(Dhcpv6Option { bytes, offset })
    }
}

/// 只证明本项 framing 的借用；decode 的内容错误不自动使同层其他选项失效。
#[derive(Clone, Copy)]
pub struct Dhcpv6Option<'a> {
    bytes: &'a [u8],
    offset: usize,
}
impl<'a> Dhcpv6Option<'a> {
    /// 原始 code，包含未知或废弃值。
    pub fn code(self) -> u16 {
        u16::from_be_bytes([self.bytes[0], self.bytes[1]])
    }
    /// 不包含四字节 TLV 头的精确值字节，不含其他重复实例。
    pub fn data(self) -> &'a [u8] {
        &self.bytes[4..]
    }
    /// 本项 Type 相对所属 Options 区起点的字节偏移；嵌套区域各自从零计。
    pub fn offset(self) -> usize {
        self.offset
    }
    /// 在 DHCP 标准命名空间解释本项，子容器只验证一层 framing，不递归解释它们的值。
    /// # Errors
    /// 已知字段宽度、DUID/UTF-8/前缀长度或子 TLV 非法；生命周期和选项适用位置由协议处理者验证。
    /// VendorInformation 内部使用厂商命名空间，不能用此方法冒充标准 DHCP 子选项语义。
    pub fn decode(self) -> Result<OptionValue<'a>, PacketError> {
        let data = self.data();
        Ok(match self.code() {
            1 => OptionValue::ClientIdentifier(Duid::parse(data)?),
            2 => OptionValue::ServerIdentifier(Duid::parse(data)?),
            3 | 25 => {
                require_minimum(data, 12)?;
                OptionValue::IdentityAssociation {
                    delegation: self.code() == 25,
                    identifier: word(data, 0),
                    renew: word(data, 4),
                    rebind: word(data, 8),
                    options: Options::parse(&data[12..])?,
                }
            }
            5 => {
                require_minimum(data, 24)?;
                OptionValue::Address {
                    address: address(&data[..16]),
                    preferred_lifetime: word(data, 16),
                    valid_lifetime: word(data, 20),
                    options: Options::parse(&data[24..])?,
                }
            }
            6 => {
                let (codes, tail) = data.as_chunks::<2>();
                if !tail.is_empty() {
                    return Err(PacketError::Malformed);
                }
                OptionValue::RequestedOptions(codes)
            }
            7 => {
                let [value] = exact(data)?;
                OptionValue::Preference(value)
            }
            8 => OptionValue::ElapsedTime(u16::from_be_bytes(exact(data)?)),
            9 => OptionValue::RelayMessage(Dhcpv6Packet::parse(data)?),
            11 => {
                require_minimum(data, 11)?;
                OptionValue::Authentication {
                    protocol: data[0],
                    algorithm: data[1],
                    replay_method: data[2],
                    replay: u64::from_be_bytes(
                        data[3..11].try_into().expect("validated replay field"),
                    ),
                    information: &data[11..],
                }
            }
            13 => {
                require_minimum(data, 2)?;
                OptionValue::Status {
                    code: u16::from_be_bytes([data[0], data[1]]),
                    message: core::str::from_utf8(&data[2..])
                        .map_err(|_| PacketError::Malformed)?,
                }
            }
            14 => {
                let [] = exact(data)?;
                OptionValue::RapidCommit
            }
            15 => OptionValue::UserClass(ClassData::parse(data)?),
            16 => {
                require_minimum(data, 4)?;
                OptionValue::VendorClass {
                    enterprise: word(data, 0),
                    classes: ClassData::parse(&data[4..])?,
                }
            }
            17 => {
                require_minimum(data, 4)?;
                OptionValue::VendorInformation {
                    enterprise: word(data, 0),
                    options: Options::parse(&data[4..])?,
                }
            }
            18 => OptionValue::InterfaceId(data),
            19 => {
                let [message_type] = exact(data)?;
                OptionValue::ReconfigureMessage(message_type)
            }
            20 => {
                let [] = exact(data)?;
                OptionValue::ReconfigureAccept
            }
            23 => {
                let (addresses, tail) = data.as_chunks::<16>();
                if !tail.is_empty() || addresses.is_empty() {
                    return Err(PacketError::Malformed);
                }
                OptionValue::DnsServers(addresses)
            }
            24 => {
                if data.is_empty() {
                    return Err(PacketError::Malformed);
                }
                OptionValue::DomainSearch(UncompressedNames::parse(data)?)
            }
            26 => {
                require_minimum(data, 25)?;
                if data[8] > 128 {
                    return Err(PacketError::Malformed);
                }
                OptionValue::Prefix {
                    preferred_lifetime: word(data, 0),
                    valid_lifetime: word(data, 4),
                    length: data[8],
                    prefix: address(&data[9..25]),
                    options: Options::parse(&data[25..])?,
                }
            }
            32 => OptionValue::InformationRefreshTime(u32::from_be_bytes(exact(data)?)),
            82 => OptionValue::SolicitMaximumTime(u32::from_be_bytes(exact(data)?)),
            83 => OptionValue::InformationMaximumTime(u32::from_be_bytes(exact(data)?)),
            code => OptionValue::Unknown { code, data },
        })
    }
}
fn require_minimum(bytes: &[u8], length: usize) -> Result<(), PacketError> {
    if bytes.len() < length {
        Err(PacketError::Truncated)
    } else {
        Ok(())
    }
}
fn exact<const N: usize>(bytes: &[u8]) -> Result<[u8; N], PacketError> {
    bytes.try_into().map_err(|_| PacketError::Malformed)
}
fn word(bytes: &[u8], start: usize) -> u32 {
    u32::from_be_bytes(bytes[start..start + 4].try_into().expect("validated word"))
}
fn address(bytes: &[u8]) -> Ipv6Addr {
    Ipv6Addr::from(<[u8; 16]>::try_from(bytes).expect("validated address"))
}

/// RFC 9915 的不透明 DUID：两字节类型后为 1..128 字节，不限制当前已知类型或提取 MAC。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Duid<'a> {
    bytes: &'a [u8],
}
impl<'a> Duid<'a> {
    /// 只验证 DUID 的总宽度；返回可直接进行字节相等比较的借用。
    /// # Errors
    /// 少于三字节或超过 130 字节；不为未知类型设置另一套识别规则。
    pub fn parse(bytes: &'a [u8]) -> Result<Self, PacketError> {
        if !(3..=130).contains(&bytes.len()) {
            return Err(PacketError::Malformed);
        }
        Ok(Self { bytes })
    }
    /// 完整不透明值，包括原始 Type，标识比较必须使用全部字节。
    pub fn bytes(self) -> &'a [u8] {
        self.bytes
    }
}

/// User/Vendor Class 中的 opaque 元素；每项以 u16 长度开头，不是 DHCP option TLV。
#[derive(Clone, Copy)]
pub struct ClassData<'a> {
    bytes: &'a [u8],
}
impl<'a> ClassData<'a> {
    /// 校验至少一项 class 值及全部长度，不解释文本编码或厂商含义；空值仍消费两字节长度字段。
    /// # Errors
    /// 列表为空、长度字段或内容截断。
    pub fn parse(bytes: &'a [u8]) -> Result<Self, PacketError> {
        if bytes.is_empty() {
            return Err(PacketError::Malformed);
        }
        let mut rest = bytes;
        while !rest.is_empty() {
            require_minimum(rest, 2)?;
            let length = usize::from(u16::from_be_bytes([rest[0], rest[1]]));
            rest = rest.get(2 + length..).ok_or(PacketError::Truncated)?;
        }
        Ok(Self { bytes })
    }
    /// 尚未消费项的原编码，可直接放入 User/Vendor Class。
    pub fn bytes(self) -> &'a [u8] {
        self.bytes
    }
}
impl<'a> Iterator for ClassData<'a> {
    type Item = &'a [u8];
    fn next(&mut self) -> Option<Self::Item> {
        if self.bytes.is_empty() {
            return None;
        }
        let length = usize::from(u16::from_be_bytes([self.bytes[0], self.bytes[1]]));
        let value = &self.bytes[2..2 + length];
        self.bytes = &self.bytes[2 + length..];
        Some(value)
    }
}

/// DHCPv6 基础/PD 选项的原值；容器只证明其下一层 framing，不代表租约或授权已经有效。
#[derive(Clone, Copy)]
pub enum OptionValue<'a> {
    ClientIdentifier(Duid<'a>),
    ServerIdentifier(Duid<'a>),
    IdentityAssociation {
        delegation: bool,
        identifier: u32,
        renew: u32,
        rebind: u32,
        options: Options<'a>,
    },
    Address {
        address: Ipv6Addr,
        preferred_lifetime: u32,
        valid_lifetime: u32,
        options: Options<'a>,
    },
    RequestedOptions(&'a [[u8; 2]]),
    Preference(u8),
    ElapsedTime(u16),
    RelayMessage(Dhcpv6Packet<'a>),
    /// 原始认证字段；MAC、算法选择和重放窗口由认证 owner 执行。
    Authentication {
        protocol: u8,
        algorithm: u8,
        replay_method: u8,
        replay: u64,
        information: &'a [u8],
    },
    Status {
        code: u16,
        message: &'a str,
    },
    RapidCommit,
    UserClass(ClassData<'a>),
    VendorClass {
        enterprise: u32,
        classes: ClassData<'a>,
    },
    /// 内部 option code 使用企业号拥有的命名空间。
    VendorInformation {
        enterprise: u32,
        options: Options<'a>,
    },
    InterfaceId(&'a [u8]),
    ReconfigureMessage(u8),
    ReconfigureAccept,
    DnsServers(&'a [[u8; 16]]),
    DomainSearch(UncompressedNames<'a>),
    Prefix {
        preferred_lifetime: u32,
        valid_lifetime: u32,
        length: u8,
        prefix: Ipv6Addr,
        options: Options<'a>,
    },
    InformationRefreshTime(u32),
    SolicitMaximumTime(u32),
    InformationMaximumTime(u32),
    /// 未解释的选项；包括 RFC 9915 已废弃的 IA_TA/Server Unicast，协议接收路径应按规范忽略。
    Unknown {
        code: u16,
        data: &'a [u8],
    },
}

impl OptionValue<'_> {
    /// 标准 code；同一布局的 IA_NA 与 IA_PD 仍保留不同类型。
    pub fn code(self) -> u16 {
        match self {
            Self::ClientIdentifier(_) => 1,
            Self::ServerIdentifier(_) => 2,
            Self::IdentityAssociation { delegation, .. } => {
                if delegation {
                    25
                } else {
                    3
                }
            }
            Self::Address { .. } => 5,
            Self::RequestedOptions(_) => 6,
            Self::Preference(_) => 7,
            Self::ElapsedTime(_) => 8,
            Self::RelayMessage(_) => 9,
            Self::Authentication { .. } => 11,
            Self::Status { .. } => 13,
            Self::RapidCommit => 14,
            Self::UserClass(_) => 15,
            Self::VendorClass { .. } => 16,
            Self::VendorInformation { .. } => 17,
            Self::InterfaceId(_) => 18,
            Self::ReconfigureMessage(_) => 19,
            Self::ReconfigureAccept => 20,
            Self::DnsServers(_) => 23,
            Self::DomainSearch(_) => 24,
            Self::Prefix { .. } => 26,
            Self::InformationRefreshTime(_) => 32,
            Self::SolicitMaximumTime(_) => 82,
            Self::InformationMaximumTime(_) => 83,
            Self::Unknown { code, .. } => code,
        }
    }
    fn data_length(self) -> Result<usize, PacketError> {
        let length = match self {
            Self::ClientIdentifier(duid) | Self::ServerIdentifier(duid) => Some(duid.bytes().len()),
            Self::IdentityAssociation { options, .. } => options.bytes().len().checked_add(12),
            Self::Address { options, .. } => options.bytes().len().checked_add(24),
            Self::RequestedOptions(codes) => Some(codes.as_flattened().len()),
            Self::Preference(_) => Some(1),
            Self::ElapsedTime(_) => Some(2),
            Self::RelayMessage(packet) => Some(packet.bytes().len()),
            Self::Authentication { information, .. } => information.len().checked_add(11),
            Self::Status { message, .. } => {
                // RFC 9915 的状态文本由 option-len 定界，发送时不得添加 NUL 终止符。
                if message.ends_with('\0') {
                    return Err(PacketError::Malformed);
                }
                message.len().checked_add(2)
            }
            Self::RapidCommit | Self::ReconfigureAccept => Some(0),
            Self::UserClass(classes) => {
                if classes.bytes().is_empty() {
                    return Err(PacketError::Malformed);
                }
                Some(classes.bytes().len())
            }
            Self::VendorClass { classes, .. } => {
                if classes.bytes().is_empty() {
                    return Err(PacketError::Malformed);
                }
                classes.bytes().len().checked_add(4)
            }
            Self::VendorInformation { options, .. } => options.bytes().len().checked_add(4),
            Self::InterfaceId(bytes) => Some(bytes.len()),
            Self::ReconfigureMessage(message_type) => {
                if !matches!(message_type, 5 | 6 | 11) {
                    return Err(PacketError::Malformed);
                }
                Some(1)
            }
            Self::DnsServers(addresses) => {
                if addresses.is_empty() {
                    return Err(PacketError::Malformed);
                }
                Some(addresses.as_flattened().len())
            }
            Self::DomainSearch(names) => {
                if names.bytes().is_empty() {
                    return Err(PacketError::Malformed);
                }
                Some(names.bytes().len())
            }
            Self::Prefix {
                length, options, ..
            } => {
                if length > 128 {
                    return Err(PacketError::Malformed);
                }
                options.bytes().len().checked_add(25)
            }
            Self::InformationRefreshTime(_)
            | Self::SolicitMaximumTime(_)
            | Self::InformationMaximumTime(_) => Some(4),
            Self::Unknown { code, data } => {
                if matches!(code, 4 | 12) {
                    return Err(PacketError::Unsupported);
                }
                if matches!(code, 1..=3 | 5..=9 | 11 | 13..=20 | 23..=26 | 32 | 82 | 83) {
                    return Err(PacketError::Malformed);
                }
                Some(data.len())
            }
        }
        .ok_or(PacketError::Capacity)?;
        if length > usize::from(u16::MAX) {
            return Err(PacketError::Capacity);
        }
        Ok(length)
    }
    /// 编码本项的 TLV 头和全部值，返回精确字节数；容器的既有子选项仅按原字节复制一次。
    /// # Errors
    /// 字段/Code/长度非法或输出不足；所有失败在写入前，output 的其余字节不改变。
    /// 原位构造整个网络包时，调用者可在所需偏移直接使用 OptionsBuilder，再由信封 emitter 加头。
    pub fn emit(self, output: &mut [u8]) -> Result<usize, PacketError> {
        // 1. 完成字段与容量预检，之后所有切片和固定字段写入都不会失败。
        let length = self.data_length()?;
        let bytes = output.get_mut(..length + 4).ok_or(PacketError::Capacity)?;
        bytes[..2].copy_from_slice(&self.code().to_be_bytes());
        bytes[2..4].copy_from_slice(&(length as u16).to_be_bytes());
        let data = &mut bytes[4..];
        // 2. 每个已发表字节都由当前值填写；没有未初始化 padding，也不引入临时分配。
        match self {
            Self::ClientIdentifier(duid) | Self::ServerIdentifier(duid) => {
                data.copy_from_slice(duid.bytes())
            }
            Self::IdentityAssociation {
                identifier,
                renew,
                rebind,
                options,
                ..
            } => {
                data[..4].copy_from_slice(&identifier.to_be_bytes());
                data[4..8].copy_from_slice(&renew.to_be_bytes());
                data[8..12].copy_from_slice(&rebind.to_be_bytes());
                data[12..].copy_from_slice(options.bytes());
            }
            Self::Address {
                address,
                preferred_lifetime,
                valid_lifetime,
                options,
            } => {
                data[..16].copy_from_slice(&address.octets());
                data[16..20].copy_from_slice(&preferred_lifetime.to_be_bytes());
                data[20..24].copy_from_slice(&valid_lifetime.to_be_bytes());
                data[24..].copy_from_slice(options.bytes());
            }
            Self::RequestedOptions(codes) => data.copy_from_slice(codes.as_flattened()),
            Self::Preference(value) | Self::ReconfigureMessage(value) => data[0] = value,
            Self::ElapsedTime(value) => data.copy_from_slice(&value.to_be_bytes()),
            Self::RelayMessage(packet) => data.copy_from_slice(packet.bytes()),
            Self::Authentication {
                protocol,
                algorithm,
                replay_method,
                replay,
                information,
            } => {
                data[..3].copy_from_slice(&[protocol, algorithm, replay_method]);
                data[3..11].copy_from_slice(&replay.to_be_bytes());
                data[11..].copy_from_slice(information);
            }
            Self::Status { code, message } => {
                data[..2].copy_from_slice(&code.to_be_bytes());
                data[2..].copy_from_slice(message.as_bytes());
            }
            Self::RapidCommit | Self::ReconfigureAccept => {}
            Self::UserClass(classes) => data.copy_from_slice(classes.bytes()),
            Self::VendorClass {
                enterprise,
                classes,
            } => {
                data[..4].copy_from_slice(&enterprise.to_be_bytes());
                data[4..].copy_from_slice(classes.bytes());
            }
            Self::VendorInformation {
                enterprise,
                options,
            } => {
                data[..4].copy_from_slice(&enterprise.to_be_bytes());
                data[4..].copy_from_slice(options.bytes());
            }
            Self::InterfaceId(bytes) | Self::Unknown { data: bytes, .. } => {
                data.copy_from_slice(bytes)
            }
            Self::DnsServers(addresses) => data.copy_from_slice(addresses.as_flattened()),
            Self::DomainSearch(names) => data.copy_from_slice(names.bytes()),
            Self::Prefix {
                preferred_lifetime,
                valid_lifetime,
                length,
                prefix,
                options,
            } => {
                data[..4].copy_from_slice(&preferred_lifetime.to_be_bytes());
                data[4..8].copy_from_slice(&valid_lifetime.to_be_bytes());
                data[8] = length;
                data[9..25].copy_from_slice(&prefix.octets());
                data[25..].copy_from_slice(options.bytes());
            }
            Self::InformationRefreshTime(value)
            | Self::SolicitMaximumTime(value)
            | Self::InformationMaximumTime(value) => data.copy_from_slice(&value.to_be_bytes()),
        }
        Ok(length + 4)
    }
}

/// 在调用者存储中追加选项；无 END/PAD 或跨选项拼接，不分配选项树。
pub struct OptionsBuilder<'a> {
    bytes: &'a mut [u8],
    end: usize,
}
impl<'a> OptionsBuilder<'a> {
    /// 借用输出存储，不预清零全部容量；finish 只暴露已经完整编码的前缀。
    pub fn new(bytes: &'a mut [u8]) -> Self {
        Self { bytes, end: 0 }
    }
    /// 原样追加一项 code/data，仅构造 TLV framing，不承诺已知内容或选项位置合法。
    /// # Errors
    /// data 超出 u16 长度或容量不足；失败不改变任何输出字节和已编码前缀。
    pub fn option(&mut self, code: u16, data: &[u8]) -> Result<(), PacketError> {
        let length = u16::try_from(data.len()).map_err(|_| PacketError::Capacity)?;
        let end = self
            .end
            .checked_add(4 + data.len())
            .ok_or(PacketError::Capacity)?;
        let output = self
            .bytes
            .get_mut(self.end..end)
            .ok_or(PacketError::Capacity)?;
        output[..2].copy_from_slice(&code.to_be_bytes());
        output[2..4].copy_from_slice(&length.to_be_bytes());
        output[4..].copy_from_slice(data);
        self.end = end;
        Ok(())
    }
    /// 追加类型化选项；字段/容量错误不改已有前缀，未发表的尾部也保持原值。
    /// # Errors
    /// 字段不可表达、未知输出冒充已知类型或容量不足。
    pub fn push(&mut self, value: OptionValue<'_>) -> Result<(), PacketError> {
        let length = value.emit(&mut self.bytes[self.end..])?;
        self.end += length;
        Ok(())
    }
    /// 返回精确已编码区域，不包含尚未使用的容量。
    pub fn finish(self) -> &'a mut [u8] {
        &mut self.bytes[..self.end]
    }
}
