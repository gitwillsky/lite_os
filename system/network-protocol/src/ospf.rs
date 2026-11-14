//! 配置 owner 和独立 OSPF 实例之间的授权配置与观察接口。

use crate::routing::RoutingSource;
use alloc::vec::Vec;
use native_runtime::{
    Channel, Handle, Vmo,
    native_abi::{self, Status},
};
use router_protocol::ospf::{Session, Snapshot, VERSION};
use service_rpc::{Reader, Writer};
use zeroize::Zeroizing;

/// 创建带有指定链路绑定的实例。
pub const OPEN: u32 = 1;
/// 查询同一实例当前的邻居观察。
pub const SNAPSHOT: u32 = 1;

/// 生命周期由配置 owner 持有。关闭能力停止协议实例及网络端点。
pub struct OspfInstance(Channel);

impl OspfInstance {
    /// 通过工厂以同域的来源与网络能力启动已投影配置，并确认实例可以响应状态请求。
    /// # Errors
    /// 配置、资源、网络初始化或工厂回复无效。
    pub fn start(
        factory: &Channel,
        session: &Session,
        source: (RoutingSource, crate::RemoteNamespace),
    ) -> Result<Self, Status> {
        let bytes = Zeroizing::new(session.encode().map_err(|_| native_abi::ERR_INVALID_ARGS)?);
        let mut request = Writer::request(OPEN);
        request.try_reserve(12)?;
        request.u32(VERSION).u64(bytes.len() as u64);
        let mut handles = Vec::new();
        handles
            .try_reserve_exact(3)
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        handles.push(Vmo::from_bytes(&bytes)?.into_handle());
        handles.push(source.0.into_channel().into_handle());
        handles.push(source.1.into_channel().into_handle());
        let mut reply = service_rpc::call(factory, &request, handles)?;
        reply.reader().finish()?;
        if reply.handles.len() != 1 {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        let instance = Self(Channel::from_handle(service_rpc::take_handle(
            &mut reply.handles,
            0,
        )?));
        instance.snapshot()?;
        Ok(instance)
    }

    /// 返回观察实际实例退出所用的能力。
    pub fn channel(&self) -> &Channel {
        &self.0
    }

    /// 获取协议线程的邻居状态。
    /// # Errors
    /// 服务退出、资源不足或回复畸形。
    pub fn snapshot(&self) -> Result<Snapshot, Status> {
        let mut request = Writer::request(SNAPSHOT);
        request.u32(VERSION);
        let mut reply = service_rpc::call(&self.0, &request, Vec::new())?;
        let mut reader = reply.reader();
        let length = reader.u64()?;
        reader.finish()?;
        if reply.handles.len() != 1 {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        let bytes = Vmo::from_handle(service_rpc::take_handle(&mut reply.handles, 0)?)
            .read_bytes(length)?;
        Snapshot::parse(&bytes).map_err(|_| native_abi::ERR_INVALID_ARGS)
    }
}

/// 读取工厂请求，验证版本、句柄和配置后才创建实例。
/// # Errors
/// 版本、正文、句柄或配置无效。
pub fn configuration(
    reader: &mut Reader<'_>,
    mut handles: Vec<Handle>,
) -> Result<(Session, RoutingSource, crate::RemoteNamespace), Status> {
    if reader.u32()? != VERSION {
        return Err(native_abi::ERR_INVALID_ARGS);
    }
    let length = reader.u64()?;
    reader.finish()?;
    if handles.len() != 3 {
        return Err(native_abi::ERR_INVALID_ARGS);
    }
    let source = Channel::from_handle(service_rpc::take_handle(&mut handles, 1)?);
    let bytes = Zeroizing::new(
        Vmo::from_handle(service_rpc::take_handle(&mut handles, 0)?).read_bytes(length)?,
    );
    let session = Session::parse(&bytes).map_err(|_| native_abi::ERR_INVALID_ARGS)?;
    let network = crate::RemoteNamespace::from_channel(Channel::from_handle(
        service_rpc::take_handle(&mut handles, 2)?,
    ));
    Ok((session, RoutingSource::new(source), network))
}
