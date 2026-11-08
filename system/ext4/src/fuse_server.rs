//! FUSE 线协议服务：每个 ext4 挂载一条 FUSE channel。
//!
//! 1. 控制 channel 的挂载请求交来块设备连接；打开 ext4 后创建 FUSE channel，服务端一端由若干工作线程读取请求；
//! 2. nodeid：根目录为 `FUSE_ROOT_ID`，其余为 ext4 inode 号；节点表持有每个 nodeid 的 inode 与 lookup 计数，
//!    FORGET 归零时释放——unlink 后仍被打开或缓存的 inode 由此保持到最后一个引用释放，与 ext4 的孤儿回收一致；
//! 3. 打开句柄 fh 持有 inode 引用，RELEASE 释放；
//! 4. 调用者的权限检查由 linuxd 在发出请求前完成（`default_permissions`），这里执行请求并按 FUSE 回复。
//!    新建 inode 的属主取请求头的 UID/GID；父目录带 set-GID 时继承其组（目录同时继承 set-GID 位）。

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use device_model::BlockDevice;
use filesystem_protocol::{MAX_TRANSFER, MountControl, MountService, Mounted};
use linux_abi::{errno, fuse};
use native_runtime::native_abi::{self, DEADLINE_INFINITE, Status, signals};
use native_runtime::sync::{Mutex, Once};
use native_runtime::{Channel, Event, Vmo};

use crate::content::Content;
use crate::ext4::{Ext4FileSystem, Ext4Inode};
use crate::vfs::{
    CreateMetadata, DeviceNumber, DirectoryEntry, DirectoryVisit, DirectoryVisitor,
    FileSystemError, InodeMetadata, InodeType, OwnerModeChange, WritePolicy, XattrChange,
};

/// 每个挂载的工作线程数：请求在 ext4 内部按 mutation owner 串行，多个线程让读与元数据查询并行。
const WORKERS: usize = 4;
/// 属性与目录项缓存有效期（秒）：ext4 只经 linuxd 修改，linuxd 在自己的修改后作废缓存。
const VALID_SECONDS: u64 = 60;

const FATTR_ATIME_NOW: u32 = 1 << 7;
const FATTR_MTIME_NOW: u32 = 1 << 8;
const FATTR_KILL_SUIDGID: u32 = 1 << 11;
const S_IFMT: u32 = 0o170000;
const S_ISGID: u32 = 0o2000;
/// `XATTR_REPLACE`：属性必须已存在。
const XATTR_REPLACE: u32 = 2;

fn status(error: FileSystemError) -> Status {
    match error {
        FileSystemError::OutOfMemory => native_abi::ERR_NO_MEMORY,
        FileSystemError::NotFound => native_abi::ERR_NOT_FOUND,
        FileSystemError::InvalidOperation | FileSystemError::InvalidFileSystem => {
            native_abi::ERR_INVALID_ARGS
        }
        _ => native_abi::ERR_IO,
    }
}

/// ext4 超级块中文件系统 UUID 的字节偏移（超级块起于 1024，`s_uuid` 位于其 0x68）。
const UUID_OFFSET: usize = 1024 + 0x68;

// OWNER: 尚未完成 shutdown 的挂载的文件系统 UUID。同一文件系统只会在使用者重启时被再次挂载（使用者
// 自己合并同一文件系统的多次挂载）；新挂载等旧挂载拆除完毕，缺失时两个挂载会同时回放并写入同一日志。
static LIVE: Mutex<Vec<[u8; 16]>> = Mutex::new(Vec::new());
// OWNER: 某个挂载完成拆除时置位 `USER_0`；首次使用时创建。
static SETTLED: Once<Event> = Once::new();

/// 一个挂载对其文件系统 UUID 的占用；析构时（拆除完毕或挂载失败）释放并通知等待者。
struct Claim([u8; 16]);

impl Claim {
    /// 等待同一文件系统的旧挂载拆除完毕后占用。
    fn acquire(uuid: [u8; 16]) -> Result<Self, Status> {
        let settled = SETTLED.try_call_once(Event::create)?;
        loop {
            settled.signal(signals::USER_0, 0)?;
            {
                let mut live = LIVE.lock();
                if !live.contains(&uuid) {
                    live.try_reserve(1).map_err(|_| native_abi::ERR_NO_MEMORY)?;
                    live.push(uuid);
                    return Ok(Self(uuid));
                }
            }
            settled.wait_one(signals::USER_0, DEADLINE_INFINITE)?;
        }
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        LIVE.lock().retain(|uuid| *uuid != self.0);
        if let Some(settled) = SETTLED.get() {
            let _ = settled.signal(0, signals::USER_0);
        }
    }
}

