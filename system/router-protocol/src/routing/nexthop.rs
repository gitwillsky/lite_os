//! 候选下一跳可以引用递归地址，执行侧 FIB 不得接收未经解析的声明。
use super::*;

/// 已选路前缀的动作；丢弃/错误动作不是可供其他路由递归解析的出口。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Action {
    /// 按下一跳组转发。
    Forward,
    /// 静默丢弃。
    Discard,
    /// 目的不可达。
    Unreachable,
    /// 管理策略禁止。
    Prohibit,
}
/// 直接出口与递归目标明确分开，不以零接口或零地址暗示另一种语义。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Target {
    /// 出口由来源声明；最终仍需执行服务依据设备/地址事实准入。
    Direct {
        /// 非零运行时接口身份。
        interface_index: u32,
        /// None 表示直接相连前缀，Some 表示立即下一跳。
        gateway: Option<IpAddr>,
        /// 显式声明网关在链路上，不跳过设备身份/状态准入。
        on_link: bool,
    },
    /// 在同一路由实例/地址族内递归解析；不是已经确认可达的立即下一跳。
    /// 本变体没有 zone 身份，IPv6 link-local 必须用带接口的 Direct 表达，否则可能解析到错误链路。
    Recursive(IpAddr),
}
/// 组内正整数权重；递归展开不得截断或近似其比例。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Member {
    /// 立即出口或递归地址。
    pub target: Target,
    /// 正整数；具体比例只在同一组内比较。
    pub weight: u32,
}
#[derive(Clone, Copy)]
enum Storage<'a> {
    Typed(&'a [Member]),
    Encoded(&'a [u8], Family),
}
/// 一个共享候选下一跳对象；只借用成员，终止动作必须没有成员。
#[derive(Clone, Copy)]
pub struct NextHop<'a> {
    action: Action,
    storage: Storage<'a>,
}
impl<'a> NextHop<'a> {
    /// 借用声明；编码/服务捕获必须验证动作和成员，构造本身不授予可达性。
    pub fn new(action: Action, members: &'a [Member]) -> Self {
        Self {
            action,
            storage: Storage::Typed(members),
        }
    }
    /// 前缀动作。
    pub fn action(self) -> Action {
        self.action
    }
    /// 成员数；终止动作必须为零，Forward 必须非零。
    pub fn len(self) -> usize {
        match self.storage {
            Storage::Typed(members) => members.len(),
            Storage::Encoded(bytes, family) => bytes.len() / (family.width() + 10),
        }
    }
    /// 是否没有成员。
    pub fn is_empty(self) -> bool {
        self.len() == 0
    }
    /// 完整验证后的借用成员迭代器；Typed 输入的语义校验仍由编码入口完成。
    pub fn members(self) -> Members<'a> {
        Members {
            storage: self.storage,
            index: 0,
        }
    }
    pub(super) fn encoded_size(self, family: Family) -> Result<usize, Invalid> {
        u32::try_from(self.len()).map_err(|_| Invalid)?;
        self.len()
            .checked_mul(family.width() + 10)
            .and_then(|bytes| bytes.checked_add(5))
            .ok_or(Invalid)
    }
    pub(super) fn validate(self, family: Family) -> Result<(), Invalid> {
        if (self.action == Action::Forward) == self.is_empty() {
            return Err(Invalid);
        }
        match self.storage {
            Storage::Typed(members) => {
                for member in members {
                    validate_member(*member, family)?;
                }
            }
            Storage::Encoded(bytes, _) => {
                for entry in bytes.chunks_exact(family.width() + 10) {
                    validate_member(get_member(entry, family)?, family)?;
                }
            }
        }
        Ok(())
    }
}
/// 借用类型化/已验证编码成员，不分配或重复验证整组。
pub struct Members<'a> {
    storage: Storage<'a>,
    index: usize,
}
impl Iterator for Members<'_> {
    type Item = Member;
    fn next(&mut self) -> Option<Self::Item> {
        let member = match self.storage {
            Storage::Typed(members) => *members.get(self.index)?,
            Storage::Encoded(bytes, family) => {
                let size = family.width() + 10;
                let start = self.index.checked_mul(size)?;
                get_member(bytes.get(start..start.checked_add(size)?)?, family)
                    .expect("fully validated member")
            }
        };
        self.index += 1;
        Some(member)
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        let length = match self.storage {
            Storage::Typed(members) => members.len(),
            Storage::Encoded(bytes, family) => bytes.len() / (family.width() + 10),
        } - self.index;
        (length, Some(length))
    }
}
impl ExactSizeIterator for Members<'_> {}
fn valid_gateway(address: IpAddr, family: Family) -> bool {
    family.contains(address)
        && !address.is_unspecified()
        && !address.is_multicast()
        && !matches!(address, IpAddr::V4(address) if address.is_broadcast())
}
fn validate_member(member: Member, family: Family) -> Result<(), Invalid> {
    let valid = match member.target {
        Target::Direct {
            interface_index,
            gateway,
            on_link,
        } => {
            interface_index != 0
                && !(on_link && gateway.is_none())
                && gateway.is_none_or(|address| valid_gateway(address, family))
        }
        Target::Recursive(address) => {
            valid_gateway(address, family)
                && !matches!(address, IpAddr::V6(address) if address.is_unicast_link_local())
        }
    };
    if member.weight == 0 || !valid {
        return Err(Invalid);
    }
    Ok(())
}
pub(super) fn put_hop(writer: &mut Encoder, hop: NextHop<'_>, family: Family) {
    writer
        .u8(match hop.action {
            Action::Forward => 0,
            Action::Discard => 1,
            Action::Unreachable => 2,
            Action::Prohibit => 3,
        })
        .u32(hop.len() as u32);
    for member in hop.members() {
        writer.u32(member.weight);
        match member.target {
            Target::Direct {
                interface_index,
                gateway,
                on_link,
            } => {
                writer
                    .u8(0)
                    .u32(interface_index)
                    .u8(u8::from(gateway.is_some()) | (u8::from(on_link) << 1));
                put_address(writer, gateway.unwrap_or_else(|| family.zero()));
            }
            Target::Recursive(address) => {
                writer.u8(1).u32(0).u8(0);
                put_address(writer, address);
            }
        }
    }
}
pub(super) fn get_hop<'a>(
    reader: &mut Decoder<'a>,
    family: Family,
) -> Result<NextHop<'a>, Invalid> {
    let action = match reader.u8()? {
        0 => Action::Forward,
        1 => Action::Discard,
        2 => Action::Unreachable,
        3 => Action::Prohibit,
        _ => return Err(Invalid),
    };
    let count = reader.u32()? as usize;
    let bytes = reader.take(count.checked_mul(family.width() + 10).ok_or(Invalid)?)?;
    Ok(NextHop {
        action,
        storage: Storage::Encoded(bytes, family),
    })
}
fn get_member(bytes: &[u8], family: Family) -> Result<Member, Invalid> {
    let mut reader = Decoder::new(bytes);
    let weight = reader.u32()?;
    let kind = reader.u8()?;
    let interface_index = reader.u32()?;
    let flags = reader.u8()?;
    let address = get_address(&mut reader, family)?;
    reader.finish()?;
    let target = match kind {
        0 if flags & !3 == 0 && (flags & 1 != 0 || address == family.zero()) => Target::Direct {
            interface_index,
            gateway: (flags & 1 != 0).then_some(address),
            on_link: flags & 2 != 0,
        },
        1 if interface_index == 0 && flags == 0 => Target::Recursive(address),
        _ => return Err(Invalid),
    };
    Ok(Member { target, weight })
}
