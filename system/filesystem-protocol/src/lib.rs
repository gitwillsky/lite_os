#![no_std]
#![feature(allocator_ext)]

//! 文件系统服务的挂载协议。
//!
//! 1. 使用者经服务的控制 channel 发出挂载请求，交出块设备连接（`device_protocol::block` 的一条 channel）；
//! 2. 服务打开文件系统，回复该挂载的 FUSE channel（使用者的 FUSE 客户端经它访问文件）与挂载控制 channel；
//! 3. 挂载控制：“转为可写”（只读挂载重新挂载为读写）与普通文件的内容 VMO（mmap 映射服务端拥有的页缓存）；
//!    卸载经 FUSE `DESTROY` 完成。
//! 4. fshost 拥有块设备与文件系统服务实例：经块设备清单协议（[`BlocksClient`]）交出块设备的全盘范围连接，
//!    经卷挂载协议（[`VolumeMountClient`]）按盘序号挂载——fshost 为每块盘启动一个 ext4 实例，
//!    自己建立块设备连接并以上面的挂载协议交给实例。
//! 5. fshost 在原生数据卷上为每个原生服务派生私有目录（[`StorageAdmin`]，只供根服务），服务经私有目录协议
//!    （[`Storage`]）整文件读写自己的持久状态（Fuchsia 的 `storage` 能力）。

extern crate alloc;

pub mod directory;

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
const BACKING_MEMORY: u32 = 2;

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

    /// 普通文件 `node`（FUSE nodeid）的内容 VMO（fuchsia.io `GetBackingMemory`）：服务端是其大小与内容的
    /// 唯一 owner。句柄包含 `INSPECT`，供使用者在映射和读取前查询对象编号与大小。
    ///
    /// # Errors
    ///
    /// 节点不存在（`ERR_NOT_FOUND`）、不是普通文件（`ERR_WRONG_TYPE`）或 VMO 建立失败。
    fn backing_memory(&self, node: u64) -> Result<Vmo, Status>;
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
                    Arc::try_new(
                        move |ordinal: u32, request: &mut Reader<'_>, _, _: &mut Writer| {
                            match ordinal {
                                MAKE_WRITABLE => control.make_writable().map(|()| Vec::new()),
                                BACKING_MEMORY => Ok(alloc::vec![
                                    control.backing_memory(request.u64()?)?.into_handle()
                                ]),
                                _ => Err(native_abi::ERR_NOT_SUPPORTED),
                            }
                        },
                    )
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

const MOUNT_VOLUME: u32 = 1;

/// fshost 端：挂载块设备清单中第 `disk` 块盘上指定逻辑块范围内的 ext4。fshost 为每块盘维持一个文件系统服务实例，
/// 同一盘的再次挂载（使用者重启）交给同一实例，由实例串行化同一文件系统的会话。
pub trait VolumeMountService: Send + Sync {
    /// # Errors
    ///
    /// 序号或块范围越界、实例启动失败或实例报告的挂载失败。
    fn mount_volume(
        &self,
        disk: usize,
        first: u64,
        count: u64,
        read_only: bool,
    ) -> Result<(Channel, RemoteMount), Status>;
}

/// 服务卷挂载协议。
///
/// # Errors
///
/// 服务线程或闭包分配失败。
pub fn serve_volume_mounts(
    channel: Channel,
    service: Arc<dyn VolumeMountService>,
) -> Result<(), Status> {
    serve(
        channel,
        1,
        b"volume-mount",
        Arc::try_new(
            move |ordinal: u32, request: &mut Reader<'_>, _: Vec<Handle>, _: &mut Writer| {
                if ordinal != MOUNT_VOLUME {
                    return Err(native_abi::ERR_NOT_SUPPORTED);
                }
                let disk = request.usize()?;
                let first = request.u64()?;
                let count = request.u64()?;
                let read_only = request.bool()?;
                let (fuse, control) = service.mount_volume(disk, first, count, read_only)?;
                Ok(alloc::vec![
                    fuse.into_handle(),
                    control.channel.into_handle()
                ])
            },
        )
        .map_err(|_| native_abi::ERR_NO_MEMORY)?,
    )
}

