//! `fuse` 文件系统类型、挂载选项、superblock 与守护进程通知（Linux `fs/fuse/inode.c`）。
use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};

use core::sync::atomic::{AtomicU16, Ordering};

use spin::Mutex;
use syscall_abi::{errno, fuse};

use super::{
    connection::{Connection, ReplyShape},
    device::FuseDevice,
    inode::FuseInode,
    options::Options,
    protocol::{self, Attributes, Fields, Reader},
};
use crate::{
    fallible_tree::FallibleMap,
    fs::{
        FileSystem, FileSystemError, FileSystemStatistics, Inode, SuperblockFacts,
        mount::{CreatedFileSystem, FileSystemType, MountRequest},
    },
};

/// Linux `FUSE_SUPER_MAGIC`。
const FUSE_SUPER_MAGIC: u64 = 0x6573_5546;

/// 一个 FUSE 挂载的共享状态；inode 与连接经它互相找到。
pub(super) struct Mount {
    pub(super) connection: Arc<Connection>,
    pub(super) filesystem_id: usize,
    pub(super) options: Options,
    // OWNER: nodeid → 存活 inode 的唯一索引（Linux `iget5_locked` 按 nodeid 查找）。查找回复据此复用
    // 同一 inode 并累加 nlookup；缺失时同一文件出现多个 inode，各自的属性与页缓存互不一致。
    nodes: Mutex<FallibleMap<u64, Weak<FuseInode>>>,
    // OWNER: 守护进程以 ENOSYS 声明不实现的可选操作（Linux `no_access`/`no_rename2`/`no_fsync`/
    // `no_fsyncdir`/`no_fallocate`/`no_flush`/`no_create`/xattr），按 [`Unsupported`] 位记录。缺失时每次 access/fsync 都多一个
    // 注定失败的往返，RENAME_NOREPLACE 在不支持的服务器上也得不到 Linux 的 EINVAL。
    unsupported: AtomicU16,
}

/// 守护进程不实现的可选操作。
#[derive(Clone, Copy)]
pub(super) enum Unsupported {
    Access = 1 << 0,
    Rename2 = 1 << 1,
    Fsync = 1 << 2,
    FsyncDirectory = 1 << 3,
    Fallocate = 1 << 4,
    Flush = 1 << 5,
    Create = 1 << 6,
    GetXattr = 1 << 7,
    SetXattr = 1 << 8,
    ListXattr = 1 << 9,
    RemoveXattr = 1 << 10,
}

impl Mount {
    pub(super) fn unsupported(&self, operation: Unsupported) -> bool {
        self.unsupported.load(Ordering::Relaxed) & operation as u16 != 0
    }

    pub(super) fn mark_unsupported(&self, operation: Unsupported) {
        self.unsupported
            .fetch_or(operation as u16, Ordering::Relaxed);
    }

    /// nodeid 对应的存活 inode。
    pub(super) fn node(&self, nodeid: u64) -> Option<Arc<FuseInode>> {
        self.nodes.lock().get(&nodeid).and_then(Weak::upgrade)
    }

    /// 消费一次成功查找（LOOKUP/CREATE/MKDIR/…）的回复。调用者在发请求前已
    /// [`Connection::reserve_forget`]，本函数恰好消费这个预留一次：
    /// 1. 回复无效：释放预留，不发 FORGET（Linux 对无效属性同样直接丢弃）；
    /// 2. 已有同 generation、同类型的存活 inode：查找计数加一并更新属性；
    /// 3. 否则新建 inode 接管预留；新建失败时立即 FORGET 这一次查找，守护进程的计数保持一致。
    pub(super) fn adopt(
        self: &Arc<Self>,
        entry: &protocol::Entry,
    ) -> Result<Arc<FuseInode>, FileSystemError> {
        let Some(kind) = entry.attributes.kind().filter(|_| entry.nodeid != 0) else {
            self.connection.forget(0, 0);
            return Err(if entry.nodeid == 0 {
                FileSystemError::NotFound
            } else {
                FileSystemError::IoError
            });
        };
        if let Some(existing) = self.node(entry.nodeid)
            && existing.matches(entry.generation, kind)
        {
            existing.add_lookup();
            existing.store_attributes(&entry.attributes, entry.attr_valid);
            self.connection.forget(0, 0);
            return Ok(existing);
        }
        let slot = FallibleMap::<u64, Weak<FuseInode>>::try_reserve_node();
        let inode = slot.ok().and_then(|slot| {
            let inode = FuseInode::new(self.clone(), entry, kind).ok()?;
            let mut nodes = self.nodes.lock();
            let retired = nodes.take_entry(&entry.nodeid);
            nodes.commit_vacant(slot.fill(entry.nodeid, Arc::downgrade(&inode)));
            drop(nodes);
            drop(retired);
            Some(inode)
        });
        inode.ok_or_else(|| {
            self.connection.forget(entry.nodeid, 1);
            FileSystemError::OutOfMemory
        })
    }

