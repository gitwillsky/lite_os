#![no_std]
#![feature(allocator_ext)]

//! 文件系统服务的挂载协议。
//!
//! 1. 使用者经服务的控制 channel 发出挂载请求，交出块设备连接（`device_protocol::block` 的一条 channel）；
//! 2. 服务打开文件系统，回复该挂载的 FUSE channel（使用者的 FUSE 客户端经它访问文件）与挂载控制 channel；
//! 3. 挂载控制目前只有“转为可写”（只读挂载重新挂载为读写）；卸载经 FUSE `DESTROY` 完成。
//! 4. fshost 拥有块设备与文件系统服务：经块设备清单协议（[`BlocksClient`]）交出块设备的全盘范围连接，
//!    并把挂载协议原样转发给 ext4d（[`MountClient::serve_proxy`]），使用者看到的线格式不变。
//! 5. fshost 在原生数据卷上为每个原生服务派生私有目录（[`StorageAdmin`]，只供根服务），服务经私有目录协议
//!    （[`Storage`]）整文件读写自己的持久状态（Fuchsia 的 `storage` 能力）。

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
        Arc::try_new(
            move |ordinal: u32,
                  request: &mut Reader<'_>,
                  mut handles: Vec<Handle>,
                  _: &mut Writer| {
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
                Ok(alloc::vec![
                    mounted.fuse.into_handle(),
                    client.into_handle()
                ])
            },
        )
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

impl MountClient {
    /// 以本连接服务 `channel` 上的挂载请求：fshost 把使用者的挂载转发给它拥有的文件系统服务。
    ///
    /// # Errors
    ///
    /// 服务线程或闭包分配失败。
    pub fn serve_proxy(self: &Arc<Self>, channel: Channel) -> Result<(), Status> {
        let target = self.clone();
        serve(
            channel,
            1,
            b"mount-proxy",
            Arc::try_new(
                move |ordinal: u32,
                      request: &mut Reader<'_>,
                      mut handles: Vec<Handle>,
                      _: &mut Writer| {
                    if ordinal != MOUNT {
                        return Err(native_abi::ERR_NOT_SUPPORTED);
                    }
                    let read_only = request.bool()?;
                    let (fuse, control) = target.mount(take_handle(&mut handles, 0)?, read_only)?;
                    Ok(alloc::vec![
                        fuse.into_handle(),
                        control.channel.into_handle()
                    ])
                },
            )
            .map_err(|_| native_abi::ERR_NO_MEMORY)?,
        )
    }
}

const BLOCKS_COUNT: u32 = 1;
const BLOCKS_OPEN: u32 = 2;

/// 块设备清单的服务端：启动枚举得到的块设备个数与按序号交出全盘范围连接的函数。
pub type BlockOpener = dyn Fn(usize) -> Result<Handle, Status> + Send + Sync;

/// 服务块设备清单协议。
///
/// # Errors
///
/// 服务线程或闭包分配失败。
pub fn serve_blocks(channel: Channel, count: usize, open: Arc<BlockOpener>) -> Result<(), Status> {
    serve(
        channel,
        1,
        b"block-host",
        Arc::try_new(
            move |ordinal: u32, request: &mut Reader<'_>, _: Vec<Handle>, reply: &mut Writer| {
                match ordinal {
                    BLOCKS_COUNT => {
                        reply.u64(count as u64);
                        Ok(Vec::new())
                    }
                    BLOCKS_OPEN => Ok(alloc::vec![open(request.usize()?)?]),
                    _ => Err(native_abi::ERR_NOT_SUPPORTED),
                }
            },
        )
        .map_err(|_| native_abi::ERR_NO_MEMORY)?,
    )
}

/// 使用者端的块设备清单。
pub struct BlocksClient {
    channel: Channel,
}

impl BlocksClient {
    pub fn new(channel: Channel) -> Self {
        Self { channel }
    }

    /// 块设备个数（启动时发现的设备，序号稳定）。
    ///
    /// # Errors
    ///
    /// 服务已终止。
    pub fn count(&self) -> Result<usize, Status> {
        call(&self.channel, &Writer::request(BLOCKS_COUNT), Vec::new())?
            .reader()
            .usize()
    }

