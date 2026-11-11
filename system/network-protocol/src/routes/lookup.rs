//! 域内点查只传本次流的键和实际选择，不捕获 FIB、设备或邻居表。
use super::{RouteAddress, RouteKind};
use crate::{
    Reader, Writer,
    native_abi::{self, Status},
};
use core::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// 只读查询采用本机输出或指定入接口的转发语义。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RouteLookup {
    /// 本次发送的目的地址。
    pub destination: IpAddr,
    /// 同族源地址。本机输出可自动选源；IPv6 输入未指定源时只匹配无源前缀限制的规则。
    pub source: IpAddr,
    /// 路由查询的方向及接口约束。
    pub direction: RouteDirection,
    /// 报文标记，未标记流量为零。
    pub mark: u32,
}

/// 输入的收包设备与输出的可选绑定设备属于不同的查找上下文。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RouteDirection {
    /// 模拟报文从给定接口进入。
    Input(u32),
    /// 模拟本机发送，可绑定指定出口。
    Output(Option<u32>),
}

impl RouteLookup {
    pub(crate) const MAX_BYTES: usize = 46;

    pub(crate) fn write(self, writer: &mut Writer) -> Result<(), Status> {
        writer.u32(3);
        match (self.destination, self.source) {
            (IpAddr::V4(destination), IpAddr::V4(source)) => {
                writer.u8(4);
                destination.write(writer);
                source.write(writer);
            }
            (IpAddr::V6(destination), IpAddr::V6(source))
                if destination.to_ipv4_mapped().is_none() && source.to_ipv4_mapped().is_none() =>
            {
                writer.u8(6);
                destination.write(writer);
                source.write(writer);
            }
            _ => return Err(native_abi::ERR_INVALID_ARGS),
        }
        match self.direction {
            RouteDirection::Input(0) | RouteDirection::Output(Some(0)) => {
                return Err(native_abi::ERR_INVALID_ARGS);
            }
            RouteDirection::Input(index) => writer.u8(1).u32(index),
            RouteDirection::Output(index) => writer.u8(0).u32(index.unwrap_or(0)),
        };
        writer.u32(self.mark);
        Ok(())
    }
    pub(crate) fn read(reader: &mut Reader<'_>) -> Result<Self, Status> {
        if reader.u32()? != 3 {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        let (destination, source) = match reader.u8()? {
            4 => (
                Ipv4Addr::read(reader)?.into(),
                Ipv4Addr::read(reader)?.into(),
            ),
            6 => {
                let destination = Ipv6Addr::read(reader)?;
                let source = Ipv6Addr::read(reader)?;
                if destination.to_ipv4_mapped().is_some() || source.to_ipv4_mapped().is_some() {
                    return Err(native_abi::ERR_INVALID_ARGS);
                }
                (destination.into(), source.into())
            }
            _ => return Err(native_abi::ERR_INVALID_ARGS),
        };
        let direction = reader.u8()?;
        let index = reader.u32()?;
        Ok(Self {
            destination,
            source,
            mark: reader.u32()?,
            direction: match direction {
                0 => RouteDirection::Output((index != 0).then_some(index)),
                1 if index != 0 => RouteDirection::Input(index),
                _ => return Err(native_abi::ERR_INVALID_ARGS),
            },
        })
    }
}

/// 发送路径实际选出的一个出口。直达时没有网关，本地交付的出口为回环。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RoutePath {
    /// 生效的路由表。
    pub table: u32,
    /// 实际发送出口或本地交付的回环接口。
    pub interface_index: u32,
    /// 本地、广播或单播转发路径。
    pub kind: RouteKind,
    /// 输出时为选定的源地址，输入时保留原报文源地址。
    pub source: IpAddr,
    /// 立即网关，IPv4 也可携带 IPv6 网关。
    pub gateway: Option<IpAddr>,
}
impl RoutePath {
    pub(crate) fn write(self, writer: &mut Writer) {
        writer
            .u32(self.table)
            .u32(self.interface_index)
            .u8(self.kind.code());
        Ipv4Addr::write_gateway(writer, Some(self.source));
        Ipv4Addr::write_gateway(writer, self.gateway);
    }
    pub(crate) fn read(reader: &mut Reader<'_>, lookup: RouteLookup) -> Result<Self, Status> {
        let table = reader.u32()?;
        let interface_index = reader.u32()?;
        let kind = RouteKind::from_code(reader.u8()?).ok_or(native_abi::ERR_INVALID_ARGS)?;
        let source = Ipv4Addr::read_gateway(reader)?.ok_or(native_abi::ERR_INVALID_ARGS)?;
        let gateway = Ipv4Addr::read_gateway(reader)?;
        if table == 0
            || interface_index == 0
            || !matches!(
                kind,
                RouteKind::Unicast | RouteKind::Local | RouteKind::Broadcast
            )
            || source.is_ipv4() != lookup.destination.is_ipv4()
            || matches!(lookup.direction, RouteDirection::Input(_)) && source != lookup.source
            || gateway.is_some_and(|address| {
                address.is_unspecified() || lookup.destination.is_ipv6() && address.is_ipv4()
            })
        {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        Ok(Self {
            table,
            interface_index,
            kind,
            source,
            gateway,
        })
    }
}
