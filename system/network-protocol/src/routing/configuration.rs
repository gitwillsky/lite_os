//! RIB 配置意图版本 3：接口与可选静态来源；绝不解释为 NET 执行 FIB。
use alloc::vec::Vec;
use native_runtime::native_abi::{self, Status};
use router_protocol::interface::{InterfaceConfigurationBatch, Ipv4InterfaceConfiguration};
use service_rpc::{Reader, Writer};

const VERSION: u32 = 3;
const HEADER: usize = 28;

/// 冻结网络变更；没有接口的批次可只替换静态来源，None 表示保持，Some 空集合表示清除。
pub struct Configuration<'a> {
    interfaces: InterfaceConfigurationBatch<'a>,
    routes: Option<router_protocol::routing::Batch<'a>>,
    policy: Option<router_protocol::policy::Policy>,
}
impl<'a> Configuration<'a> {
    /// 编码接口意图与可选完整静态来源；None 保持来源，Some 空 Batch 撤回来源。
    /// # Errors
    /// 无变更、字段/正文/数量非法或资源不足；失败不返回部分正文。
    pub fn encode(
        interfaces: &[Ipv4InterfaceConfiguration],
        routes: Option<&[u8]>,
        policy: Option<&router_protocol::policy::Policy>,
    ) -> Result<Vec<u8>, Status> {
        if let Some(routes) = routes {
            router_protocol::routing::Batch::parse(routes)
                .map_err(|_| native_abi::ERR_INVALID_ARGS)?;
        }
        let policy = policy
            .map(router_protocol::policy::Policy::encode)
            .transpose()
            .map_err(policy_status)?;
        let writer = Self::encoder(
            interfaces,
            routes.map_or(0, <[u8]>::len),
            policy.as_ref().map_or(0, Vec::len),
        )?;
        let mut bytes = writer.into_bytes();
        if let Some(routes) = routes {
            bytes.extend_from_slice(routes);
        }
        if let Some(policy) = policy {
            bytes.extend_from_slice(&policy);
        }
        Ok(bytes)
    }
    fn encoder(
        interfaces: &[Ipv4InterfaceConfiguration],
        route_length: usize,
        policy_length: usize,
    ) -> Result<Writer, Status> {
        if interfaces.is_empty() && route_length == 0 && policy_length == 0 {
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
            .and_then(|length| length.checked_add(policy_length))
            .ok_or(native_abi::ERR_OUT_OF_RANGE)?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(length)
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        bytes.resize(prefix, 0);
        bytes[..4].copy_from_slice(&VERSION.to_le_bytes());
        bytes[4..12].copy_from_slice(&(interface_length as u64).to_le_bytes());
        bytes[12..20].copy_from_slice(&(route_length as u64).to_le_bytes());
        bytes[20..28].copy_from_slice(&(policy_length as u64).to_le_bytes());
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
        let policy_length =
            usize::try_from(reader.u64()?).map_err(|_| native_abi::ERR_OUT_OF_RANGE)?;
        let split = HEADER
            .checked_add(interface_length)
            .ok_or(native_abi::ERR_INVALID_ARGS)?;
        let policy_start = split
            .checked_add(route_length)
            .ok_or(native_abi::ERR_INVALID_ARGS)?;
        if interface_length == 0 && route_length == 0 && policy_length == 0
            || policy_start.checked_add(policy_length) != Some(bytes.len())
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
        let routes = if route_length == 0 {
            None
        } else {
            Some(
                router_protocol::routing::Batch::parse(&bytes[split..policy_start])
                    .map_err(|_| native_abi::ERR_INVALID_ARGS)?,
            )
        };
        let policy = if policy_length == 0 {
            None
        } else {
            Some(
                router_protocol::policy::Policy::decode(&bytes[policy_start..])
                    .map_err(policy_status)?,
            )
        };
        Ok(Self {
            interfaces,
            routes,
            policy,
        })
    }
    /// 无分配借用接口计划；没有接口变更时迭代器为空。
    pub fn interfaces(&self) -> InterfaceConfigurationBatch<'a> {
        self.interfaces
    }
    /// 可选的完整静态来源替换；None 不改变原静态来源，Some 空集合撤销该来源。
    pub fn routes(&self) -> Option<router_protocol::routing::Batch<'a>> {
        self.routes
    }
    /// 移交可选的完整策略；None 保持，显式缺省策略清除原限制。
    pub fn into_policy(self) -> Option<router_protocol::policy::Policy> {
        self.policy
    }
}

fn policy_status(error: router_protocol::policy::WireError) -> Status {
    match error {
        router_protocol::policy::WireError::Invalid => native_abi::ERR_INVALID_ARGS,
        router_protocol::policy::WireError::NoMemory => native_abi::ERR_NO_MEMORY,
    }
}