    /// inode 最后释放时摘除仍指向它的索引项。
    pub(super) fn retire(&self, nodeid: u64, inode: *const FuseInode) {
        let removed = {
            let mut nodes = self.nodes.lock();
            if nodes
                .get(&nodeid)
                .is_some_and(|entry| core::ptr::eq(entry.as_ptr(), inode))
            {
                nodes.remove(&nodeid)
            } else {
                None
            }
        };
        drop(removed);
    }
}

/// 一个已挂载的 FUSE superblock。
struct FuseFileSystem {
    mount: Arc<Mount>,
    root: Arc<FuseInode>,
}

impl FileSystem for FuseFileSystem {
    fn superblock(&self) -> SuperblockFacts {
        SuperblockFacts {
            type_name: "fuse",
            read_only: false,
        }
    }

    fn subtype(&self) -> &[u8] {
        &self.mount.options.subtype
    }

    /// Linux `fuse_show_options` 的旧式选项。
    fn show_mount_options(&self) -> Result<Vec<u8>, FileSystemError> {
        let options = &self.mount.options;
        crate::fs::try_format_bytes(format_args!(
            ",user_id={},group_id={}{}{}",
            options.user_id,
            options.group_id,
            if options.default_permissions {
                ",default_permissions"
            } else {
                ""
            },
            if options.allow_other {
                ",allow_other"
            } else {
                ""
            },
        ))
        .and_then(|mut bytes| {
            if let Some(max_read) = options.max_read {
                let tail = crate::fs::try_format_bytes(format_args!(",max_read={max_read}"))?;
                bytes
                    .try_reserve(tail.len())
                    .map_err(|_| FileSystemError::OutOfMemory)?;
                bytes.extend_from_slice(&tail);
            }
            Ok(bytes)
        })
    }

    fn root_inode(&self) -> Result<Arc<dyn Inode>, FileSystemError> {
        Ok(self.root.clone())
    }

    /// FUSE_STATFS；不允许访问挂载的调用者只得到类型（Linux `fuse_statfs`）。
    fn statistics(&self) -> Result<FileSystemStatistics, FileSystemError> {
        let mut statistics = FileSystemStatistics {
            magic: FUSE_SUPER_MAGIC,
            block_size: 0,
            blocks: 0,
            blocks_free: 0,
            blocks_available: 0,
            files: 0,
            files_free: 0,
            fsid: [0, 0],
            name_length: 0,
            fragment_size: 0,
            flags: 0,
        };
        let options = &self.mount.options;
        if !options.allow_other
            && !self
                .mount
                .connection
                .current_allowed(options.user_id, options.group_id)
        {
            return Ok(statistics);
        }
        let reply = self.mount.connection.call(
            fuse::STATFS,
            fuse::ROOT_ID,
            &[],
            ReplyShape::fixed(fuse::STATFS_OUT_SIZE),
        )?;
        let reply = protocol::statfs_out(&reply)?;
        statistics.block_size = u64::from(reply.block_size);
        statistics.blocks = reply.blocks;
        statistics.blocks_free = reply.blocks_free;
        statistics.blocks_available = reply.blocks_available;
        statistics.files = reply.files;
        statistics.files_free = reply.files_free;
        statistics.name_length = u64::from(reply.name_length);
        statistics.fragment_size = u64::from(reply.fragment_size);
        Ok(statistics)
    }

    /// 卸载（Linux `fuse_sb_destroy` → `fuse_abort_conn`）：守护进程 read 返回 `ENODEV` 后退出。
    fn shutdown(&self) -> Result<(), FileSystemError> {
        self.mount.connection.abort(false);
        self.root.clear_entries();
        Ok(())
    }
}

/// `fuse` 文件系统类型。
pub(super) struct FuseFileSystemType;