/// 读取块设备上 ext4 超级块的 UUID（日志回放之前，只读）。
fn filesystem_uuid(device: &dyn BlockDevice) -> Result<[u8; 16], Status> {
    let block_size = device.block_size();
    let mut block = alloc::vec![0u8; block_size];
    device
        .read_blocks(UUID_OFFSET / block_size, &mut block)
        .map_err(|_| native_abi::ERR_IO)?;
    let start = UUID_OFFSET % block_size;
    block
        .get(start..start + 16)
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or(native_abi::ERR_INVALID_ARGS)
}

/// 挂载服务：打开块设备上的 ext4 并为它建立 FUSE channel。
struct Ext4Service;

impl MountService for Ext4Service {
    fn mount(&self, block: Channel, read_only: bool) -> Result<Mounted, Status> {
        let device = device_protocol::block::from_channel(block)?;
        let claim = Claim::acquire(filesystem_uuid(&*device)?)?;
        let filesystem = Ext4FileSystem::new(device, read_only).map_err(status)?;
        filesystem.start_writeback().map_err(status)?;
        let root = filesystem.root_inode().map_err(status)?;
        let mount = Arc::try_new(Mount {
            filesystem: filesystem.clone(),
            nodes: Mutex::new(BTreeMap::new()),
            handles: Mutex::new(BTreeMap::new()),
            contents: Mutex::new(BTreeMap::new()),
            next_handle: AtomicU64::new(1),
            destroyed: AtomicBool::new(false),
            claim: Mutex::new(Some(claim)),
        })
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        mount.nodes.lock().insert(
            fuse::ROOT_ID,
            Node {
                inode: root,
                lookups: 1,
            },
        );
        let (client, server) = Channel::create()?;
        let session = FuseSession(mount.clone());
        let handler: Arc<service_rpc::MessageHandler<'static>> =
            Arc::try_new(move |channel: &Channel, message| session.serve(channel, message))
                .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        service_rpc::serve_messages(server, WORKERS, b"ext4-fuse", handler)?;
        let writeback = mount.clone();
        let thread = native_runtime::thread::spawn(b"ext4-content-writeback", move || {
            writeback.flush_loop()
        })?;
        drop(thread);
        let control: Arc<dyn MountControl> = Arc::try_new(Ext4Control { filesystem, mount })
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        Ok(Mounted {
            fuse: client,
            control,
        })
    }
}

struct Ext4Control {
    filesystem: Arc<Ext4FileSystem>,
    mount: Arc<Mount>,
}

impl MountControl for Ext4Control {
    fn make_writable(&self) -> Result<(), Status> {
        self.filesystem.remount_writable().map_err(status)
    }

    fn backing_memory(&self, node: u64) -> Result<Vmo, Status> {
        let inode = self
            .mount
            .node(node)
            .map_err(|_| native_abi::ERR_NOT_FOUND)?;
        self.mount
            .content(&inode)
            .map_err(status)?
            .ok_or(native_abi::ERR_WRONG_TYPE)?
            .backing_memory()
    }
}

/// 周期回写的间隔（纳秒），与日志的 5 秒提交周期一致。
const CONTENT_WRITEBACK_INTERVAL: i64 = 5_000_000_000;

/// 服务控制 channel 直到使用者关闭它。
pub(crate) fn serve_control(control: Channel) -> i64 {
    let service: Arc<dyn MountService> = Arc::new(Ext4Service);
    filesystem_protocol::serve_mounts(control, service).expect("ext4 control service");
    info!("ext4 ready");
    // 控制与挂载都在服务线程上；主线程只结束自己。
    native_runtime::task::exit_thread()
}

struct Node {
    inode: Arc<Ext4Inode>,
    lookups: u64,
}

struct Mount {
    filesystem: Arc<Ext4FileSystem>,
    // OWNER: nodeid → inode 与内核（linuxd FUSE 客户端）持有的 lookup 计数；FORGET 归零时移除。
    // 缺失时被打开后 unlink 的文件在最后一次关闭前就被回收。
    nodes: Mutex<BTreeMap<u64, Node>>,
    // OWNER: 打开句柄 fh → inode；RELEASE 移除。
    handles: Mutex<BTreeMap<u64, Arc<Ext4Inode>>>,
    // OWNER: inode 号 → 普通文件的内容 VMO；节点被 FORGET 或挂载拆除时写回后移除。缺失时 mmap 与
    // read/write 各看到一份内容，映射写入永远不会到达存储。
    contents: Mutex<BTreeMap<u64, Arc<Content>>>,
    next_handle: AtomicU64,
    // OWNER: DESTROY 已执行 shutdown；连接随后断开时不再重复。
    destroyed: AtomicBool,
    // OWNER: 文件系统 UUID 的占用；shutdown 完成后释放，同一文件系统的新挂载随之继续。
    claim: Mutex<Option<Claim>>,
}

