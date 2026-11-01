//! USB 设备连接：身份与名称随目录记录传递，状态与安全移除走请求。

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use device_model::{UsbDevice, UsbIdentity, UsbRemoveError};
use native_runtime::Channel;
use native_runtime::native_abi::{self, Status};

use crate::directory::{self, Announcement, Class};
use crate::{Reader, Writer, call, serve, take_handle};

const CONNECTED: u32 = 1;
const REMOVE: u32 = 2;

const ERROR_DISCONNECTED: Status = native_abi::ERR_PEER_CLOSED;

/// 驱动宿主：发布一个 USB 设备连接（热插拔时在枚举完成之后同样调用）。
///
/// # Errors
///
/// 对象、线程创建或目录写入失败。
pub fn publish(directory: &Channel, device: Arc<dyn UsbDevice>) -> Result<(), Status> {
    let (client, server) = Channel::create()?;
    let identity = device.identity();
    let mut description = directory::description();
    description
        .u64(identity.bus as u64)
        .u64(identity.port as u64)
        .u16(identity.vendor)
        .u16(identity.product)
        .bytes(device.name().as_bytes());
    serve(
        server,
        1,
        b"usb-server",
        Arc::try_new(
            move |ordinal: u32, _: &mut Reader<'_>, _, reply: &mut Writer| {
                match ordinal {
                    CONNECTED => {
                        reply.bool(device.connected());
                    }
                    REMOVE => device.remove().map_err(|error| match error {
                        UsbRemoveError::Disconnected => ERROR_DISCONNECTED,
                        UsbRemoveError::Device => native_abi::ERR_IO,
                    })?,
                    _ => return Err(native_abi::ERR_NOT_SUPPORTED),
                }
                Ok(Vec::new())
            },
        )
        .map_err(|_| native_abi::ERR_NO_MEMORY)?,
    )?;
    directory::announce(
        directory,
        Class::Usb,
        &description,
        alloc::vec![client.into_handle()],
    )
}

/// 使用者端的远端 USB 设备。
pub struct RemoteUsb {
    channel: Channel,
    identity: UsbIdentity,
    name: String,
}

/// 连接目录中的 USB 设备。
///
/// # Errors
///
/// 描述或句柄不完整。
pub fn connect(mut announcement: Announcement) -> Result<Arc<RemoteUsb>, Status> {
    let channel = Channel::from_handle(take_handle(&mut announcement.handles, 0)?);
    let mut reader = announcement.reader();
    let identity = UsbIdentity {
        bus: reader.usize()?,
        port: reader.usize()?,
        vendor: reader.u16()?,
        product: reader.u16()?,
    };
    let name =
        String::from_utf8(reader.bytes()?.to_vec()).map_err(|_| native_abi::ERR_INVALID_ARGS)?;
    Arc::try_new(RemoteUsb {
        channel,
        identity,
        name,
    })
    .map_err(|_| native_abi::ERR_NO_MEMORY)
}

impl UsbDevice for RemoteUsb {
    fn identity(&self) -> UsbIdentity {
        self.identity
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn connected(&self) -> bool {
        call(&self.channel, &Writer::request(CONNECTED), Vec::new())
            .and_then(|reply| reply.reader().bool())
            .unwrap_or(false)
    }

    fn remove(&self) -> Result<(), UsbRemoveError> {
        call(&self.channel, &Writer::request(REMOVE), Vec::new()).map_err(|status| {
            if status == ERROR_DISCONNECTED {
                UsbRemoveError::Disconnected
            } else {
                UsbRemoveError::Device
            }
        })?;
        Ok(())
    }
}
