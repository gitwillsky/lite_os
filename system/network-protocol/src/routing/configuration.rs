//! RIB 配置意图：接口、静态来源、准入策略及备选路由表。
use alloc::vec::Vec;
use native_runtime::native_abi::{self, Status};
use router_protocol::{
    interface::{InterfaceConfiguration, InterfaceConfigurationBatch},
    policy_routing::Tables,
    routing::{Batch, Family},
};
use service_rpc::Reader;
const VERSION: u32 = 5;
const HEADER: usize = 44;
/// 同一持久决定的接口/双族来源/策略；缺席保持，显式空来源清除，不接受跨族正文。
pub struct Configuration<'a> {
    interfaces: InterfaceConfigurationBatch<'a>,
    routes: [Option<Batch<'a>>; 2],
    policy: Option<router_protocol::policy::Policy>,
    tables: Option<Tables<'a>>,
}
impl<'a> Configuration<'a> {
    /// 编码接口与按 IPv4/IPv6 排列的完整来源；空字段保持，非空的空 Batch 撤回。
    /// # Errors
    /// 全部缺席、字段/地址族/长度非法或内存不足，不返回部分正文。
    pub fn encode(
        interfaces: &[InterfaceConfiguration],
        routes: [Option<&[u8]>; 2],
        policy: Option<&router_protocol::policy::Policy>,
        tables: Option<&[u8]>,
    ) -> Result<Vec<u8>, Status> {
        for (body, family) in routes.into_iter().zip([Family::Ipv4, Family::Ipv6]) {
            if let Some(body) = body
                && Batch::parse(body)
                    .map_err(|_| native_abi::ERR_INVALID_ARGS)?
                    .family()
                    != family
            {
                return Err(native_abi::ERR_INVALID_ARGS);
            }
        }
        let policy = policy
            .map(router_protocol::policy::Policy::encode)
            .transpose()
            .map_err(policy_status)?;
        let sizes = [
            routes[0].map_or(0, <[u8]>::len),
            routes[1].map_or(0, <[u8]>::len),
            policy.as_ref().map_or(0, Vec::len),
            tables.map_or(0, <[u8]>::len),
        ];
        if interfaces.is_empty() && sizes == [0; 4] || tables.is_some_and(<[u8]>::is_empty) {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        let interface_size = if interfaces.is_empty() {
            0
        } else {
            InterfaceConfigurationBatch::encoded_size(interfaces)
                .map_err(|_| native_abi::ERR_INVALID_ARGS)?
        };
        let prefix = HEADER
            .checked_add(interface_size)
            .ok_or(native_abi::ERR_OUT_OF_RANGE)?;
        let capacity = sizes.iter().try_fold(prefix, |size, part| {
            size.checked_add(*part).ok_or(native_abi::ERR_OUT_OF_RANGE)
        })?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(capacity)
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        bytes.resize(prefix, 0);
        bytes[..4].copy_from_slice(&VERSION.to_le_bytes());
        for (slot, size) in bytes[4..HEADER].as_chunks_mut::<8>().0.iter_mut().zip([
            interface_size,
            sizes[0],
            sizes[1],
            sizes[2],
            sizes[3],
        ]) {
            *slot = (size as u64).to_le_bytes();
        }
        if interface_size != 0 {
            InterfaceConfigurationBatch::encode(interfaces, &mut bytes[HEADER..])
                .map_err(|_| native_abi::ERR_INVALID_ARGS)?;
        }
        for body in routes.into_iter().flatten() {
            bytes.extend_from_slice(body);
        }
        if let Some(policy) = policy {
            bytes.extend_from_slice(&policy);
        }
        if let Some(tables) = tables {
            bytes.extend_from_slice(tables);
        }
        Ok(bytes)
    }
    /// 完整验证所有长度与领域正文后借用；不接受旧版本或错族布局。
    /// # Errors
    /// 版本/长度/字段/族非法、截断、包尾或资源不足。
    pub fn parse(bytes: &'a [u8]) -> Result<Self, Status> {
        let mut reader = Reader::new(bytes);
        if reader.u32()? != VERSION {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        let mut sizes = [0usize; 5];
        for size in &mut sizes {
            *size = usize::try_from(reader.u64()?).map_err(|_| native_abi::ERR_OUT_OF_RANGE)?;
        }
        if sizes == [0; 5] {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        let length = sizes.iter().try_fold(HEADER, |sum, size| {
            sum.checked_add(*size).ok_or(native_abi::ERR_INVALID_ARGS)
        })?;
        if length != bytes.len() {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        let mut offset = HEADER + sizes[0];
        let interfaces = if sizes[0] == 0 {
            InterfaceConfigurationBatch::default()
        } else {
            InterfaceConfigurationBatch::parse(&bytes[HEADER..offset])
                .map_err(|_| native_abi::ERR_INVALID_ARGS)?
        };
        let mut routes = [None, None];
        for ((route, length), family) in routes
            .iter_mut()
            .zip(&sizes[1..3])
            .zip([Family::Ipv4, Family::Ipv6])
        {
            if *length != 0 {
                let batch = Batch::parse(&bytes[offset..offset + length])
                    .map_err(|_| native_abi::ERR_INVALID_ARGS)?;
                if batch.family() != family {
                    return Err(native_abi::ERR_INVALID_ARGS);
                }
                *route = Some(batch);
            }
            offset += length;
        }
        let policy = if sizes[3] == 0 {
            None
        } else {
            Some(
                router_protocol::policy::Policy::decode(&bytes[offset..offset + sizes[3]])
                    .map_err(policy_status)?,
            )
        };
        let tables = if sizes[4] == 0 {
            None
        } else {
            Some(
                Tables::parse(&bytes[offset + sizes[3]..])
                    .map_err(|_| native_abi::ERR_INVALID_ARGS)?,
            )
        };
        Ok(Self {
            interfaces,
            routes,
            policy,
            tables,
        })
    }
    /// 借用接口计划，无接口变化时为空。
    pub fn interfaces(&self) -> InterfaceConfigurationBatch<'a> {
        self.interfaces
    }
    /// 按族借用完整静态来源；None 保持，显式空 Batch 撤回。
    pub fn routes(&self, family: Family) -> Option<Batch<'a>> {
        self.routes[match family {
            Family::Ipv4 => 0,
            Family::Ipv6 => 1,
        }]
    }
    /// 移交可选完整策略；缺席保持，缺省策略清除原限制。
    pub fn into_policy(self) -> Option<router_protocol::policy::Policy> {
        self.policy
    }

    /// 同批可选的完整备选表与选表规则；缺席保持。
    pub fn tables(&self) -> Option<Tables<'a>> {
        self.tables
    }
}
fn policy_status(error: router_protocol::policy::WireError) -> Status {
    match error {
        router_protocol::policy::WireError::Invalid => native_abi::ERR_INVALID_ARGS,
        router_protocol::policy::WireError::NoMemory => native_abi::ERR_NO_MEMORY,
    }
}