/// 一条请求的头部（`fuse_in_header`）。
struct Header {
    opcode: u32,
    unique: u64,
    nodeid: u64,
    uid: u32,
    gid: u32,
}

struct Input<'a> {
    bytes: &'a [u8],
}

impl<'a> Input<'a> {
    fn take(&mut self, length: usize) -> Result<&'a [u8], i32> {
        if self.bytes.len() < length {
            return Err(errno::EINVAL as i32);
        }
        let (head, tail) = self.bytes.split_at(length);
        self.bytes = tail;
        Ok(head)
    }

    fn u32(&mut self) -> Result<u32, i32> {
        Ok(u32::from_ne_bytes(
            self.take(4)?.try_into().expect("four bytes"),
        ))
    }

    fn u64(&mut self) -> Result<u64, i32> {
        Ok(u64::from_ne_bytes(
            self.take(8)?.try_into().expect("eight bytes"),
        ))
    }

    /// NUL 结尾的名字。
    fn name(&mut self) -> Result<&'a [u8], i32> {
        let end = self
            .bytes
            .iter()
            .position(|&byte| byte == 0)
            .ok_or(errno::EINVAL as i32)?;
        let name = &self.bytes[..end];
        self.bytes = &self.bytes[end + 1..];
        Ok(name)
    }

    fn rest(&mut self) -> &'a [u8] {
        core::mem::take(&mut self.bytes)
    }
}

/// 回复编码（原生字节序）。
struct Output(Vec<u8>);

impl Output {
    fn new() -> Self {
        Self(Vec::new())
    }

    fn u32(&mut self, value: u32) -> &mut Self {
        self.0.extend_from_slice(&value.to_ne_bytes());
        self
    }

    fn u64(&mut self, value: u64) -> &mut Self {
        self.0.extend_from_slice(&value.to_ne_bytes());
        self
    }

    fn u16(&mut self, value: u16) -> &mut Self {
        self.0.extend_from_slice(&value.to_ne_bytes());
        self
    }

    fn bytes(&mut self, value: &[u8]) -> &mut Self {
        self.0.extend_from_slice(value);
        self
    }

    fn zeros(&mut self, count: usize) -> &mut Self {
        self.0.resize(self.0.len() + count, 0);
        self
    }

    /// `fuse_attr`（88 字节）。
    fn attr(&mut self, nodeid: u64, metadata: &InodeMetadata) -> &mut Self {
        self.u64(nodeid)
            .u64(metadata.size)
            .u64(metadata.blocks)
            .u64(metadata.atime)
            .u64(metadata.mtime)
            .u64(metadata.ctime)
            .u32(metadata.fine_times.atime_nsec)
            .u32(metadata.fine_times.mtime_nsec)
            .u32(metadata.fine_times.ctime_nsec)
            .u32(metadata.mode)
            .u32(metadata.links)
            .u32(metadata.uid)
            .u32(metadata.gid)
            .u32(metadata.device.map_or(0, DeviceNumber::encode))
            .u32(metadata.block_size)
            .u32(0)
    }

    /// `fuse_entry_out`。
    fn entry(&mut self, nodeid: u64, metadata: &InodeMetadata) -> &mut Self {
        self.u64(nodeid)
            .u64(0)
            .u64(VALID_SECONDS)
            .u64(VALID_SECONDS)
            .u32(0)
            .u32(0)
            .attr(nodeid, metadata)
    }

    /// `fuse_attr_out`。
    fn attr_out(&mut self, nodeid: u64, metadata: &InodeMetadata) -> &mut Self {
        self.u64(VALID_SECONDS).u32(0).u32(0).attr(nodeid, metadata)
    }
}

type Reply = Result<Option<Output>, i32>;

fn fs_error(error: FileSystemError) -> i32 {
    error.errno()
}

/// xattr 操作的失败：属性不存在是 `ENODATA`（Linux getxattr/removexattr/XATTR_REPLACE），不是 `ENOENT`。
fn xattr_error(error: FileSystemError) -> i32 {
    match error {
        FileSystemError::NotFound => errno::ENODATA as i32,
        other => fs_error(other),
    }
}