impl FileSystemType for FuseFileSystemType {
    fn name(&self) -> &'static str {
        "fuse"
    }

    fn has_subtype(&self) -> bool {
        true
    }

    fn validate_mount_options(&self, options: &[u8]) -> Result<(), FileSystemError> {
        Options::parse(options).map(drop)
    }

    /// Linux `fuse_fill_super`：`fd=` 必须是未绑定的 `/dev/fuse` 端点；根 inode 用 `rootmode`
    /// 合成属性，INIT 异步完成，挂载不等守护进程回复。
    fn create(&self, request: &MountRequest<'_>) -> Result<CreatedFileSystem, FileSystemError> {
        let options = Options::parse(request.options)?;
        let file = request.files.device_file(options.fd)?;
        let device =
            FuseDevice::from_file(file.as_ref()).ok_or(FileSystemError::InvalidOperation)?;
        let requester =
            (request.environment.requester)().ok_or(FileSystemError::InvalidOperation)?;
        let connection = Connection::new(
            request.environment.requester,
            requester.thread.namespace(),
            options.max_read.unwrap_or(u32::MAX),
        )?;
        let root_attributes =
            Attributes::root(options.root_mode, options.user_id, options.group_id);
        let mount = Arc::try_new(Mount {
            connection: connection.clone(),
            filesystem_id: crate::fs::allocate_filesystem_id(),
            options,
            nodes: Mutex::new(FallibleMap::new()),
            unsupported: AtomicU16::new(0),
        })
        .map_err(|_| FileSystemError::OutOfMemory)?;
        let root = FuseInode::root(mount.clone(), &root_attributes)?;
        mount
            .nodes
            .lock()
            .try_insert(fuse::ROOT_ID, Arc::downgrade(&root))
            .map_err(|_| FileSystemError::OutOfMemory)?;
        device.bind(&connection)?;
        connection.attach_mount(&mount);
        let filesystem = Arc::try_new(FuseFileSystem { mount, root })
            .map_err(|_| FileSystemError::OutOfMemory)?;
        Ok(CreatedFileSystem {
            filesystem,
            user_namespace: request.identity.user_namespace(),
        })
    }
}

/// 守护进程的 unique 为零的通知（Linux `fuse_notify`）。
///
/// # Errors
///
/// 负载格式错误或通知码未知返回 `InvalidOperation`（`EINVAL`）；挂载已卸载或 inode 不在缓存中
/// 返回 `NotFound`（`ENOENT`）；本实现不提供的 STORE/RETRIEVE/POLL/RESEND/INC_EPOCH/PRUNE 返回 `ENOSYS`。
pub(super) fn notify(
    connection: &Connection,
    code: i32,
    payload: &[u8],
) -> Result<(), FileSystemError> {
    let mount = connection.mount().ok_or(FileSystemError::NotFound)?;
    let mut reader = Reader::new(payload);
    let invalid = |_| FileSystemError::InvalidOperation;
    match code {
        fuse::NOTIFY_INVAL_INODE => {
            let nodeid = reader.u64().map_err(invalid)?;
            let offset = reader.u64().map_err(invalid)? as i64;
            let _length = reader.u64().map_err(invalid)?;
            let inode = mount.node(nodeid).ok_or(FileSystemError::NotFound)?;
            inode.invalidate_attributes();
            // 负偏移只作废属性；否则作废数据。页缓存按整个文件作废，不按 len 区间。
            if offset >= 0 {
                inode.invalidate_data(true)?;
            }
            Ok(())
        }
        fuse::NOTIFY_INVAL_ENTRY | fuse::NOTIFY_DELETE => {
            let parent = reader.u64().map_err(invalid)?;
            let child = if code == fuse::NOTIFY_DELETE {
                Some(reader.u64().map_err(invalid)?)
            } else {
                None
            };
            let length = reader.u32().map_err(invalid)? as usize;
            reader.skip(4).map_err(invalid)?;
            let name = reader.bytes(length).map_err(invalid)?;
            if reader.bytes(1).map_err(invalid)? != [0] {
                return Err(FileSystemError::InvalidOperation);
            }
            let directory = mount.node(parent).ok_or(FileSystemError::NotFound)?;
            directory.invalidate_entry(name, child);
            Ok(())
        }
        1 | 4 | 5 | 7..=9 => Err(FileSystemError::Errno(errno::ENOSYS as u16)),
        _ => Err(FileSystemError::InvalidOperation),
    }
}

/// `fuse_setattr_in`：`valid` 位与各字段按 Linux 布局编码。
pub(super) struct SetAttributes {
    pub(super) valid: u32,
    pub(super) handle: u64,
    pub(super) size: u64,
    pub(super) atime: u64,
    pub(super) mtime: u64,
    pub(super) mode: u32,
    pub(super) uid: u32,
    pub(super) gid: u32,
}

impl SetAttributes {
    pub(super) const fn new(valid: u32) -> Self {
        Self {
            valid,
            handle: 0,
            size: 0,
            atime: 0,
            mtime: 0,
            mode: 0,
            uid: 0,
            gid: 0,
        }
    }

    pub(super) fn encode(&self) -> [u8; 88] {
        Fields::<88>::new()
            .u32(self.valid)
            .u32(0)
            .u64(self.handle)
            .u64(self.size)
            .u64(0)
            .u64(self.atime)
            .u64(self.mtime)
            .u64(0)
            .u32(0)
            .u32(0)
            .u32(0)
            .u32(self.mode)
            .u32(0)
            .u32(self.uid)
            .u32(self.gid)
            .finish()
    }
}
