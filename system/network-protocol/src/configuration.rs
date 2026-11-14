//! 仅接口配置 owner 得到的执行事务能力；沿现有安装授权派生，不向 Linux 域 channel 开放。

use alloc::vec::Vec;
use native_runtime::{
    Channel, Handle, Vmo,
    native_abi::{self, Status},
};
use service_rpc::{Reader, Writer};

use router_protocol::firewall::Prerouting;
use router_protocol::switching::Ports;

use super::SocketError;

mod batch;
pub use batch::ConfigurationBatch;

/// 一次配置事务中的双栈转发策略；None 保持整组，Some 原子替换两族，不复制接口/FIB。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Forwarding {
    /// 是否启用 IPv4 单播和已配置的组播转发。
    pub ipv4: bool,
    /// 是否启用 IPv6 单播和已配置的组播转发。
    pub ipv6: bool,
}

/// 同一配置事务中的执行策略。缺席的字段保持运行值。
#[derive(Clone, Copy, Default)]
pub struct ExecutionPolicy<'a> {
    /// 双栈转发开关。
    pub forwarding: Option<Forwarding>,
    /// 预路由规则；显式空链清除原规则。
    pub prerouting: Option<Prerouting<'a>>,
    /// 物理二层成员；显式空集合清除网桥。
    pub bridges: Option<Ports<'a>>,
    /// 完整组播转发表正文；显式空表撤回全部组播路径。
    pub multicast: Option<&'a [u8]>,
}

const PREPARE_VERSION: u32 = 6;
/// 冻结预备请求；正文由 RIB 意图或 NET 执行 codec 拥有，此处只拥有通用事务传输。
/// 消费编码所有权并在返回前释放该副本，避免调用者等待 RPC 时仍保留全表。
/// 只为非空正文创建 VMO，单改执行策略无需复制路由表。
/// # Errors
/// 无实际变更、资源不足或 VMO 创建失败。
pub(crate) fn prepare_request(
    encoded: Vec<u8>,
    policy: ExecutionPolicy<'_>,
) -> Result<(Writer, Vec<Handle>), Status> {
    if policy.multicast.is_some_and(<[u8]>::is_empty) {
        return Err(native_abi::ERR_INVALID_ARGS);
    }
    if encoded.is_empty()
        && policy.forwarding.is_none()
        && policy.prerouting.is_none()
        && policy.bridges.is_none()
        && policy.multicast.is_none()
    {
        return Err(native_abi::ERR_INVALID_ARGS);
    }
    let mut request = Writer::request(crate::installer::PREPARE_CONFIGURATION);
    request.try_reserve(40)?;
    request
        .u32(PREPARE_VERSION)
        .u32(match policy.forwarding {
            None => 0,
            Some(policy) => 4 | u32::from(policy.ipv4) | (u32::from(policy.ipv6) << 1),
        })
        .u64(encoded.len() as u64)
        .u64(
            policy
                .prerouting
                .map_or(0, |chain| chain.bytes().len() as u64),
        )
        .u64(policy.bridges.map_or(0, |ports| ports.bytes().len() as u64))
        .u64(policy.multicast.map_or(0, |bytes| bytes.len() as u64));
    let mut handles = Vec::new();
    handles
        .try_reserve_exact(
            usize::from(!encoded.is_empty())
                + usize::from(policy.prerouting.is_some())
                + usize::from(policy.bridges.is_some())
                + usize::from(policy.multicast.is_some()),
        )
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    if !encoded.is_empty() {
        handles.push(Vmo::from_bytes(&encoded)?.into_handle());
    }
    if let Some(chain) = policy.prerouting {
        handles.push(Vmo::from_bytes(chain.bytes())?.into_handle());
    }
    if let Some(ports) = policy.bridges {
        handles.push(Vmo::from_bytes(ports.bytes())?.into_handle());
    }
    if let Some(bytes) = policy.multicast {
        handles.push(Vmo::from_bytes(bytes)?.into_handle());
    }
    Ok((request, handles))
}
pub(crate) struct CapturedPrepare {
    pub body: Option<Vec<u8>>,
    forwarding: Option<Forwarding>,
    prerouting: Option<Vec<u8>>,
    bridges: Option<Vec<u8>>,
    multicast: Option<Vec<u8>>,
}