/// FUSE 连接寿命独立于控制端点和回写线程持有的 Mount；最后一个 dispatcher 退出后才关闭文件系统。
/// 不能让第一个空闲 reader 退出就 shutdown，也不能等 Mount 最后引用（回写线程本身持有它）。
struct FuseSession(Arc<Mount>);
impl Drop for FuseSession {
    fn drop(&mut self) {
        let _ = self.0.finish();
    }
}
impl FuseSession {
    fn serve(&self, channel: &Channel, message: native_runtime::Message) -> Result<(), Status> {
        // 格式错误的帧没有可回复的请求头，保持既有丢弃语义。
        let Ok(bytes) = filesystem_protocol::receive_fuse(message) else {
            return Ok(());
        };
        let mut input = Input { bytes: &bytes };
        let Some(header) = Mount::header(&mut input) else {
            return Ok(());
        };
        let reply = self.0.dispatch(&header, &mut input);
        if matches!(
            header.opcode,
            fuse::FORGET | fuse::BATCH_FORGET | fuse::INTERRUPT
        ) {
            return Ok(());
        }
        let mut message = Output::new();
        match reply {
            Ok(body) => {
                let body = body.map_or_else(Vec::new, |body| body.0);
                message
                    .u32((fuse::OUT_HEADER_SIZE + body.len()) as u32)
                    .u32(0)
                    .u64(header.unique)
                    .bytes(&body);
            }
            Err(code) => {
                message
                    .u32(fuse::OUT_HEADER_SIZE as u32)
                    .u32((-code) as u32)
                    .u64(header.unique);
            }
        }
        filesystem_protocol::send_fuse(channel, &message.0)
    }
}

impl Mount {
    /// 恰好一次地拆除挂载并释放文件系统 UUID 的占用。shutdown 失败同样释放：失败后的文件系统
    /// 已停止写回，不会再写日志。
    fn finish(&self) -> Result<(), FileSystemError> {
        if self.destroyed.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        let contents = core::mem::take(&mut *self.contents.lock());
        let written = contents
            .values()
            .try_for_each(|content| content.writeback());
        drop(contents);
        // 写回失败也必须 shutdown 并释放占用：失败后的文件系统不再写日志。
        let shutdown = self.filesystem.shutdown();
        let result = written.and(shutdown);
        drop(self.claim.lock().take());
        result
    }

    fn header(input: &mut Input<'_>) -> Option<Header> {
        let _length = input.u32().ok()?;
        let opcode = input.u32().ok()?;
        let unique = input.u64().ok()?;
        let nodeid = input.u64().ok()?;
        let uid = input.u32().ok()?;
        let gid = input.u32().ok()?;
        let _pid = input.u32().ok()?;
        let _extension = input.take(4).ok()?;
        Some(Header {
            opcode,
            unique,
            nodeid,
            uid,
            gid,
        })
    }

    fn node(&self, nodeid: u64) -> Result<Arc<Ext4Inode>, i32> {
        self.nodes
            .lock()
            .get(&nodeid)
            .map(|node| node.inode.clone())
            .ok_or(errno::ESTALE as i32)
    }

    fn handle(&self, handle: u64) -> Result<Arc<Ext4Inode>, i32> {
        self.handles
            .lock()
            .get(&handle)
            .cloned()
            .ok_or(errno::EBADF as i32)
    }

    /// 普通文件的内容 VMO（首次使用时建立）；其他类型返回 `None`。
    fn content(&self, inode: &Arc<Ext4Inode>) -> Result<Option<Arc<Content>>, FileSystemError> {
        if inode.inode_type() != InodeType::File {
            return Ok(None);
        }
        let mut contents = self.contents.lock();
        if let Some(content) = contents.get(&inode.number()) {
            return Ok(Some(content.clone()));
        }
        let content = Content::new(inode.clone())?;
        contents.insert(inode.number(), content.clone());
        Ok(Some(content))
    }

    /// 已建立的内容 VMO；大小改变或持久化时只需处理已缓存的文件。
    fn cached_content(&self, inode: &Ext4Inode) -> Option<Arc<Content>> {
        self.contents.lock().get(&inode.number()).cloned()
    }

    /// 写回全部内容 VMO 的映射写入。
    fn writeback_contents(&self) -> Result<(), FileSystemError> {
        let contents: Vec<_> = self.contents.lock().values().cloned().collect();
        contents.iter().try_for_each(|content| content.writeback())
    }

    /// 周期写回映射写入，直到挂载拆除。
    fn flush_loop(&self) {
        while !self.destroyed.load(Ordering::Acquire) {
            let deadline = native_runtime::system::monotonic() + CONTENT_WRITEBACK_INTERVAL;
            let _ = native_runtime::system::sleep_until(deadline);
            if let Err(error) = self.writeback_contents() {
                warn!("ext4 content writeback failed: {error:?}");
            }
        }
    }

    fn nodeid_of(inode: &Ext4Inode) -> u64 {
        if inode.is_root() {
            fuse::ROOT_ID
        } else {
            inode.number()
        }
    }