    /// 第 `index` 个块设备的全盘范围连接（`device_protocol::block` 线协议）。
    ///
    /// # Errors
    ///
    /// 序号越界、设备不可共享或服务已终止。
    pub fn open(&self, index: usize) -> Result<Channel, Status> {
        let mut request = Writer::request(BLOCKS_OPEN);
        request.u64(index as u64);
        let mut reply = call(&self.channel, &request, Vec::new())?;
        Ok(Channel::from_handle(take_handle(&mut reply.handles, 0)?))
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

const STORAGE_OPEN: u32 = 1;
const STORAGE_READ: u32 = 1;
const STORAGE_WRITE: u32 = 2;
/// 私有目录中单个文件的上限：整文件随一条消息传递。
pub const STORAGE_FILE_MAX: usize = 48 * 1024;
/// 文件名上限；名称不得含 `/` 或 NUL，也不得是 `.` 或 `..`。
pub const STORAGE_NAME_MAX: usize = 64;

/// 校验私有目录中的文件名。
///
/// # Errors
///
/// 名称为空、过长、含路径分隔符或 NUL、或为 `.`/`..`。
pub fn storage_name(name: &[u8]) -> Result<&[u8], Status> {
    if name.is_empty()
        || name.len() > STORAGE_NAME_MAX
        || name.contains(&b'/')
        || name.contains(&0)
        || name == b"."
        || name == b".."
    {
        return Err(native_abi::ERR_INVALID_ARGS);
    }
    Ok(name)
}

/// 存储管理的服务端：按服务名派生私有目录连接。
pub type StorageOpener = dyn Fn(&[u8]) -> Result<Channel, Status> + Send + Sync;

/// 服务存储管理协议。
///
/// # Errors
///
/// 服务线程或闭包分配失败。
pub fn serve_storage_admin(channel: Channel, open: Arc<StorageOpener>) -> Result<(), Status> {
    serve(
        channel,
        1,
        b"storage-admin",
        Arc::try_new(
            move |ordinal: u32, request: &mut Reader<'_>, _: Vec<Handle>, _: &mut Writer| {
                if ordinal != STORAGE_OPEN {
                    return Err(native_abi::ERR_NOT_SUPPORTED);
                }
                Ok(alloc::vec![
                    open(storage_name(request.bytes()?)?)?.into_handle()
                ])
            },
        )
        .map_err(|_| native_abi::ERR_NO_MEMORY)?,
    )
}

/// 使用者端（根服务）的存储管理。
pub struct StorageAdmin(Channel);

impl StorageAdmin {
    pub fn new(channel: Channel) -> Self {
        Self(channel)
    }

    /// 服务 `name` 的私有目录连接；目录不存在时创建。
    ///
    /// # Errors
    ///
    /// 名称非法、数据卷不可用或服务已终止。
    pub fn open(&self, name: &[u8]) -> Result<Channel, Status> {
        let mut request = Writer::request(STORAGE_OPEN);
        request.bytes(name);
        let mut reply = call(&self.0, &request, Vec::new())?;
        Ok(Channel::from_handle(take_handle(&mut reply.handles, 0)?))
    }
}

/// 服务端：一个私有目录。
pub trait StorageService: Send + Sync {
    /// 读取整个文件；不存在返回 `None`。
    ///
    /// # Errors
    ///
    /// 文件超过 [`STORAGE_FILE_MAX`] 或 I/O 失败。
    fn read(&self, name: &[u8]) -> Result<Option<Vec<u8>>, Status>;

    /// 原子替换整个文件：读者只会看到旧内容或完整的新内容，返回前内容已持久化。
    ///
    /// # Errors
    ///
    /// 空间不足或 I/O 失败。
    fn write(&self, name: &[u8], bytes: &[u8]) -> Result<(), Status>;
}

/// 服务私有目录协议。
///
/// # Errors
///
/// 服务线程或闭包分配失败。
pub fn serve_storage(channel: Channel, service: Arc<dyn StorageService>) -> Result<(), Status> {
    serve(
        channel,
        1,
        b"storage",
        Arc::try_new(
            move |ordinal: u32, request: &mut Reader<'_>, _: Vec<Handle>, reply: &mut Writer| {
                let name = storage_name(request.bytes()?)?;
                match ordinal {
                    STORAGE_READ => match service.read(name)? {
                        Some(bytes) => {
                            reply.bool(true).bytes(&bytes);
                        }
                        None => {
                            reply.bool(false);
                        }
                    },
                    STORAGE_WRITE => {
                        let bytes = request.bytes()?;
                        if bytes.len() > STORAGE_FILE_MAX {
                            return Err(native_abi::ERR_OUT_OF_RANGE);
                        }
                        service.write(name, bytes)?;
                    }
                    _ => return Err(native_abi::ERR_NOT_SUPPORTED),
                }
                Ok(Vec::new())
            },
        )
        .map_err(|_| native_abi::ERR_NO_MEMORY)?,
    )
}

/// 使用者端的私有目录。
pub struct Storage(Channel);

impl Storage {
    pub fn new(channel: Channel) -> Self {
        Self(channel)
    }

    /// 读取整个文件；不存在返回 `None`。
    ///
    /// # Errors
    ///
    /// 名称非法、I/O 失败或服务已终止。
    pub fn read(&self, name: &[u8]) -> Result<Option<Vec<u8>>, Status> {
        let mut request = Writer::request(STORAGE_READ);
        request.bytes(storage_name(name)?);
        let reply = call(&self.0, &request, Vec::new())?;
        let mut reader = reply.reader();
        if !reader.bool()? {
            return Ok(None);
        }
        Ok(Some(reader.bytes()?.to_vec()))
    }

    /// 原子替换整个文件，返回前已持久化。
    ///
    /// # Errors
    ///
    /// 名称非法、文件过大、空间不足、I/O 失败或服务已终止。
    pub fn write(&self, name: &[u8], bytes: &[u8]) -> Result<(), Status> {
        if bytes.len() > STORAGE_FILE_MAX {
            return Err(native_abi::ERR_OUT_OF_RANGE);
        }
        let mut request = Writer::request(STORAGE_WRITE);
        request.bytes(storage_name(name)?).bytes(bytes);
        call(&self.0, &request, Vec::new()).map(drop)
    }
}