impl CapturedPrepare {
    pub(crate) fn policy(&self) -> Result<ExecutionPolicy<'_>, Status> {
        Ok(ExecutionPolicy {
            forwarding: self.forwarding,
            prerouting: self
                .prerouting
                .as_deref()
                .map(Prerouting::parse)
                .transpose()
                .map_err(|_| native_abi::ERR_INVALID_ARGS)?,
            bridges: self
                .bridges
                .as_deref()
                .map(Ports::parse)
                .transpose()
                .map_err(|_| native_abi::ERR_INVALID_ARGS)?,
            multicast: self.multicast.as_deref(),
        })
    }
}

/// 完整捕获预备控制帧及可选正文；业务 owner 在产生副作用前再解码唯一领域格式。
/// # Errors
/// 版本、可选值、尾部、空操作、句柄或 VMO 非法；资源不足。
pub(crate) fn capture_prepare(
    request: &mut Reader<'_>,
    mut handles: Vec<Handle>,
) -> Result<CapturedPrepare, Status> {
    if request.u32()? != PREPARE_VERSION {
        return Err(native_abi::ERR_INVALID_ARGS);
    }
    let forwarding = match request.u32()? {
        0 => None,
        bits @ 4..=7 => Some(Forwarding {
            ipv4: bits & 1 != 0,
            ipv6: bits & 2 != 0,
        }),
        _ => return Err(native_abi::ERR_INVALID_ARGS),
    };
    let length = request.u64()?;
    let firewall_length = request.u64()?;
    let bridge_length = request.u64()?;
    let multicast_length = request.u64()?;
    request.finish()?;
    let previous_handles = usize::from(length != 0) + usize::from(firewall_length != 0);
    let multicast_index = previous_handles + usize::from(bridge_length != 0);
    if handles.len() != multicast_index + usize::from(multicast_length != 0)
        || multicast_index == 0 && multicast_length == 0 && forwarding.is_none()
    {
        return Err(native_abi::ERR_INVALID_ARGS);
    }
    let mut capture = |length, index| {
        if length == 0 {
            return Ok(None);
        }
        Vmo::from_handle(service_rpc::take_handle(&mut handles, index)?)
            .read_bytes(length)
            .map(Some)
    };
    Ok(CapturedPrepare {
        body: capture(length, 0)?,
        forwarding,
        prerouting: capture(firewall_length, usize::from(length != 0))?,
        bridges: capture(bridge_length, previous_handles)?,
        multicast: capture(multicast_length, multicast_index)?,
    })
}

const ACTIVATE: u32 = 1;
const CONFIRM: u32 = 2;
const ABORT: u32 = 3;

/// 执行服务的在途配置：未确认前必须持有可无分配恢复的旧资源。
pub trait ConfigurationService: Send + Sync {
    /// 原子发布预备好的批次。失败不允许半次发布。
    /// # Errors
    /// 事务无效或设备已消失。
    fn activate(&self) -> Result<(), SocketError>;
    /// 确认发布并释放旧资源与配置写租约。
    /// # Errors
    /// 未发布或事务无效；已发布后的设备寿命不改变确认决定。
    fn confirm(&self) -> Result<(), SocketError>;
    /// 撤销发布并释放写租约；其他存活资源必须先恢复，再报告无法恢复的设备。
    /// # Errors
    /// 事务无效或设备已消失。
    fn abort(&self) -> Result<(), SocketError>;
}

/// 分派事务请求；拒绝附带数据和句柄，错误通过同一网络领域结果编码返回。
///
/// # Errors
/// 请求畸形或未知操作。
pub fn dispatch_configuration(
    service: &dyn ConfigurationService,
    ordinal: u32,
    request: &mut Reader<'_>,
    handles: Vec<Handle>,
    reply: &mut Writer,
) -> Result<Vec<Handle>, Status> {
    request.finish()?;
    if !handles.is_empty() {
        return Err(native_abi::ERR_INVALID_ARGS);
    }
    // 成功/领域错误均编码为一个字节；在激活/确认副作用前准备，不能发布后再因回复扩容失败退出。
    reply.try_reserve(1)?;
    let result = match ordinal {
        ACTIVATE => service.activate(),
        CONFIRM => service.confirm(),
        ABORT => service.abort(),
        _ => return Err(native_abi::ERR_NOT_SUPPORTED),
    };
    super::put_result(reply, result);
    Ok(Vec::new())
}