    /// 登记一次 lookup 并回复 entry。
    fn entry(&self, inode: Arc<Ext4Inode>) -> Reply {
        let metadata = inode.metadata().map_err(fs_error)?;
        let nodeid = Self::nodeid_of(&inode);
        self.nodes
            .lock()
            .entry(nodeid)
            .and_modify(|node| node.lookups += 1)
            .or_insert(Node { inode, lookups: 1 });
        let mut output = Output::new();
        output.entry(nodeid, &metadata);
        Ok(Some(output))
    }

    fn open_handle(&self, inode: Arc<Ext4Inode>) -> u64 {
        let handle = self.next_handle.fetch_add(1, Ordering::Relaxed);
        self.handles.lock().insert(handle, inode);
        handle
    }

    /// 新建 inode 的属主与模式：父目录带 set-GID 时继承组（目录同时继承 set-GID 位）。
    fn create_metadata(
        parent: &Ext4Inode,
        header: &Header,
        mode: u32,
        umask: u32,
    ) -> Result<CreateMetadata, i32> {
        let parent_metadata = parent.metadata().map_err(fs_error)?;
        let mut permissions = mode & 0o7777 & !umask;
        let gid = if parent_metadata.mode & S_ISGID != 0 {
            if mode & S_IFMT == InodeType::Directory.mode_bits() {
                permissions |= S_ISGID;
            }
            parent_metadata.gid
        } else {
            header.gid
        };
        Ok(CreateMetadata {
            mode: permissions,
            uid: header.uid,
            gid,
        })
    }

