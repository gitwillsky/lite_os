//! 下一跳组的类型化成员与借用 framing；同一路径编解码用于完整集合和独立更新。
use super::*;

/// 一个带正整数权重的 IPv4 路径；权重只比较组内比例。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct NextHopMember {
    pub interface_index: u32,
    pub gateway: Option<core::net::Ipv4Addr>,
    pub on_link: bool,
    pub weight: u32,
}
impl NextHopMember {
    /// 用路径和前缀属性组成执行准入输入；不表示实际设备/网关已通过校验。
    /// # Parameters
    /// `attributes` 是与路径独立的路由属性。
    /// # Returns
    /// 完整的单路径准入请求；字段错误由执行 owner 返回。
    pub fn request(self, attributes: RouteAttributes) -> Ipv4RouteRequest {
        Ipv4RouteRequest {
            destination: attributes.destination,
            prefix_length: attributes.prefix_length,
            table: attributes.table,
            metric: attributes.metric,
            scope: attributes.scope,
            protocol: attributes.protocol,
            kind: attributes.kind,
            preferred_source: attributes.preferred_source,
            metrics: attributes.metrics,
            interface_index: Some(self.interface_index),
            gateway: self.gateway,
            on_link: self.on_link,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Members<'a> {
    Typed(&'a [NextHopMember]),
    Encoded(&'a [u8]),
}
/// 具名 IPv4 下一跳组。描述只借用成员，不分配或复制整组；单路径也是一成员组。
#[derive(Clone, Copy, Debug)]
pub struct NextHopConfiguration<'a> {
    pub id: u64,
    members: Members<'a>,
}
impl PartialEq for NextHopConfiguration<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id && self.members().eq(other.members())
    }
}
impl Eq for NextHopConfiguration<'_> {}
impl<'a> NextHopConfiguration<'a> {
    /// 从类型化成员借用配置；语义校验由编码器及执行 owner 完成。
    pub fn new(id: u64, members: &'a [NextHopMember]) -> Self {
        Self {
            id,
            members: Members::Typed(members),
        }
    }
    /// 只供冻结快照的已验证正文借用；parse 已检查每个成员和 14 字节边界，重复查询不再扫描全组。
    pub(super) fn from_validated_bytes(id: u64, bytes: &'a [u8]) -> Self {
        Self {
            id,
            members: Members::Encoded(bytes),
        }
    }
    /// 成员数；空组只可表示内部已撤回的运行状态，不可作为安装输入。
    pub fn len(self) -> usize {
        match self.members {
            Members::Typed(m) => m.len(),
            Members::Encoded(m) => m.len() / MEMBER_BYTES,
        }
    }
    /// 组是否没有路径。
    pub fn is_empty(self) -> bool {
        self.len() == 0
    }
    /// 借用成员的精确有界迭代器；不分配解析数组。
    pub fn members(self) -> NextHopMembers<'a> {
        NextHopMembers {
            members: self.members,
            index: 0,
        }
    }
    pub(super) fn encoded_size(self) -> Result<usize, Status> {
        let bytes = self
            .len()
            .checked_mul(MEMBER_BYTES)
            .ok_or(native_abi::ERR_OUT_OF_RANGE)?;
        u32::try_from(bytes).map_err(|_| native_abi::ERR_OUT_OF_RANGE)?;
        bytes.checked_add(12).ok_or(native_abi::ERR_OUT_OF_RANGE)
    }
    pub(super) fn validate(self) -> Result<(), Status> {
        if self.id == 0 || self.is_empty() || self.members().any(|member| member.weight == 0) {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        Ok(())
    }
    /// 编码一个独立更新的完整版本化正文，可经只读 VMO 传输。
    /// # Errors
    /// 零身份、空组、零权重、长度溢出或资源不足。
    pub fn encode(self) -> Result<Writer, Status> {
        self.validate()?;
        let mut writer = Writer::payload();
        writer.try_reserve(
            self.encoded_size()?
                .checked_add(4)
                .ok_or(native_abi::ERR_OUT_OF_RANGE)?,
        )?;
        writer.u32(VERSION);
        put_next_hop(&mut writer, self);
        Ok(writer)
    }
    /// 验证独立更新的版本、完整成员及包尾；只借用捕获的正文。
    /// # Errors
    /// 版本、身份、长度、成员字段或包尾不合法。
    pub fn parse(bytes: &'a [u8]) -> Result<Self, Status> {
        let mut reader = Reader::new(bytes);
        if reader.u32()? != VERSION {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        let result = get_next_hop(&mut reader)?;
        reader.finish()?;
        Ok(result)
    }
}
/// 不分配的类型化/字节成员投影；构造只能经配置或完整 framing 验证。
pub struct NextHopMembers<'a> {
    members: Members<'a>,
    index: usize,
}
impl Iterator for NextHopMembers<'_> {
    type Item = NextHopMember;
    fn next(&mut self) -> Option<Self::Item> {
        let member = match self.members {
            Members::Typed(m) => *m.get(self.index)?,
            Members::Encoded(m) => {
                let bytes = m.as_chunks::<MEMBER_BYTES>().0.get(self.index)?;
                get_member(&mut Reader::new(bytes)).expect("next-hop member framing was checked")
            }
        };
        self.index += 1;
        Some(member)
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        let total = match self.members {
            Members::Typed(m) => m.len(),
            Members::Encoded(m) => m.len() / MEMBER_BYTES,
        };
        let remaining = total - self.index;
        (remaining, Some(remaining))
    }
}
impl ExactSizeIterator for NextHopMembers<'_> {}
const MEMBER_BYTES: usize = 14;
pub(crate) fn put_next_hop(writer: &mut Writer, hop: NextHopConfiguration<'_>) {
    writer.u64(hop.id).u32((hop.len() * MEMBER_BYTES) as u32);
    for member in hop.members() {
        writer.u32(member.interface_index);
        put_option_ipv4(writer, member.gateway);
        writer.bool(member.on_link).u32(member.weight);
    }
}
fn get_member(reader: &mut Reader<'_>) -> Result<NextHopMember, Status> {
    let result = NextHopMember {
        interface_index: reader.u32()?,
        gateway: get_option_ipv4(reader)?,
        on_link: match reader.u8()? {
            0 => false,
            1 => true,
            _ => return Err(native_abi::ERR_INVALID_ARGS),
        },
        weight: reader.u32()?,
    };
    if result.weight == 0 {
        return Err(native_abi::ERR_INVALID_ARGS);
    }
    Ok(result)
}
pub(crate) fn get_next_hop<'a>(
    reader: &mut Reader<'a>,
) -> Result<NextHopConfiguration<'a>, Status> {
    let id = reader.u64()?;
    let bytes = reader.bytes()?;
    if id == 0 || bytes.is_empty() || bytes.len() % MEMBER_BYTES != 0 {
        return Err(native_abi::ERR_INVALID_ARGS);
    }
    for member in bytes.as_chunks::<MEMBER_BYTES>().0 {
        get_member(&mut Reader::new(member))?;
    }
    Ok(NextHopConfiguration {
        id,
        members: Members::Encoded(bytes),
    })
}
