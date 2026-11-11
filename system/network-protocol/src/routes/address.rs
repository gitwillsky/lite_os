//! 路由 wire 的编译期地址宽度；两族共用批次/快照算法，不做运行时地址截断或布局降级。
use super::{Reader, Status, Writer, native_abi};
use core::net::{IpAddr, Ipv4Addr, Ipv6Addr};

mod sealed {
    pub trait Sealed {}
    impl Sealed for super::Ipv4Addr {}
    impl Sealed for super::Ipv6Addr {}
}
/// 路由地址的静态族与字段编码；仅标准 IPv4/IPv6 可实现，不允许外部扩展破坏固定 framing。
pub trait RouteAddress: sealed::Sealed + Copy + core::fmt::Debug + Eq + Ord + 'static {
    /// 地址族允许的下一跳地址。IPv4 路由可经 IPv4 或 IPv6 邻接，IPv6 路由使用 IPv6 邻接。
    type Gateway: Copy + core::fmt::Debug + Eq + Ord + Into<IpAddr>;
    /// 原生族标签；不是 Linux AF_* 数值。
    const FAMILY: u32;
    /// 一个地址字段的精确字节数。
    const BYTES: usize;
    /// 缺席可选字段的唯一零值。
    const UNSPECIFIED: Self;
    /// 将单个地址写入已预留的最终存储，不分配临时正文。
    /// # Parameters
    /// writer 属于最终正文；调用者已按 BYTES 预留容量，self 为本族地址。
    /// # Returns
    /// 原位追加恰好 BYTES 字节，不创建地址副本或改变族。
    fn write(self, writer: &mut Writer);
    /// 读取本静态族的一个地址。
    /// # Parameters
    /// reader 是已通过外层族/版本检查的不可变正文游标。
    /// # Returns
    /// 消费恰好 BYTES 字节并返回完整地址。
    /// # Errors
    /// 截断返回 INVALID_ARGS，不降级或切换地址族。
    fn read(reader: &mut Reader<'_>) -> Result<Self, Status>;
    /// 把已解析的立即地址投影到本族允许的网关类型。
    fn gateway(address: IpAddr) -> Option<Self::Gateway>;
    /// 把可选网关编码为一字节标签及十六字节地址。
    fn write_gateway(writer: &mut Writer, address: Option<Self::Gateway>);
    /// 读取并检查可选网关的标签、地址族和填充。
    fn read_gateway(reader: &mut Reader<'_>) -> Result<Option<Self::Gateway>, Status>;
}
impl RouteAddress for Ipv4Addr {
    type Gateway = IpAddr;
    const FAMILY: u32 = 4;
    const BYTES: usize = 4;
    const UNSPECIFIED: Self = Self::UNSPECIFIED;
    fn write(self, writer: &mut Writer) {
        super::super::put_ipv4(writer, self);
    }
    fn read(reader: &mut Reader<'_>) -> Result<Self, Status> {
        super::super::get_ipv4(reader)
    }
    fn gateway(address: IpAddr) -> Option<IpAddr> {
        Some(address)
    }
    fn write_gateway(writer: &mut Writer, address: Option<IpAddr>) {
        match address {
            None => {
                writer.u8(0).u64(0).u64(0);
            }
            Some(IpAddr::V4(address)) => {
                writer.u8(4);
                address.write(writer);
                writer.u64(0).u32(0);
            }
            Some(IpAddr::V6(address)) => {
                writer.u8(6);
                address.write(writer);
            }
        }
    }
    fn read_gateway(reader: &mut Reader<'_>) -> Result<Option<IpAddr>, Status> {
        match reader.u8()? {
            0 if Ipv6Addr::read(reader)?.is_unspecified() => Ok(None),
            4 => {
                let address = Ipv4Addr::read(reader)?;
                if reader.u64()? != 0 || reader.u32()? != 0 {
                    return Err(native_abi::ERR_INVALID_ARGS);
                }
                Ok(Some(address.into()))
            }
            6 => Ipv6Addr::read(reader).map(|address| Some(address.into())),
            _ => Err(native_abi::ERR_INVALID_ARGS),
        }
    }
}
impl RouteAddress for Ipv6Addr {
    type Gateway = Ipv6Addr;
    const FAMILY: u32 = 6;
    const BYTES: usize = 16;
    const UNSPECIFIED: Self = Self::UNSPECIFIED;
    fn write(self, writer: &mut Writer) {
        for word in self.octets().as_chunks::<8>().0 {
            writer.u64(u64::from_be_bytes(*word));
        }
    }
    fn read(reader: &mut Reader<'_>) -> Result<Self, Status> {
        let mut bytes = [0; 16];
        for word in bytes.as_chunks_mut::<8>().0 {
            word.copy_from_slice(&reader.u64()?.to_be_bytes());
        }
        Ok(Self::from(bytes))
    }
    fn gateway(address: IpAddr) -> Option<Ipv6Addr> {
        match address {
            IpAddr::V4(_) => None,
            IpAddr::V6(address) => Some(address),
        }
    }
    fn write_gateway(writer: &mut Writer, address: Option<Ipv6Addr>) {
        Ipv4Addr::write_gateway(writer, address.map(IpAddr::V6));
    }
    fn read_gateway(reader: &mut Reader<'_>) -> Result<Option<Ipv6Addr>, Status> {
        Ipv4Addr::read_gateway(reader)?
            .map(|address| Self::gateway(address).ok_or(native_abi::ERR_INVALID_ARGS))
            .transpose()
    }
}
pub(super) fn put_optional<A: RouteAddress>(writer: &mut Writer, address: Option<A>) {
    writer.bool(address.is_some());
    address.unwrap_or(A::UNSPECIFIED).write(writer);
}
pub(super) fn get_optional<A: RouteAddress>(reader: &mut Reader<'_>) -> Result<Option<A>, Status> {
    let present = match reader.u8()? {
        0 => false,
        1 => true,
        _ => return Err(native_abi::ERR_INVALID_ARGS),
    };
    let address = A::read(reader)?;
    if !present && address != A::UNSPECIFIED {
        return Err(native_abi::ERR_INVALID_ARGS);
    }
    Ok(present.then_some(address))
}