    fn dispatch(&self, header: &Header, input: &mut Input<'_>) -> Reply {
        match header.opcode {
            fuse::INIT => Self::init(input),
            fuse::DESTROY => {
                self.finish().map_err(fs_error)?;
                Ok(None)
            }
            fuse::LOOKUP => {
                let name = input.name()?;
                let child = self
                    .node(header.nodeid)?
                    .find_child(name)
                    .map_err(fs_error)?;
                self.entry(child)
            }
            fuse::FORGET => {
                let count = input.u64()?;
                self.forget(header.nodeid, count);
                Ok(None)
            }
            fuse::BATCH_FORGET => {
                let count = input.u32()?;
                let _ = input.u32()?;
                for _ in 0..count {
                    let nodeid = input.u64()?;
                    let lookups = input.u64()?;
                    self.forget(nodeid, lookups);
                }
                Ok(None)
            }
            fuse::INTERRUPT => Ok(None),
            fuse::GETATTR => {
                let inode = self.node(header.nodeid)?;
                let metadata = inode.metadata().map_err(fs_error)?;
                let mut output = Output::new();
                output.attr_out(header.nodeid, &metadata);
                Ok(Some(output))
            }
            fuse::SETATTR => self.set_attributes(header, input),
            fuse::READLINK => {
                let target = self.node(header.nodeid)?.read_link().map_err(fs_error)?;
                let mut output = Output::new();
                output.bytes(&target);
                Ok(Some(output))
            }
            fuse::SYMLINK => {
                let name = input.name()?;
                let target = input.name()?;
                let parent = self.node(header.nodeid)?;
                let metadata = Self::create_metadata(&parent, header, 0o120777, 0)?;
                let inode = parent.symlink(name, target, metadata).map_err(fs_error)?;
                self.entry(inode)
            }
            fuse::MKNOD => {
                let mode = input.u32()?;
                let rdev = input.u32()?;
                let umask = input.u32()?;
                let _ = input.u32()?;
                let name = input.name()?;
                let parent = self.node(header.nodeid)?;
                let metadata = Self::create_metadata(&parent, header, mode, umask)?;
                let inode = match mode & S_IFMT {
                    0o100000 => parent.create(name, InodeType::File, metadata),
                    0o140000 => parent.create(name, InodeType::Socket, metadata),
                    0o010000 => parent.mknod(name, InodeType::Fifo, metadata, None),
                    0o020000 => parent.mknod(
                        name,
                        InodeType::CharacterDevice,
                        metadata,
                        Some(DeviceNumber::decode(rdev)),
                    ),
                    0o060000 => parent.mknod(
                        name,
                        InodeType::BlockDevice,
                        metadata,
                        Some(DeviceNumber::decode(rdev)),
                    ),
                    _ => return Err(errno::EINVAL as i32),
                }
                .map_err(fs_error)?;
                self.entry(inode)
            }
            fuse::MKDIR => {
                let mode = input.u32()?;
                let umask = input.u32()?;
                let name = input.name()?;
                let parent = self.node(header.nodeid)?;
                let metadata = Self::create_metadata(&parent, header, mode | 0o040000, umask)?;
                let inode = parent
                    .create(name, InodeType::Directory, metadata)
                    .map_err(fs_error)?;
                self.entry(inode)
            }
            fuse::UNLINK | fuse::RMDIR => {
                let name = input.name()?;
                self.node(header.nodeid)?
                    .unlink(name, header.opcode == fuse::RMDIR)
                    .map_err(fs_error)?;
                Ok(None)
            }
            fuse::RENAME | fuse::RENAME2 => {
                let new_parent = input.u64()?;
                let flags = if header.opcode == fuse::RENAME2 {
                    let flags = input.u32()?;
                    let _ = input.u32()?;
                    flags
                } else {
                    0
                };
                if flags & !fuse::RENAME_NOREPLACE != 0 {
                    // RENAME_EXCHANGE / RENAME_WHITEOUT 不受本实现支持。
                    return Err(errno::EINVAL as i32);
                }
                let old_name = input.name()?;
                let new_name = input.name()?;
                let target = self.node(new_parent)?;
                self.node(header.nodeid)?
                    .rename(
                        old_name,
                        target.number(),
                        new_name,
                        flags & fuse::RENAME_NOREPLACE != 0,
                    )
                    .map_err(fs_error)?;
                Ok(None)
            }
            fuse::LINK => {
                let old = input.u64()?;
                let name = input.name()?;
                let target = self.node(old)?;
                self.node(header.nodeid)?
                    .link(name, target.clone())
                    .map_err(fs_error)?;
                self.entry(target)
            }
            fuse::OPEN | fuse::OPENDIR => {
                let inode = self.node(header.nodeid)?;
                let mut output = Output::new();
                output.u64(self.open_handle(inode)).u32(0).u32(0);
                Ok(Some(output))
            }
            fuse::CREATE => {
                let _flags = input.u32()?;
                let mode = input.u32()?;
                let umask = input.u32()?;
                let _ = input.u32()?;
                let name = input.name()?;
                let parent = self.node(header.nodeid)?;
                let metadata = Self::create_metadata(&parent, header, mode | 0o100000, umask)?;
                let inode = parent
                    .create(name, InodeType::File, metadata)
                    .map_err(fs_error)?;
                let handle = self.open_handle(inode.clone());
                let mut output = self.entry(inode)?.expect("entry reply has a body");
                output.u64(handle).u32(0).u32(0);
                Ok(Some(output))
            }
            fuse::READ => {
                let handle = input.u64()?;
                let offset = input.u64()?;
                let size = input.u32()?.min(MAX_TRANSFER) as usize;
                let inode = self.handle(handle)?;
                let mut data = alloc::vec![0u8; size];
                let read = match self.content(&inode).map_err(fs_error)? {
                    Some(content) => content.read(offset, &mut data),
                    None => inode.read_storage(offset, &mut data),
                }
                .map_err(fs_error)?;
                data.truncate(read);
                let _ = inode.touch_access(crate::time::get_realtime_ns() / 1_000_000_000);
                Ok(Some(Output(data)))
            }
            fuse::WRITE => {
                let handle = input.u64()?;
                let offset = input.u64()?;
                let size = input.u32()? as usize;
                let write_flags = input.u32()?;
                let _lock_owner = input.u64()?;
                let _flags = input.u32()?;
                let _ = input.u32()?;
                let data = input.rest();
                let data = data.get(..size).ok_or(errno::EINVAL as i32)?;
                let policy = WritePolicy {
                    kill_suidgid: write_flags & fuse::WRITE_KILL_SUIDGID != 0,
                };
                let inode = self.handle(handle)?;
                let written = match self.content(&inode).map_err(fs_error)? {
                    Some(content) => content.write(offset, data, Some(policy)),
                    None => inode.write_storage(offset, data, Some(policy)),
                }
                .map_err(fs_error)?;
                let mut output = Output::new();
                output.u32(written as u32).u32(0);
                Ok(Some(output))
            }
            fuse::STATFS => {
                let statistics = self.filesystem.statistics().map_err(fs_error)?;
                let mut output = Output::new();
                output
                    .u64(statistics.blocks)
                    .u64(statistics.blocks_free)
                    .u64(statistics.blocks_available)
                    .u64(statistics.files)
                    .u64(statistics.files_free)
                    .u32(statistics.block_size as u32)
                    .u32(statistics.name_length as u32)
                    .u32(statistics.fragment_size as u32)
                    .u32(0)
                    .zeros(24);
                Ok(Some(output))
            }
            fuse::RELEASE | fuse::RELEASEDIR => {
                let handle = input.u64()?;
                let released = self.handles.lock().remove(&handle);
                drop(released);
                Ok(None)
            }
            // FSYNC/FSYNCDIR 与 SYNCFS 都提交整个日志：ext4 的持久化边界是日志事务。
            fuse::FSYNC | fuse::FSYNCDIR | fuse::SYNCFS => {
                let inode = self.node(header.nodeid)?;
                if header.opcode == fuse::SYNCFS {
                    self.writeback_contents().map_err(fs_error)?;
                } else if let Some(content) = self.cached_content(&inode) {
                    content.writeback().map_err(fs_error)?;
                }
                inode.sync_storage().map_err(fs_error)?;
                Ok(None)
            }
            fuse::FLUSH => Ok(None),
            fuse::SETXATTR => {
                let size = input.u32()? as usize;
                let flags = input.u32()?;
                let name = input.name()?;
                let value = input.rest().get(..size).ok_or(errno::EINVAL as i32)?;
                let mut owned = Vec::new();
                owned
                    .try_reserve_exact(value.len())
                    .map_err(|_| errno::ENOMEM as i32)?;
                owned.extend_from_slice(value);
                self.node(header.nodeid)?
                    .change_xattr(XattrChange {
                        name,
                        value: Some(owned),
                        flags,
                    })
                    .map_err(xattr_error)?;
                Ok(None)
            }
            fuse::REMOVEXATTR => {
                let name = input.name()?;
                self.node(header.nodeid)?
                    // 删除不存在的属性是 ENODATA（Linux removexattr）：按 XATTR_REPLACE 要求属性存在。
                    .change_xattr(XattrChange {
                        name,
                        value: None,
                        flags: XATTR_REPLACE,
                    })
                    .map_err(xattr_error)?;
                Ok(None)
            }
            fuse::GETXATTR | fuse::LISTXATTR => {
                let size = input.u32()? as usize;
                let _ = input.u32()?;
                let inode = self.node(header.nodeid)?;
                let value = if header.opcode == fuse::GETXATTR {
                    let name = input.name()?;
                    inode.read_xattr(name).map_err(xattr_error)?
                } else {
                    // 请求头不携带 capability：根用户视为可见 `trusted.*`（Linux 按 CAP_SYS_ADMIN）。
                    inode.list_xattrs(header.uid == 0).map_err(fs_error)?
                };
                let mut output = Output::new();
                if size == 0 {
                    output.u32(value.len() as u32).u32(0);
                } else if value.len() > size {
                    return Err(errno::ERANGE as i32);
                } else {
                    output.bytes(&value);
                }
                Ok(Some(output))
            }
            fuse::READDIR => {
                let handle = input.u64()?;
                let offset = input.u64()?;
                let size = input.u32()?.min(MAX_TRANSFER) as usize;
                let inode = self.handle(handle)?;
                let mut visitor = DirentWriter {
                    output: Output::new(),
                    capacity: size,
                };
                inode
                    .read_directory(offset, &mut visitor)
                    .map_err(fs_error)?;
                Ok(Some(visitor.output))
            }
            fuse::FALLOCATE => {
                let handle = input.u64()?;
                let offset = input.u64()?;
                let length = input.u64()?;
                let mode = input.u32()?;
                if mode != 0 {
                    return Err(errno::EOPNOTSUPP as i32);
                }
                let inode = self.handle(handle)?;
                if let Some(content) = self.content(&inode).map_err(fs_error)? {
                    content.allocate(offset, length).map_err(fs_error)?;
                } else {
                    inode
                        .allocate_storage(offset, length, None)
                        .map_err(fs_error)?;
                }
                Ok(None)
            }
            _ => Err(errno::ENOSYS as i32),
        }
    }