/// 客户端持有的事务 channel；关闭自动撤销未确认配置，不需 Drop 时追加 RPC。
pub struct PreparedConfiguration(Channel);

impl PreparedConfiguration {
    pub(super) fn new(channel: Channel) -> Self {
        Self(channel)
    }
    /// 移交同一执行事务的寿命能力；不复制、不扩大授权，关闭仍撤销未确认配置。
    pub fn into_channel(self) -> Channel {
        self.0
    }

    fn command(&self, ordinal: u32) -> Result<(), SocketError> {
        let reply = service_rpc::call(&self.0, &Writer::request(ordinal), Vec::new())
            .map_err(super::client::transport)?;
        if !reply.handles.is_empty() {
            return Err(SocketError::Invalid);
        }
        let mut reader = reply.reader();
        let result = super::get_result(&mut reader).map_err(super::client::transport)?;
        reader.finish().map_err(|_| SocketError::Invalid)?;
        result
    }

    /// 原子激活批次；重复激活同一事务无副作用，确认前可回滚。
    /// # Errors
    /// 设备消失、事务无效或执行服务不可用。
    pub fn activate(&self) -> Result<(), SocketError> {
        self.command(ACTIVATE)
    }

    /// 确认当前配置并消费能力，执行服务释放原节点。
    /// # Errors
    /// 未激活、事务无效或服务不可用；传输失败不证明服务尚未执行确认。
    pub fn confirm(self) -> Result<(), SocketError> {
        self.command(CONFIRM)
    }

    /// 同步撤销并消费能力；可用来等待回滚完成，而非仅等待 channel 关闭通知。
    /// # Errors
    /// 设备已消失、事务无效或服务不可用。
    pub fn abort(self) -> Result<(), SocketError> {
        self.command(ABORT)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn prepare_control_requires_canonical_intent_and_exact_body_capabilities() {
        for (ipv4, ipv6) in [(false, false), (false, true), (true, false), (true, true)] {
            let enabled = Forwarding { ipv4, ipv6 };
            let (request, handles) = prepare_request(
                Vec::new(),
                ExecutionPolicy {
                    forwarding: Some(enabled),
                    ..Default::default()
                },
            )
            .unwrap();
            assert!(handles.is_empty());
            let bytes = &request.as_bytes()[8..];
            let captured = capture_prepare(&mut Reader::new(bytes), handles).unwrap();
            assert!(captured.body.is_none());
            assert_eq!(captured.policy().unwrap().forwarding, Some(enabled));
            for length in 0..bytes.len() {
                assert!(capture_prepare(&mut Reader::new(&bytes[..length]), Vec::new()).is_err());
            }
            let mut extra = bytes.to_vec();
            extra.push(0);
            assert!(capture_prepare(&mut Reader::new(&extra), Vec::new()).is_err());
            let mut invalid = bytes.to_vec();
            invalid[..4].copy_from_slice(&999u32.to_le_bytes());
            assert!(capture_prepare(&mut Reader::new(&invalid), Vec::new()).is_err());
            invalid[..4].copy_from_slice(&PREPARE_VERSION.to_le_bytes());
            invalid[4..8].copy_from_slice(&3u32.to_le_bytes());
            assert!(capture_prepare(&mut Reader::new(&invalid), Vec::new()).is_err());
            invalid[4..8].copy_from_slice(&0u32.to_le_bytes());
            assert!(capture_prepare(&mut Reader::new(&invalid), Vec::new()).is_err());
            invalid[4..8].copy_from_slice(&4u32.to_le_bytes());
            for offset in (8..40).step_by(8) {
                invalid[offset..offset + 8].copy_from_slice(&1u64.to_le_bytes());
                assert!(capture_prepare(&mut Reader::new(&invalid), Vec::new()).is_err());
                invalid[offset..offset + 8].fill(0);
            }
        }
        assert!(prepare_request(Vec::new(), ExecutionPolicy::default()).is_err());
        assert!(
            prepare_request(
                Vec::new(),
                ExecutionPolicy {
                    multicast: Some(&[]),
                    ..Default::default()
                }
            )
            .is_err()
        );
    }
}
