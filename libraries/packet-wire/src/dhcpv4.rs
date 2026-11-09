//! RFC 2131/2132 DHCPv4；RFC 3396 options→file→sname 顺序拼接，不分配选项副本。
use crate::PacketError;
use core::net::Ipv4Addr;

/// DHCPv4 客户端 UDP 端口。
pub const CLIENT_PORT: u16 = 68;
/// DHCPv4 服务端 UDP 端口。
pub const SERVER_PORT: u16 = 67;
const COOKIE: [u8; 4] = [99, 130, 83, 99];

/// RFC 2132 option 53 的消息类型；未知类型由调用者忽略，不映射为其他状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MessageType {
    Discover = 1,
    Offer = 2,
    Request = 3,
    Decline = 4,
    Ack = 5,
    Nak = 6,
    Release = 7,
    Inform = 8,
}
impl TryFrom<u8> for MessageType {
    type Error = PacketError;
    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Discover),
            2 => Ok(Self::Offer),
            3 => Ok(Self::Request),
            4 => Ok(Self::Decline),
            5 => Ok(Self::Ack),
            6 => Ok(Self::Nak),
            7 => Ok(Self::Release),
            8 => Ok(Self::Inform),
            _ => Err(PacketError::Unsupported),
        }
    }
}

/// 已验证固定头、cookie 和所有被启用选项区的 DHCPv4 借用。
#[derive(Clone, Copy)]
pub struct Dhcpv4Packet<'a> {
    bytes: &'a [u8],
    // RFC 2132 option 52 决定 file/sname 是选项还是原始字符串；未启用时不能解析这些字段。
    overload: u8,
}
impl<'a> Dhcpv4Packet<'a> {
    /// 解析完整 UDP payload，不要求理解未知选项；不在此决定租约或客户端授权。
    /// # Errors
    /// 截断、op/hlen/cookie 错误、TLV 越界、缺失 END 或 option overload 非法。
    pub fn parse(bytes: &'a [u8]) -> Result<Self, PacketError> {
        if bytes.len() < 240 {
            return Err(PacketError::Truncated);
        }
        if !matches!(bytes[0], 1 | 2) || bytes[2] > 16 {
            return Err(PacketError::Malformed);
        }
        if bytes[236..240] != COOKIE {
            return Err(PacketError::Malformed);
        }
        validate_options(&bytes[240..])?;
        let mut packet = Self { bytes, overload: 0 };
        if let Some(value) = packet.option(52) {
            let [overload] = value.array()?;
            if !(1..=3).contains(&overload) {
                return Err(PacketError::Malformed);
            }
            packet.overload = overload;
        }
        let options = packet.options();
        for area in &options.areas[1..] {
            if !area.is_empty() {
                validate_options(area)?;
                if Options::new([area, &[], &[]]).any(|option| option.code == 52) {
                    return Err(PacketError::Malformed);
                }
            }
        }
        Ok(packet)
    }
    /// BOOTP op（1 请求，2 回复）。
    pub fn operation(self) -> u8 {
        self.bytes[0]
    }
    /// ARP hardware type；Ethernet 为 1。
    pub fn hardware_type(self) -> u8 {
        self.bytes[1]
    }
    /// 事务 ID。
    pub fn transaction_id(self) -> u32 {
        u32::from_be_bytes(self.bytes[4..8].try_into().expect("validated header"))
    }
    /// hlen 限定的 chaddr，不含 16 字节区域的 padding。
    pub fn hardware_address(self) -> &'a [u8] {
        &self.bytes[28..28 + usize::from(self.bytes[2])]
    }
    /// ciaddr；客户端已有地址时使用。
    pub fn client_address(self) -> Ipv4Addr {
        self.address(12)
    }
    /// yiaddr；服务端提供的客户端地址。
    pub fn your_address(self) -> Ipv4Addr {
        self.address(16)
    }
    /// giaddr；中继地址。
    pub fn relay_address(self) -> Ipv4Addr {
        self.address(24)
    }
    /// RFC 2132 消息类型；缺失/重复拼接后长度错误或未定义类型均返回错误。
    pub fn message_type(self) -> Result<MessageType, PacketError> {
        let [kind] = self.option(53).ok_or(PacketError::Malformed)?.array()?;
        kind.try_into()
    }
    /// 按主 options、file、sname 次序遍历原始 TLV，不包含 PAD/END。
    pub fn options(self) -> Options<'a> {
        Options::new([
            &self.bytes[240..],
            if self.overload & 1 != 0 {
                &self.bytes[108..236]
            } else {
                &[]
            },
            if self.overload & 2 != 0 {
                &self.bytes[44..108]
            } else {
                &[]
            },
        ])
    }
    /// 返回 `code` 的逻辑选项，按 RFC 3396 拼接同码各段；缺失返回 None。
    pub fn option(self, code: u8) -> Option<OptionValue<'a>> {
        let mut found = false;
        let length = self
            .options()
            .filter(|option| option.code == code)
            .map(|option| {
                found = true;
                option.data.len()
            })
            .sum();
        found.then_some(OptionValue {
            options: self.options(),
            code,
            length,
        })
    }
    fn address(self, offset: usize) -> Ipv4Addr {
        Ipv4Addr::from(
            <[u8; 4]>::try_from(&self.bytes[offset..offset + 4]).expect("validated address"),
        )
    }
}