/// 使用者端：fshost 的卷挂载连接。
pub struct VolumeMountClient {
    channel: Channel,
}

impl VolumeMountClient {
    pub fn new(channel: Channel) -> Self {
        Self { channel }
    }

    /// 挂载第 `disk` 块盘的 `[first, first + count)` 逻辑块，返回 FUSE channel 与挂载控制。
    ///
    /// # Errors
    ///
    /// fshost 或文件系统服务报告的挂载失败，或回复不完整。
    pub fn mount_volume(
        &self,
        disk: usize,
        first: u64,
        count: u64,
        read_only: bool,
    ) -> Result<(Channel, RemoteMount), Status> {
        let mut request = Writer::request(MOUNT_VOLUME);
        request
            .u64(disk as u64)
            .u64(first)
            .u64(count)
            .bool(read_only);
        let mut reply = call(&self.channel, &request, Vec::new())?;
        let fuse = Channel::from_handle(take_handle(&mut reply.handles, 0)?);
        let control = Channel::from_handle(take_handle(&mut reply.handles, 1)?);
        Ok((fuse, RemoteMount { channel: control }))
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

    /// 取得普通文件 `node`（FUSE nodeid）的内容 VMO，见 [`MountControl::backing_memory`]。
    ///
    /// # Errors
    ///
    /// 服务报告的失败，或回复不完整。
    pub fn backing_memory(&self, node: u64) -> Result<Vmo, Status> {
        let mut request = Writer::request(BACKING_MEMORY);
        request.u64(node);
        let mut reply = call(&self.channel, &request, Vec::new())?;
        Ok(Vmo::from_handle(take_handle(&mut reply.handles, 0)?))
    }
}

const STORAGE_OPEN: u32 = 1;
const STORAGE_READ: u32 = 1;
const STORAGE_WRITE: u32 = 2;
const STORAGE_REMOVE: u32 = 3;
const STORAGE_SYNC: u32 = 4;
const STORAGE_LIST: u32 = 5;
const STORAGE_CREATE: u32 = 6;
/// 私有文件控制消息版本；内容一律使用只读 VMO，大小不受 channel 字节容量限制。
const STORAGE_VERSION: u32 = 1;
/// 文件名上限；名称不得含 `/` 或 NUL，也不得是 `.` 或 `..`。
pub const STORAGE_NAME_MAX: usize = 64;

/// 私有目录的普通文件事实；大小与修改时刻只由实际文件系统提供。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageEntry {
    /// 经 storage_name 校验的直接子文件名。
    pub name: Vec<u8>,
    /// 逻辑文件大小。
    pub size: u64,
    /// 文件系统的 UTC 修改秒数。
    pub modified_seconds: i64,
}

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
            move |ordinal: u32, request: &mut Reader<'_>, handles: Vec<Handle>, _: &mut Writer| {
                if ordinal != STORAGE_OPEN {
                    return Err(native_abi::ERR_NOT_SUPPORTED);
                }
                let name = storage_name(request.bytes()?)?;
                request.finish()?;
                if !handles.is_empty() {
                    return Err(native_abi::ERR_INVALID_ARGS);
                }
                Ok(alloc::vec![open(name)?.into_handle()])
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
        request.bytes(storage_name(name)?);
        let mut reply = call(&self.0, &request, Vec::new())?;
        if reply.handles.len() != 1 {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        reply.reader().finish()?;
        Ok(Channel::from_handle(take_handle(&mut reply.handles, 0)?))
    }
}

/// 一个私有目录的存储能力；实际 provider 与远程 client 使用同一操作契约。
pub trait StorageService: Send + Sync {
    /// 枚举符合文件名契约的直接普通子文件，不包含目录。
    /// # Errors
    /// 枚举/元数据读取、分配或传输失败。
    fn list(&self) -> Result<Vec<StorageEntry>, Status>;

