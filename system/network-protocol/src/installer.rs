//! 网卡安装协议（`service_directory::NETWORK_INSTALLER`，只路由给接口配置 owner netcfg）。

use alloc::vec::Vec;

use native_runtime::native_abi::{self, Status};
use native_runtime::{Channel, Handle};
use service_rpc::{Reader, Writer, call};

const INSTALL_DEVICE: u32 = 1;

/// 服务端：网卡安装（只路由给接口配置 owner netcfg）。
pub trait InstallerService: Send + Sync {
    /// 把设备目录中打开的一块网卡（目录条目描述与会话句柄）安装为默认网络的新接口，返回接口序号。
    ///
    /// # Errors
    ///
    /// 描述或句柄不完整、连接失败或资源不足。
    fn install_device(&self, description: Vec<u8>, handles: Vec<Handle>) -> Result<u32, Status>;
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
            let description = request.bytes()?;
            let mut copy = Vec::new();
            copy.try_reserve_exact(description.len())
                .map_err(|_| native_abi::ERR_NO_MEMORY)?;
            copy.extend_from_slice(description);
            reply.u32(service.install_device(copy, handles)?);
            Ok(Vec::new())
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

    /// 把一块网卡安装进默认网络，返回接口序号。
    ///
    /// # Errors
    ///
    /// 服务拒绝该设备或已终止。
    pub fn install_device(&self, description: &[u8], handles: Vec<Handle>) -> Result<u32, Status> {
        let mut writer = Writer::request(INSTALL_DEVICE);
        writer.bytes(description);
        call(&self.channel, &writer, handles)?.reader().u32()
    }
}
