//! entropy 设备：按请求长度返回设备 entropy。

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::mem::MaybeUninit;

use device_model::{EntropyError, EntropySource};
use native_runtime::Channel;
use native_runtime::native_abi::{self, Status};

use crate::directory::{self, Class, Directory, OpenedDevice, Session, SessionState};
use crate::{Reader, Writer, call, take_handle};

const FILL: u32 = 1;
/// 单次请求的最大字节数；更长的请求由 client 分段。
const CHUNK: usize = 4096;

/// 驱动宿主：登记 entropy 设备（可共享，会话无状态）。
///
/// # Errors
///
/// 分配失败。
pub fn publish(directory: &Directory, device: Arc<dyn EntropySource>) -> Result<(), Status> {
    directory
        .add(
            Class::Entropy,
            &directory::description(),
            false,
            Box::new(move |end| {
                let session = SessionState::new(device.clone(), end);
                let (client, server) = Channel::create()?;
                let server = directory::serve_session(
                    server,
                    1,
                    b"entropy-server",
                    Arc::try_new(
                        move |ordinal: u32, request: &mut Reader<'_>, _, reply: &mut Writer| {
                            if ordinal != FILL {
                                return Err(native_abi::ERR_NOT_SUPPORTED);
                            }
                            let length = request.usize()?.min(CHUNK);
                            let mut bytes = alloc::vec![MaybeUninit::<u8>::uninit(); length];
                            session
                                .get()
                                .fill(&mut bytes)
                                .map_err(|EntropyError| native_abi::ERR_IO)?;
                            // SAFETY: `fill` 成功即完整初始化了全部字节。
                            let bytes = unsafe {
                                &*(core::ptr::from_ref::<[MaybeUninit<u8>]>(bytes.as_slice())
                                    as *const [u8])
                            };
                            reply.bytes(bytes);
                            Ok(Vec::new())
                        },
                    )
                    .map_err(|_| native_abi::ERR_NO_MEMORY)?,
                )?;
                Ok(Session {
                    handles: alloc::vec![client.into_handle()],
                    server,
                })
            }),
        )
        .map(drop)
}

/// 使用者端的远端 entropy 设备。
pub struct RemoteEntropy {
    channel: Channel,
}

/// 连接目录中的 entropy 设备。
///
/// # Errors
///
/// 句柄缺失。
pub fn connect(mut announcement: OpenedDevice) -> Result<Arc<RemoteEntropy>, Status> {
    let channel = Channel::from_handle(take_handle(&mut announcement.handles, 0)?);
    Arc::try_new(RemoteEntropy { channel }).map_err(|_| native_abi::ERR_NO_MEMORY)
}

impl EntropySource for RemoteEntropy {
    fn fill(&self, bytes: &mut [MaybeUninit<u8>]) -> Result<(), EntropyError> {
        for chunk in bytes.chunks_mut(CHUNK) {
            let mut request = Writer::request(FILL);
            request.u64(chunk.len() as u64);
            let reply = call(&self.channel, &request, Vec::new()).map_err(|_| EntropyError)?;
            let data = reply.reader().bytes().map_err(|_| EntropyError)?;
            if data.len() != chunk.len() {
                return Err(EntropyError);
            }
            for (slot, byte) in chunk.iter_mut().zip(data) {
                slot.write(*byte);
            }
        }
        Ok(())
    }
}
