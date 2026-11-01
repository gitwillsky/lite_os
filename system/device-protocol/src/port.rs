//! named byte-stream port（virtio-console 的 SPICE 剪贴板通道）。

use alloc::sync::Arc;
use alloc::vec::Vec;

use device_model::{Activity, PortActivity, PortDevice, PortError};
use native_runtime::native_abi::{self, Status, rights};
use native_runtime::{Channel, Event};

use crate::directory::{self, Announcement, Class};
use crate::{ActivityBinding, EventHub, Reader, Writer, activity_signal, call, serve, take_handle};

const READ: u32 = 1;
const WRITE: u32 = 2;
const STATE: u32 = 3;
const DISPATCH: u32 = 4;
/// 单次读写的最大字节数。
const CHUNK: usize = 4096;

const ERROR_WOULD_BLOCK: Status = native_abi::ERR_SHOULD_WAIT;
const ERROR_DISCONNECTED: Status = native_abi::ERR_PEER_CLOSED;

fn encode_error(error: PortError) -> Status {
    match error {
        PortError::WouldBlock => ERROR_WOULD_BLOCK,
        PortError::Disconnected => ERROR_DISCONNECTED,
    }
}

fn decode_error(status: Status) -> PortError {
    if status == ERROR_WOULD_BLOCK {
        PortError::WouldBlock
    } else {
        PortError::Disconnected
    }
}

/// 驱动宿主：发布 port 设备；活动经 Event 通知使用者。
///
/// # Errors
///
/// 对象、线程创建或目录写入失败。
pub fn publish(directory: &Channel, device: Arc<dyn PortDevice>) -> Result<(), Status> {
    let (client, server) = Channel::create()?;
    let event = Event::create()?;
    let peer_event = event.duplicate(rights::SAME)?;
    device.bind_activity(activity_signal(Arc::try_new(event).map_err(|_| native_abi::ERR_NO_MEMORY)?)?);
    serve(
        server,
        1,
        b"port-server",
        Arc::try_new(move |ordinal: u32, request: &mut Reader<'_>, _, reply: &mut Writer| {
            match ordinal {
                READ => {
                    let length = request.usize()?.min(CHUNK);
                    let mut data = alloc::vec![0u8; length];
                    let count = device.read(&mut data).map_err(encode_error)?;
                    reply.bytes(&data[..count]);
                }
                WRITE => {
                    let count = device.write(request.bytes()?).map_err(encode_error)?;
                    reply.u64(count as u64);
                }
                STATE => {
                    reply.bool(device.readable()).bool(device.writable()).bool(device.connected());
                }
                DISPATCH => {
                    let activity = device.dispatch();
                    reply
                        .bool(activity.readable_changed)
                        .bool(activity.writable_changed)
                        .bool(activity.backlog);
                }
                _ => return Err(native_abi::ERR_NOT_SUPPORTED),
            }
            Ok(Vec::new())
        })
        .map_err(|_| native_abi::ERR_NO_MEMORY)?,
    )?;
    directory::announce(
        directory,
        Class::Port,
        &directory::description(),
        alloc::vec![client.into_handle(), peer_event.into_handle()],
    )
}

/// 使用者端的远端 port。
pub struct RemotePort {
    channel: Channel,
    activity: ActivityBinding,
}

/// 连接目录中的 port。
///
/// # Errors
///
/// 句柄缺失或事件登记失败。
pub fn connect(mut announcement: Announcement, hub: &Arc<EventHub>) -> Result<Arc<RemotePort>, Status> {
    let channel = Channel::from_handle(take_handle(&mut announcement.handles, 0)?);
    let activity = hub.activity(Event::from_handle(take_handle(&mut announcement.handles, 1)?))?;
    Arc::try_new(RemotePort { channel, activity }).map_err(|_| native_abi::ERR_NO_MEMORY)
}

impl RemotePort {
    fn state(&self) -> (bool, bool, bool) {
        call(&self.channel, &Writer::request(STATE), Vec::new())
            .and_then(|reply| {
                let mut reader = reply.reader();
                Ok((reader.bool()?, reader.bool()?, reader.bool()?))
            })
            .unwrap_or((false, false, false))
    }
}

impl PortDevice for RemotePort {
    fn read(&self, output: &mut [u8]) -> Result<usize, PortError> {
        let mut request = Writer::request(READ);
        request.u64(output.len().min(CHUNK) as u64);
        let reply = call(&self.channel, &request, Vec::new()).map_err(decode_error)?;
        let data = reply.reader().bytes().map_err(|_| PortError::Disconnected)?;
        let count = data.len().min(output.len());
        output[..count].copy_from_slice(&data[..count]);
        Ok(count)
    }

    fn write(&self, input: &[u8]) -> Result<usize, PortError> {
        let mut request = Writer::request(WRITE);
        request.bytes(&input[..input.len().min(CHUNK)]);
        let reply = call(&self.channel, &request, Vec::new()).map_err(decode_error)?;
        reply.reader().usize().map_err(|_| PortError::Disconnected)
    }

    fn readable(&self) -> bool {
        self.state().0
    }

    fn writable(&self) -> bool {
        self.state().1
    }

    fn connected(&self) -> bool {
        self.state().2
    }

    fn dispatch(&self) -> PortActivity {
        call(&self.channel, &Writer::request(DISPATCH), Vec::new())
            .and_then(|reply| {
                let mut reader = reply.reader();
                Ok(PortActivity {
                    readable_changed: reader.bool()?,
                    writable_changed: reader.bool()?,
                    backlog: reader.bool()?,
                })
            })
            .unwrap_or_default()
    }

    fn bind_activity(&self, activity: Activity) {
        self.activity.bind(activity);
    }
}
