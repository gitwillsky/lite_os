//! 按设备目录授权装配一块物理网卡。

use alloc::vec::Vec;
use native_runtime::native_abi::{self, Status};
use native_runtime::{Channel, Event, Handle};
use router_protocol::configuration::Name;
use service_rpc::{Reader, Writer, call, take_handle};

use super::installer::routing_domain_name;

const INSTALL_DEVICE: u32 = 1;
const INSTALL_DEVICE_VERSION: u32 = 3;

/// 在单独授权的设备能力端点解码装配请求。回调接收目录能力、设备身份、描述和目标域。
///
/// 成功返回接口序号、寿命通道和链路事件。未知操作、协议格式错误或装配失败返回状态码。
pub fn dispatch_install_device(
    ordinal: u32,
    request: &mut Reader<'_>,
    handles: Vec<Handle>,
    reply: &mut Writer,
    install: impl FnOnce(u64, Option<Name>, Vec<u8>, Channel) -> Result<(u32, Channel, Event), Status>,
) -> Result<Vec<Handle>, Status> {
    if ordinal != INSTALL_DEVICE {
        return Err(native_abi::ERR_NOT_SUPPORTED);
    }
    if request.u32()? != INSTALL_DEVICE_VERSION || handles.len() != 1 {
        return Err(native_abi::ERR_INVALID_ARGS);
    }
    let device_id = request.u64()?;
    let name = request.bytes()?;
    let domain = (!name.is_empty())
        .then(|| routing_domain_name(name))
        .transpose()?;
    let description = request.bytes()?;
    request.finish()?;
    // 安装之后不能再因回复扩容失败而丢失刚创建的接口身份/寿命能力。
    reply.try_reserve(4)?;
    let mut returned = Vec::new();
    returned
        .try_reserve_exact(2)
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    let mut copy = Vec::new();
    copy.try_reserve_exact(description.len())
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    copy.extend_from_slice(description);
    let mut handles = handles;
    let directory = Channel::from_handle(take_handle(&mut handles, 0)?);
    let (index, lifetime, link_changed) = install(device_id, domain, copy, directory)?;
    reply.u32(index);
    returned.push(lifetime.into_handle());
    returned.push(link_changed.into_handle());
    Ok(returned)
}

/// 单独授权的网卡装配客户端。
pub struct DeviceInstaller(Channel);

impl DeviceInstaller {
    /// 使用配置服务从目录获得的设备装配端点建立客户端。
    pub fn new(channel: Channel) -> Self {
        Self(channel)
    }

    /// 把网卡装入 `domain`，缺席表示默认域。`directory` 是授权的设备目录。
    /// 成功返回接口序号、寿命通道与链路事件。接口移除时寿命通道关闭，链路变化使事件置 USER_0。
    ///
    /// # Errors
    ///
    /// 服务拒绝该设备或已终止。
    pub fn install_device(
        &self,
        device_id: u64,
        domain: Option<&Name>,
        description: &[u8],
        directory: Channel,
    ) -> Result<(u32, Channel, Event), Status> {
        let name = domain.map_or(&[][..], |name| name.as_str().as_bytes());
        let mut writer = Writer::request(INSTALL_DEVICE);
        writer.try_reserve(
            20usize
                .checked_add(description.len())
                .and_then(|size| size.checked_add(name.len()))
                .ok_or(native_abi::ERR_OUT_OF_RANGE)?,
        )?;
        writer
            .u32(INSTALL_DEVICE_VERSION)
            .u64(device_id)
            .bytes(name)
            .bytes(description);
        let mut handles = Vec::new();
        handles
            .try_reserve_exact(1)
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        handles.push(directory.into_handle());
        let mut reply = call(&self.0, &writer, handles)?;
        if reply.handles.len() != 2 {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        let (mut reader, handles) = reply.split();
        let index = reader.u32()?;
        reader.finish()?;
        let link_changed = Event::from_handle(take_handle(handles, 1)?);
        let lifetime = Channel::from_handle(take_handle(handles, 0)?);
        Ok((index, lifetime, link_changed))
    }
}
