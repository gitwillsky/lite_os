#![no_std]
#![feature(allocator_ext)]

//! 文件系统服务的挂载协议。
//!
//! 1. 使用者经服务的控制 channel 发出挂载请求，交出块设备连接（`device_protocol::block` 的一条 channel）；
//! 2. 服务打开文件系统，回复该挂载的 FUSE channel（使用者的 FUSE 客户端经它访问文件）与挂载控制 channel；
//! 3. 挂载控制目前只有“转为可写”（只读挂载重新挂载为读写）；卸载经 FUSE `DESTROY` 完成。

extern crate alloc;

use alloc::sync::Arc;
use alloc::vec::Vec;

use native_runtime::native_abi::{self, Status};
use native_runtime::{Channel, Handle, Message, Vmo};
use service_rpc::{Reader, Writer, call, serve, take_handle};

/// 一条 FUSE READ/WRITE 的最大数据量：请求与回复都必须装进一条 channel 消息（64 KiB），留出
/// 头部空间。服务端在 INIT 中以它为 `max_write`，使用者以它为 `max_read`；任一端更大都会让
/// 消息写入以 `ERR_OUT_OF_RANGE` 失败。
pub const MAX_TRANSFER: u32 = 12 * 4096;

/// 一条 FUSE 消息的上限：最大的是携带 64 KiB 值的 SETXATTR 请求与 GETXATTR 回复（Linux
/// `XATTR_SIZE_MAX`）。超过单条 channel 消息的 FUSE 消息经 VMO 传递，见 [`send_fuse`]。
pub const FUSE_MESSAGE_MAX: usize = 128 * 1024;

/// 在 FUSE channel 上发送一条 FUSE 消息：不超过单条 channel 消息时原样发送字节；更大的消息写入 VMO，
/// channel 消息只携带该 VMO（不含字节），消息长度由 FUSE 头部的 `len` 给出。
///
/// # Errors
///
/// VMO 创建或写入失败，或 channel 写入失败（对端已关闭）。
pub fn send_fuse(channel: &Channel, bytes: &[u8]) -> Result<(), Status> {
    if bytes.len() <= native_abi::CHANNEL_MAX_BYTES {
        return channel.write(bytes, Vec::new());
    }
    let vmo = Vmo::create(bytes.len() as u64, 0)?;
    vmo.write(bytes, 0)?;
    channel.write(&[], alloc::vec![vmo.into_handle()])
}

/// 取出 [`send_fuse`] 发送的 FUSE 消息字节。
///
/// # Errors
///
/// 帧格式错误（VMO 消息带字节、长度越界）或 VMO 读取失败。
pub fn receive_fuse(mut message: Message) -> Result<Vec<u8>, Status> {
    if message.handles.is_empty() {
        return Ok(message.bytes);
    }
    if !message.bytes.is_empty() || message.handles.len() != 1 {
        return Err(native_abi::ERR_INVALID_ARGS);
    }
    let vmo = Vmo::from_handle(take_handle(&mut message.handles, 0)?);
    let mut header = [0u8; 4];
    vmo.read(&mut header, 0)?;
    // FUSE 头部按主机字节序编码（Linux `fuse_in_header`/`fuse_out_header`）。
    let length = u32::from_ne_bytes(header) as usize;
    if !(native_abi::CHANNEL_MAX_BYTES + 1..=FUSE_MESSAGE_MAX).contains(&length) {
        return Err(native_abi::ERR_OUT_OF_RANGE);
    }
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(length)
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    bytes.resize(length, 0);
    vmo.read(&mut bytes, 0)?;
    Ok(bytes)
}

const MOUNT: u32 = 1;
const MAKE_WRITABLE: u32 = 1;

/// 服务端的一次挂载结果。
pub struct Mounted {
    /// 服务端持有的 FUSE channel 对端已在服务中；这里是交给使用者的一端。
    pub fuse: Channel,
    /// 挂载控制。
    pub control: Arc<dyn MountControl>,
}

/// 服务端：打开块设备上的文件系统。
pub trait MountService: Send + Sync {
    /// # Errors
    ///
    /// 块设备不可用、文件系统格式不受支持或资源不足。
    fn mount(&self, block: Channel, read_only: bool) -> Result<Mounted, Status>;
}

/// 服务端：一个已挂载文件系统的控制。
pub trait MountControl: Send + Sync {
    /// 只读挂载转为可写。
    ///
    /// # Errors
    ///
    /// 日志恢复或写入准备失败。
    fn make_writable(&self) -> Result<(), Status>;
}

/// 服务端：以 `service` 服务控制 channel；每个挂载的控制 channel 由一个线程服务。
///
/// # Errors
///
/// 线程创建失败。
pub fn serve_mounts(control: Channel, service: Arc<dyn MountService>) -> Result<(), Status> {
    serve(
        control,
        1,
        b"mount-control",
        Arc::try_new(move |ordinal: u32, request: &mut Reader<'_>, mut handles: Vec<Handle>, _: &mut Writer| {
            if ordinal != MOUNT {
                return Err(native_abi::ERR_NOT_SUPPORTED);
            }
            let read_only = request.bool()?;
            let block = Channel::from_handle(take_handle(&mut handles, 0)?);
            let mounted = service.mount(block, read_only)?;
            let (client, server) = Channel::create()?;
            let control = mounted.control;
            serve(
                server,
                1,
                b"mount",
                Arc::try_new(move |ordinal: u32, _: &mut Reader<'_>, _, _: &mut Writer| {
                    match ordinal {
                        MAKE_WRITABLE => control.make_writable()?,
                        _ => return Err(native_abi::ERR_NOT_SUPPORTED),
                    }
                    Ok(Vec::new())
                })
                .map_err(|_| native_abi::ERR_NO_MEMORY)?,
            )?;
            Ok(alloc::vec![mounted.fuse.into_handle(), client.into_handle()])
        })
        .map_err(|_| native_abi::ERR_NO_MEMORY)?,
    )
}

/// 使用者端：文件系统服务的控制 channel。
pub struct MountClient {
    channel: Channel,
}

/// 使用者端：一个挂载的控制。
pub struct RemoteMount {
    channel: Channel,
}

impl MountClient {
    pub fn new(channel: Channel) -> Self {
        Self { channel }
    }

    /// 请求挂载 `block`（块设备连接），返回 FUSE channel 与挂载控制。
    ///
    /// # Errors
    ///
    /// 服务报告的挂载失败，或回复不完整。
    pub fn mount(&self, block: Handle, read_only: bool) -> Result<(Channel, RemoteMount), Status> {
        let mut request = Writer::request(MOUNT);
        request.bool(read_only);
        let mut reply = call(&self.channel, &request, alloc::vec![block])?;
        let fuse = Channel::from_handle(take_handle(&mut reply.handles, 0)?);
        let control = Channel::from_handle(take_handle(&mut reply.handles, 1)?);
        Ok((fuse, RemoteMount { channel: control }))
    }
}

impl RemoteMount {
    /// 只读挂载转为可写。
    ///
    /// # Errors
    ///
    /// 服务报告的失败。
    pub fn make_writable(&self) -> Result<(), Status> {
        call(&self.channel, &Writer::request(MAKE_WRITABLE), Vec::new()).map(|_| ())
    }
}