/// 一个原始 option 片段；同 code 可以有多段，语义读取应使用 OptionValue。
#[derive(Clone, Copy)]
pub struct DhcpOption<'a> {
    pub code: u8,
    pub data: &'a [u8],
}
/// 已验证区域的线性游标，不重复检查已验证的 TLV 边界，不拥有报文字节。
#[derive(Clone)]
pub struct Options<'a> {
    areas: [&'a [u8]; 3],
    area: usize,
    offset: usize,
}
impl<'a> Options<'a> {
    fn new(areas: [&'a [u8]; 3]) -> Self {
        Self {
            areas,
            area: 0,
            offset: 0,
        }
    }
}
impl<'a> Iterator for Options<'a> {
    type Item = DhcpOption<'a>;
    fn next(&mut self) -> Option<Self::Item> {
        while self.area < 3 {
            let bytes = self.areas[self.area];
            if self.offset == bytes.len() || bytes[self.offset] == 255 {
                self.area += 1;
                self.offset = 0;
                continue;
            }
            let code = bytes[self.offset];
            self.offset += 1;
            if code == 0 {
                continue;
            }
            let length = usize::from(bytes[self.offset]);
            self.offset += 1;
            let data = &bytes[self.offset..self.offset + length];
            self.offset += length;
            return Some(DhcpOption { code, data });
        }
        None
    }
}

/// 逻辑选项的已验证分段借用；长度是各段之和，不创建拼接 Vec。
#[derive(Clone)]
pub struct OptionValue<'a> {
    options: Options<'a>,
    code: u8,
    length: usize,
}
impl OptionValue<'_> {
    /// 拼接后的字节数。
    pub fn len(&self) -> usize {
        self.length
    }
    /// 是否为空选项。
    pub fn is_empty(&self) -> bool {
        self.length == 0
    }
    /// 按 RFC 3396 顺序读取各字节，不依赖片段恰好位于字段边界。
    pub fn bytes(self) -> impl Iterator<Item = u8> {
        self.options
            .filter(move |option| option.code == self.code)
            .flat_map(|option| option.data.iter().copied())
    }
    /// 读取固定长度字段；拼接后的长度必须恰好为 N。
    /// # Errors
    /// 选项大小与该字段的标准长度不同。
    pub fn array<const N: usize>(self) -> Result<[u8; N], PacketError> {
        if self.length != N {
            return Err(PacketError::Malformed);
        }
        let mut result = [0; N];
        for (target, byte) in result.iter_mut().zip(self.bytes()) {
            *target = byte;
        }
        Ok(result)
    }
    /// 复制到调用者提供的固定空间，返回有效字节数；不写未使用尾部。
    /// # Errors
    /// 拼接结果超过输出空间时不写入。
    pub fn copy_to(self, output: &mut [u8]) -> Result<usize, PacketError> {
        let length = self.length;
        if output.len() < length {
            return Err(PacketError::Capacity);
        }
        for (target, byte) in output.iter_mut().zip(self.bytes()) {
            *target = byte;
        }
        Ok(length)
    }
    /// 读取 Router/DNS 等 IPv4 地址列表，接受地址跨两个 TLV 片段的编码。
    /// # Errors
    /// 长度为零或不是 4 的倍数。
    pub fn ipv4_addresses(self) -> Result<impl Iterator<Item = Ipv4Addr>, PacketError> {
        if self.length == 0 || !self.length.is_multiple_of(4) {
            return Err(PacketError::Malformed);
        }
        let mut bytes = self.bytes();
        Ok(core::iter::from_fn(move || {
            Some(Ipv4Addr::from([
                bytes.next()?,
                bytes.next()?,
                bytes.next()?,
                bytes.next()?,
            ]))
        }))
    }
}