    fn init(input: &mut Input<'_>) -> Reply {
        let major = input.u32()?;
        let minor = input.u32()?;
        let _readahead = input.u32()?;
        let flags = u64::from(input.u32()?);
        let flags2 = if input.bytes.len() >= 4 {
            u64::from(input.u32()?)
        } else {
            0
        };
        let offered = flags | flags2 << 32;
        if major != fuse::KERNEL_VERSION {
            return Err(errno::EPROTO as i32);
        }
        // 不提供 WRITEBACK_CACHE：本服务拥有文件内容（[`Content`]），是大小与时间戳的权威，使用者不缓存写入。
        let wanted = fuse::ASYNC_READ
            | fuse::BIG_WRITES
            | fuse::PARALLEL_DIROPS
            | fuse::ABORT_ERROR
            | fuse::MAX_PAGES;
        let accepted = offered & wanted;
        let mut output = Output::new();
        output
            .u32(fuse::KERNEL_VERSION)
            .u32(minor.min(fuse::KERNEL_MINOR_VERSION))
            .u32(MAX_TRANSFER)
            .u32(accepted as u32)
            .u16(16)
            .u16(12)
            .u32(MAX_TRANSFER)
            .u32(1)
            .u16((MAX_TRANSFER / 4096) as u16)
            .u16(0)
            .u32((accepted >> 32) as u32)
            .u32(0)
            .zeros(24);
        Ok(Some(output))
    }

