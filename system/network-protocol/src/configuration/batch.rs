//! 单一网络配置捕获：接口与完整受管 FIB 共用同一正文、写租约及发布点。
use crate::routes::{NextHopConfiguration, RouteConfiguration, RouteConfigurationBatch};
use alloc::vec::Vec;
use native_runtime::native_abi::{self, Status};
use router_protocol::interface::{InterfaceConfigurationBatch, Ipv4InterfaceConfiguration};
use service_rpc::{Reader, Writer};

const VERSION: u32 = 1;
const HEADER: usize = 20;

/// 冻结网络变更；没有接口的批次可只替换 FIB，None FIB 表示不变，Some 空集合表示清除。
#[derive(Clone, Copy)]
pub struct ConfigurationBatch<'a> {
    interfaces: InterfaceConfigurationBatch<'a>,
    forwarding: Option<RouteConfigurationBatch<'a>>,
}
impl<'a> ConfigurationBatch<'a> {
    /// 编码一个复合变更；字段由原接口/FIB codec 拥有，不按成员/路由复制描述对象。
    /// # Parameters
    /// `interfaces` 可以为空；`forwarding` 是可选的完整下一跳/路由集合替换。
    /// # Returns
    /// 适合单一只读 VMO 的独占正文，不受 channel 大小限制。
    /// # Errors
    /// 没有任何变更、字段/长度非法或资源不足；失败不返回部分正文。
    pub fn encode(
        interfaces: &[Ipv4InterfaceConfiguration],
        forwarding: Option<(&[NextHopConfiguration<'_>], &[RouteConfiguration])>,
    ) -> Result<Vec<u8>, Status> {
        match forwarding {
            Some((hops, routes)) => {
                Self::encode_forwarding(interfaces, hops.iter().copied(), routes.iter().copied())
            }
            None => Ok(Self::encoder(interfaces, 0)?.into_bytes()),
        }
    }
    /// 从冻结借用投影编码接口与完整 FIB，不收集第二份逐前缀 DTO 数组。
    /// # Parameters
    /// `hops`/`routes` 必须在克隆/两次遍历间保持同一数量与值；空集合明确清除受管 FIB。
    /// `interfaces` 可为空；其字段仍由原接口 codec 验证。
    /// # Returns
    /// 只分配一次最终正文，全部字段复用同一 FIB codec。
    /// # Errors
    /// 字段/数量/长度非法或资源不足；不返回部分正文。
    pub fn encode_forwarding<'b>(
        interfaces: &[Ipv4InterfaceConfiguration],
        hops: impl Clone + ExactSizeIterator<Item = NextHopConfiguration<'b>>,
        routes: impl Clone + ExactSizeIterator<Item = RouteConfiguration>,
    ) -> Result<Vec<u8>, Status> {
        let route_length = RouteConfigurationBatch::encoded_size(hops.clone(), routes.clone())?;
        let mut writer = Self::encoder(interfaces, route_length)?;
        RouteConfigurationBatch::write_fields(&mut writer, hops, routes);
        Ok(writer.into_bytes())
    }
    fn encoder(
        interfaces: &[Ipv4InterfaceConfiguration],
        route_length: usize,
    ) -> Result<Writer, Status> {
        if interfaces.is_empty() && route_length == 0 {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        let interface_length = if interfaces.is_empty() {
            0
        } else {
            InterfaceConfigurationBatch::encoded_size(interfaces.len())
                .map_err(|_| native_abi::ERR_INVALID_ARGS)?
        };
        let prefix = HEADER
            .checked_add(interface_length)
            .ok_or(native_abi::ERR_OUT_OF_RANGE)?;
        let length = prefix
            .checked_add(route_length)
            .ok_or(native_abi::ERR_OUT_OF_RANGE)?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(length)
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        bytes.resize(prefix, 0);
        bytes[..4].copy_from_slice(&VERSION.to_le_bytes());
        bytes[4..12].copy_from_slice(&(interface_length as u64).to_le_bytes());
        bytes[12..20].copy_from_slice(&(route_length as u64).to_le_bytes());
        if interface_length != 0 {
            InterfaceConfigurationBatch::encode(interfaces, &mut bytes[HEADER..])
                .map_err(|_| native_abi::ERR_INVALID_ARGS)?;
        }
        Ok(Writer::from_bytes(bytes))
    }
    /// 完整验证外层长度及两个领域正文；所有引用只借用这一份已捕获的不可变字节。
    /// # Errors
    /// 未知版本、空变更、长度/字段错误、截断或包尾。
    pub fn parse(bytes: &'a [u8]) -> Result<Self, Status> {
        let mut reader = Reader::new(bytes);
        if reader.u32()? != VERSION {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        let interface_length =
            usize::try_from(reader.u64()?).map_err(|_| native_abi::ERR_OUT_OF_RANGE)?;
        let route_length =
            usize::try_from(reader.u64()?).map_err(|_| native_abi::ERR_OUT_OF_RANGE)?;
        let split = HEADER
            .checked_add(interface_length)
            .ok_or(native_abi::ERR_INVALID_ARGS)?;
        if interface_length == 0 && route_length == 0
            || split.checked_add(route_length) != Some(bytes.len())
        {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        let interface_bytes = bytes
            .get(HEADER..split)
            .ok_or(native_abi::ERR_INVALID_ARGS)?;
        let interfaces = if interface_bytes.is_empty() {
            InterfaceConfigurationBatch::default()
        } else {
            InterfaceConfigurationBatch::parse(interface_bytes)
                .map_err(|_| native_abi::ERR_INVALID_ARGS)?
        };
        let forwarding = if route_length == 0 {
            None
        } else {
            Some(RouteConfigurationBatch::parse(&bytes[split..])?)
        };
        Ok(Self {
            interfaces,
            forwarding,
        })
    }
    /// 无分配借用接口计划；没有接口变更时迭代器为空。
    pub fn interfaces(self) -> InterfaceConfigurationBatch<'a> {
        self.interfaces
    }
    /// 可选的完整 FIB 替换；None 不改变原受管集合，Some 空集合撤销该来源。
    pub fn forwarding(self) -> Option<RouteConfigurationBatch<'a>> {
        self.forwarding
    }
}