    /// 原子创建并持久化新文件；名字存在返回 ALREADY_BOUND，绝不替换原内容。
    /// # Errors
    /// 名字冲突、资源或 I/O 失败；目录同步/回复丢失时，错误不证明尚未创建。
    fn create(&self, name: &[u8], bytes: &[u8]) -> Result<(), Status>;

    /// 读取整个文件；不存在返回 `None`。
    ///
    /// # Errors
    ///
    /// 内容分配或 I/O 失败。
    fn read(&self, name: &[u8]) -> Result<Option<Vec<u8>>, Status>;

    /// 原子替换整个文件：读者只会看到旧内容或完整的新内容，返回前内容已持久化。
    ///
    /// # Errors
    ///
    /// 空间不足或 I/O 失败。rename/目录同步或回复传输失败时，错误不证明原文件未被替换。
    fn write(&self, name: &[u8], bytes: &[u8]) -> Result<(), Status>;

    /// 删除文件并同步目录；不存在也成功，以便撤销首次创建或重试未确认的删除。
    ///
    /// # Errors
    /// I/O 失败；错误不证明删除尚未发生。
    fn remove(&self, name: &[u8]) -> Result<(), Status>;

    /// 同步私有目录的命名状态，不创建或重写文件。
    ///
    /// # Errors
    /// I/O 失败或服务不可用。
    fn sync(&self) -> Result<(), Status>;
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
            move |ordinal: u32,
                  request: &mut Reader<'_>,
                  mut handles: Vec<Handle>,
                  reply: &mut Writer| {
                if request.u32()? != STORAGE_VERSION {
                    return Err(native_abi::ERR_INVALID_ARGS);
                }
                // 固定回复头在文件副作用之前准备；内容读操作另行返回独立能力。
                reply.try_reserve(13)?;
                reply.u32(STORAGE_VERSION);
                if ordinal == STORAGE_LIST {
                    request.finish()?;
                    if !handles.is_empty() {
                        return Err(native_abi::ERR_INVALID_ARGS);
                    }
                    let entries = service.list()?;
                    let mut data = Writer::payload();
                    let length = entries
                        .iter()
                        .try_fold(8usize, |total, entry| {
                            total.checked_add(20 + entry.name.len())
                        })
                        .ok_or(native_abi::ERR_OUT_OF_RANGE)?;
                    data.try_reserve(length)?;
                    data.u64(entries.len() as u64);
                    for entry in entries {
                        data.bytes(storage_name(&entry.name)?)
                            .u64(entry.size)
                            .u64(entry.modified_seconds as u64);
                    }
                    let content = Vmo::from_bytes(data.as_bytes())?;
                    reply.u64(data.as_bytes().len() as u64);
                    return Ok(alloc::vec![content.into_handle()]);
                }
                if ordinal == STORAGE_SYNC {
                    request.finish()?;
                    if !handles.is_empty() {
                        return Err(native_abi::ERR_INVALID_ARGS);
                    }
                    service.sync()?;
                    return Ok(Vec::new());
                }
                let name = storage_name(request.bytes()?)?;
                match ordinal {
                    STORAGE_READ => {
                        request.finish()?;
                        if !handles.is_empty() {
                            return Err(native_abi::ERR_INVALID_ARGS);
                        }
                        if let Some(bytes) = service.read(name)? {
                            let content = Vmo::from_bytes(&bytes)?;
                            let mut returned = Vec::new();
                            returned
                                .try_reserve_exact(1)
                                .map_err(|_| native_abi::ERR_NO_MEMORY)?;
                            returned.push(content.into_handle());
                            reply.bool(true).u64(bytes.len() as u64);
                            return Ok(returned);
                        }
                        reply.bool(false);
                    }
                    STORAGE_WRITE | STORAGE_CREATE => {
                        let length = request.u64()?;
                        request.finish()?;
                        if handles.len() != 1 {
                            return Err(native_abi::ERR_INVALID_ARGS);
                        }
                        let content =
                            Vmo::from_handle(take_handle(&mut handles, 0)?).read_bytes(length)?;
                        if ordinal == STORAGE_CREATE {
                            service.create(name, &content)?;
                        } else {
                            service.write(name, &content)?;
                        }
                    }
                    STORAGE_REMOVE => {
                        request.finish()?;
                        if !handles.is_empty() {
                            return Err(native_abi::ERR_INVALID_ARGS);
                        }
                        service.remove(name)?;
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

    /// 启动者派生的私有能力连接；关闭后无法原地恢复，使用者应由其组件生命周期重取。
    pub fn channel(&self) -> &Channel {
        &self.0
    }
}

impl StorageService for Storage {
    fn list(&self) -> Result<Vec<StorageEntry>, Status> {
        let mut request = Writer::request(STORAGE_LIST);
        request.u32(STORAGE_VERSION);
        let mut reply = call(&self.0, &request, Vec::new())?;
        let mut reader = reply.reader();
        if reader.u32()? != STORAGE_VERSION || reply.handles.len() != 1 {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        let length = reader.u64()?;
        reader.finish()?;
        let bytes = Vmo::from_handle(take_handle(&mut reply.handles, 0)?).read_bytes(length)?;
        let mut reader = Reader::new(&bytes);
        let count = reader.usize()?;
        if count > bytes.len() / 21 {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(count)
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        for _ in 0..count {
            entries.push(StorageEntry {
                name: storage_name(reader.bytes()?)?.to_vec(),
                size: reader.u64()?,
                modified_seconds: reader.u64()? as i64,
            });
        }
        reader.finish()?;
        Ok(entries)
    }

    fn create(&self, name: &[u8], bytes: &[u8]) -> Result<(), Status> {
        self.store(STORAGE_CREATE, name, bytes)
    }
    fn sync(&self) -> Result<(), Status> {
        let mut request = Writer::request(STORAGE_SYNC);
        request.u32(STORAGE_VERSION);
        self.empty_reply(call(&self.0, &request, Vec::new())?)
    }

    fn read(&self, name: &[u8]) -> Result<Option<Vec<u8>>, Status> {
        let mut request = Writer::request(STORAGE_READ);
        request.u32(STORAGE_VERSION).bytes(storage_name(name)?);
        let mut reply = call(&self.0, &request, Vec::new())?;
        let mut reader = reply.reader();
        if reader.u32()? != STORAGE_VERSION {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        let present = reader.u8()?;
        let length = match present {
            0 => None,
            1 => Some(reader.u64()?),
            _ => return Err(native_abi::ERR_INVALID_ARGS),
        };
        reader.finish()?;
        match length {
            None if reply.handles.is_empty() => Ok(None),
            Some(length) if reply.handles.len() == 1 => {
                let content = Vmo::from_handle(take_handle(&mut reply.handles, 0)?);
                content.read_bytes(length).map(Some)
            }
            _ => Err(native_abi::ERR_INVALID_ARGS),
        }
    }

    fn write(&self, name: &[u8], bytes: &[u8]) -> Result<(), Status> {
        self.store(STORAGE_WRITE, name, bytes)
    }

    fn remove(&self, name: &[u8]) -> Result<(), Status> {
        let mut request = Writer::request(STORAGE_REMOVE);
        request.u32(STORAGE_VERSION).bytes(storage_name(name)?);
        self.empty_reply(call(&self.0, &request, Vec::new())?)
    }
}

impl Storage {
    fn store(&self, operation: u32, name: &[u8], bytes: &[u8]) -> Result<(), Status> {
        let mut request = Writer::request(operation);
        request
            .u32(STORAGE_VERSION)
            .bytes(storage_name(name)?)
            .u64(bytes.len() as u64);
        let content = Vmo::from_bytes(bytes)?;
        self.empty_reply(call(&self.0, &request, alloc::vec![content.into_handle()])?)
    }
    fn empty_reply(&self, reply: service_rpc::Reply) -> Result<(), Status> {
        let mut reader = reply.reader();
        if reader.u32()? != STORAGE_VERSION || !reply.handles.is_empty() {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        reader.finish()
    }
}