/// BOOTP 固定字段；sname/file 可为原始字符串，或与 option 52 一起编码为以 END 结束的 TLV。
pub struct Dhcpv4Header<'a> {
    pub operation: u8,
    pub hardware_type: u8,
    pub hardware_address: &'a [u8],
    pub hops: u8,
    pub transaction_id: u32,
    pub seconds: u16,
    pub broadcast: bool,
    pub client_address: Ipv4Addr,
    pub your_address: Ipv4Addr,
    pub server_address: Ipv4Addr,
    pub relay_address: Ipv4Addr,
    pub server_name: &'a [u8],
    pub boot_file: &'a [u8],
}

/// 在调用者的 UDP payload 存储中编码 DHCP；仅清零标准固定头，不预清零整个 MTU。
pub struct Dhcpv4Builder<'a> {
    bytes: &'a mut [u8],
    end: usize,
}
impl<'a> Dhcpv4Builder<'a> {
    /// 填写 `header` 与 cookie，借用 `bytes` 直到 finish；失败时不写入。
    /// # Errors
    /// 空间不足、op 非法或固定字段过长。
    pub fn new(bytes: &'a mut [u8], header: &Dhcpv4Header<'_>) -> Result<Self, PacketError> {
        if bytes.len() < 241 {
            return Err(PacketError::Capacity);
        }
        if !matches!(header.operation, 1 | 2)
            || header.hardware_address.len() > 16
            || header.server_name.len() > 64
            || header.boot_file.len() > 128
        {
            return Err(PacketError::Malformed);
        }
        bytes[..240].fill(0);
        bytes[0] = header.operation;
        bytes[1] = header.hardware_type;
        bytes[2] = header.hardware_address.len() as u8;
        bytes[3] = header.hops;
        bytes[4..8].copy_from_slice(&header.transaction_id.to_be_bytes());
        bytes[8..10].copy_from_slice(&header.seconds.to_be_bytes());
        bytes[10..12].copy_from_slice(&(u16::from(header.broadcast) << 15).to_be_bytes());
        for (start, address) in [
            (12, header.client_address),
            (16, header.your_address),
            (20, header.server_address),
            (24, header.relay_address),
        ] {
            bytes[start..start + 4].copy_from_slice(&address.octets());
        }
        bytes[28..28 + header.hardware_address.len()].copy_from_slice(header.hardware_address);
        bytes[44..44 + header.server_name.len()].copy_from_slice(header.server_name);
        bytes[108..108 + header.boot_file.len()].copy_from_slice(header.boot_file);
        bytes[236..240].copy_from_slice(&COOKIE);
        Ok(Self { bytes, end: 240 })
    }
    /// 追加逻辑选项；超过 255 字节时按 RFC 3396 顺序分段，始终预留 END 空间。
    /// # Errors
    /// code 为 PAD/END 或容量不足；失败不改变已有前缀。
    pub fn option(&mut self, code: u8, data: &[u8]) -> Result<(), PacketError> {
        if code == 0 || code == 255 {
            return Err(PacketError::Malformed);
        }
        let overhead = data.len().div_ceil(255).max(1) * 2;
        let end = self
            .end
            .checked_add(data.len())
            .and_then(|end| end.checked_add(overhead))
            .ok_or(PacketError::Capacity)?;
        if end >= self.bytes.len() {
            return Err(PacketError::Capacity);
        }
        if data.is_empty() {
            self.bytes[self.end..self.end + 2].copy_from_slice(&[code, 0]);
        } else {
            let mut offset = self.end;
            for fragment in data.chunks(255) {
                self.bytes[offset..offset + 2].copy_from_slice(&[code, fragment.len() as u8]);
                self.bytes[offset + 2..offset + 2 + fragment.len()].copy_from_slice(fragment);
                offset += 2 + fragment.len();
            }
        }
        self.end = end;
        Ok(())
    }
    /// 写入 END 并验证最终选项/overload；返回精确 payload，不包含未使用空间。
    /// # Errors
    /// 选项或 overload 不一致；错误后调用者须丢弃尚未发表的报文。
    pub fn finish(self) -> Result<&'a mut [u8], PacketError> {
        self.bytes[self.end] = 255;
        let bytes = &mut self.bytes[..self.end + 1];
        Dhcpv4Packet::parse(bytes)?;
        Ok(bytes)
    }
}
fn validate_options(mut bytes: &[u8]) -> Result<(), PacketError> {
    while let Some((&code, tail)) = bytes.split_first() {
        match code {
            255 => return Ok(()),
            0 => bytes = tail,
            _ => {
                let length = usize::from(*tail.first().ok_or(PacketError::Truncated)?);
                bytes = tail.get(1 + length..).ok_or(PacketError::Truncated)?;
            }
        }
    }
    Err(PacketError::Malformed)
}