    fn forget(&self, nodeid: u64, lookups: u64) {
        let removed = {
            let mut nodes = self.nodes.lock();
            let Some(node) = nodes.get_mut(&nodeid) else {
                return;
            };
            node.lookups = node.lookups.saturating_sub(lookups);
            if node.lookups == 0 && nodeid != fuse::ROOT_ID {
                nodes.remove(&nodeid)
            } else {
                None
            }
        };
        // 最后引用可能触发孤儿回收（ext4 I/O），在节点表锁外写回/丢弃内容并释放 inode。
        if let Some(node) = removed {
            let content = self.contents.lock().remove(&node.inode.number());
            if let Some(content) = content {
                // 无链接内容直接丢弃，避免先分配块再回收；未知链接状态仍保守写回。
                let unlinked = node.inode.metadata().is_ok_and(|attrs| attrs.links == 0);
                if !unlinked && let Err(error) = content.writeback() {
                    warn!("ext4 content writeback on forget failed: {error:?}");
                }
            }
        }
    }

    fn set_attributes(&self, header: &Header, input: &mut Input<'_>) -> Reply {
        let valid = input.u32()?;
        let _ = input.u32()?;
        let _fh = input.u64()?;
        let size = input.u64()?;
        let _lock_owner = input.u64()?;
        let atime = input.u64()?;
        let mtime = input.u64()?;
        let _ctime = input.u64()?;
        let _ = input.take(12)?;
        let mode = input.u32()?;
        let _ = input.u32()?;
        let uid = input.u32()?;
        let gid = input.u32()?;
        let inode = self.node(header.nodeid)?;
        let kill_suidgid = valid & FATTR_KILL_SUIDGID != 0;
        if valid & fuse::FATTR_SIZE != 0 {
            let policy = WritePolicy { kill_suidgid };
            if let Some(content) = self.content(&inode).map_err(fs_error)? {
                content.truncate(size, policy).map_err(fs_error)?;
            } else {
                inode
                    .truncate_storage(size, Some(policy))
                    .map_err(fs_error)?;
            }
        }
        if valid & (fuse::FATTR_MODE | fuse::FATTR_UID | fuse::FATTR_GID) != 0 || kill_suidgid {
            inode
                .change_owner_mode(OwnerModeChange {
                    mode: (valid & fuse::FATTR_MODE != 0).then_some(mode),
                    uid: (valid & fuse::FATTR_UID != 0).then_some(uid),
                    gid: (valid & fuse::FATTR_GID != 0).then_some(gid),
                    kill_suidgid,
                })
                .map_err(fs_error)?;
        }
        let now = crate::time::get_realtime_ns() / 1_000_000_000;
        let atime = (valid & fuse::FATTR_ATIME != 0).then_some(if valid & FATTR_ATIME_NOW != 0 {
            now
        } else {
            atime
        });
        let mtime = (valid & fuse::FATTR_MTIME != 0).then_some(if valid & FATTR_MTIME_NOW != 0 {
            now
        } else {
            mtime
        });
        if atime.is_some() || mtime.is_some() {
            inode.set_times(atime, mtime).map_err(fs_error)?;
        }
        let metadata = inode.metadata().map_err(fs_error)?;
        let mut output = Output::new();
        output.attr_out(header.nodeid, &metadata);
        Ok(Some(output))
    }
}

/// 按 `fuse_dirent` 编码目录项，直到用尽回复容量。
struct DirentWriter {
    output: Output,
    capacity: usize,
}

impl DirectoryVisitor for DirentWriter {
    fn visit(
        &mut self,
        next_cursor: u64,
        entry: DirectoryEntry<'_>,
    ) -> Result<DirectoryVisit, FileSystemError> {
        let record = (fuse::DIRENT_NAME_OFFSET + entry.name.len()).next_multiple_of(8);
        if self.output.0.len() + record > self.capacity {
            return Ok(DirectoryVisit::Stop);
        }
        self.output
            .u64(entry.inode)
            .u64(next_cursor)
            .u32(entry.name.len() as u32)
            .u32(entry.kind.directory_type())
            .bytes(entry.name)
            .zeros(record - fuse::DIRENT_NAME_OFFSET - entry.name.len());
        Ok(DirectoryVisit::Continue)
    }
}
