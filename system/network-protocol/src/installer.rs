//! 网卡安装协议（`service_directory::NETWORK_INSTALLER`，只路由给接口配置 owner netcfg）。

use alloc::vec::Vec;

use native_runtime::native_abi::{self, Status};
use native_runtime::{Channel, Handle};
use service_rpc::{Reader, Writer, call};

const INSTALL_DEVICE: u32 = 1;

/// 服务端：网卡安装（只路由给接口配置 owner netcfg）。
pub trait InstallerService: Send + Sync {
    /// 把授权目录中的 device_id 安装为新接口；netstack 按 worker 配置打开会话，handles 只携带目录能力。
    ///
    /// # Returns
    ///
    /// 接口序号，以及接口的 lifetime channel：接口移除（网卡会话关闭）时服务端关闭对端，
    /// 使用者在其上观察到 `PEER_CLOSED`（同 Fuchsia `interfaces.admin.Control` 的移除终止）。
    ///
    /// # Errors
    ///
    /// 描述或句柄不完整、连接失败或资源不足。
    fn install_device(
        &self,
        device_id: u64,
        description: Vec<u8>,
        handles: Vec<Handle>,
    ) -> Result<(u32, Channel), Status>;
}

/// 分派安装 channel 的一条请求；签名与 `service_rpc::Handler` 一致。
///
/// # Errors
///
/// 未知请求、格式错误或服务的失败。
pub fn dispatch_installer(
    service: &dyn InstallerService,
    ordinal: u32,
    request: &mut Reader<'_>,
    handles: Vec<Handle>,
    reply: &mut Writer,
) -> Result<Vec<Handle>, Status> {
    match ordinal {
        INSTALL_DEVICE => {
            let device_id = request.u64()?;
            let description = request.bytes()?;
            let mut copy = Vec::new();
            copy.try_reserve_exact(description.len())
                .map_err(|_| native_abi::ERR_NO_MEMORY)?;
            copy.extend_from_slice(description);
            let (index, lifetime) = service.install_device(device_id, copy, handles)?;
            reply.u32(index);
            let mut returned = Vec::new();
            returned
                .try_reserve_exact(1)
                .map_err(|_| native_abi::ERR_NO_MEMORY)?;
            returned.push(lifetime.into_handle());
            Ok(returned)
        }
        _ => Err(native_abi::ERR_NOT_SUPPORTED),
    }
}

/// 使用者端：网卡安装。
pub struct NetworkInstaller {
    channel: Channel,
}

impl NetworkInstaller {
    pub fn new(channel: Channel) -> Self {
        Self { channel }
    }

    /// 连接的 channel：netstack 终止时置位 `PEER_CLOSED`，使用者据此重新安装。
    pub fn channel(&self) -> &Channel {
        &self.channel
    }

    /// 把一块网卡安装进默认网络，返回接口序号与接口的 lifetime channel（接口移除时 `PEER_CLOSED`）。
    ///
    /// # Errors
    ///
    /// 服务拒绝该设备或已终止。
    pub fn install_device(
        &self,
        device_id: u64,
        description: &[u8],
        handles: Vec<Handle>,
    ) -> Result<(u32, Channel), Status> {
        let mut writer = Writer::request(INSTALL_DEVICE);
        writer.u64(device_id).bytes(description);
        let mut reply = call(&self.channel, &writer, handles)?;
        let (mut reader, handles) = reply.split();
        let index = reader.u32()?;
        let lifetime = Channel::from_handle(service_rpc::take_handle(handles, 0)?);
        Ok((index, lifetime))
    }
}
