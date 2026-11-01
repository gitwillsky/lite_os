//! entropy 设备：按请求长度返回设备 entropy。

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::mem::MaybeUninit;

use device_model::{EntropyError, EntropySource};
use native_runtime::native_abi::{self, Status};
use native_runtime::Channel;

use crate::directory::{self, Announcement, Class};
use crate::{Reader, Writer, call, serve, take_handle};

const FILL: u32 = 1;
/// 单次请求的最大字节数；更长的请求由 client 分段。
const CHUNK: usize = 4096;

/// 驱动宿主：发布 entropy 设备。
///
/// # Errors
///
/// channel、线程创建或目录写入失败。
pub fn publish(directory: &Channel, device: Arc<dyn EntropySource>) -> Result<(), Status> {
    let (client, server) = Channel::create()?;
    serve(
        server,
        1,
        b"entropy-server",
        Arc::try_new(move |ordinal: u32, request: &mut Reader<'_>, _, reply: &mut Writer| {
            if ordinal != FILL {
                return Err(native_abi::ERR_NOT_SUPPORTED);
            }
            let length = request.usize()?.min(CHUNK);
            let mut bytes = alloc::vec![MaybeUninit::<u8>::uninit(); length];
            device.fill(&mut bytes).map_err(|EntropyError| native_abi::ERR_IO)?;
            // SAFETY: `fill` 成功即完整初始化了全部字节。
            let bytes = unsafe { &*(core::ptr::from_ref::<[MaybeUninit<u8>]>(bytes.as_slice()) as *const [u8]) };
            reply.bytes(bytes);
            Ok(Vec::new())
        })
        .map_err(|_| native_abi::ERR_NO_MEMORY)?,
    )?;
    directory::announce(directory, Class::Entropy, &directory::description(), alloc::vec![client.into_handle()])
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
pub fn connect(mut announcement: Announcement) -> Result<Arc<RemoteEntropy>, Status> {
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
