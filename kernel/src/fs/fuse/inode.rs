//! FUSE inode：把 VFS inode 操作翻译成请求（Linux `fs/fuse/dir.c`、`file.c`、`readdir.c`）。
//!
//! VFS 每次路径解析都调用 `find_child`，没有 dcache；本模块按守护进程给出的 entry/attr 超时缓存
//! 查找结果与属性，承担 Linux dentry 与 `fuse_inode` 属性缓存的职责。
use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use spin::Mutex;
use syscall_abi::{
    errno, fuse,
    open::{O_CREAT, O_EXCL, O_NOCTTY, O_TRUNC},
};

use super::{
    connection::{Negotiated, ReplyShape},
    filesystem::{Mount, SetAttributes, Unsupported},
    protocol::{self, Attributes, Entry, Fields},
};
use crate::{
    fallible_tree::FallibleMap,
    fs::{
        AccessIdentity, CreateMetadata, DeviceNumber, DirectoryRead, DirectoryVisitor,
        FileSystemError, FineTimes, Inode, InodeMetadata, InodeOpen, InodeOpenError, InodeType,
        InodeWriteAccess, MAY_ACCESS, MAY_CHDIR, OwnerModeChange, inode::open_by_type,
        permission::OwnerModeState,
    },
    memory::SharedFileId,
    sync::{TaskMutex, TaskMutexGuard},
};

mod directory;
mod handle;
mod io;
mod xattr;
use directory::CachedEntry;
use handle::HandleRef;

/// READLINK 回复上限（Linux 读一页，满页视为 `ENAMETOOLONG`）。
const LINK_BUFFER: usize = 4096;

fn now() -> u64 {
    crate::timer::get_time_ns()
}

struct CachedAttributes {
    attributes: Attributes,
    valid_until: u64,
}

/// 一个 FUSE inode（Linux `fuse_inode`）。
pub(super) struct FuseInode {
    mount: Arc<Mount>,
    nodeid: u64,
    generation: u64,
    kind: InodeType,
    // OWNER: 守护进程为本 inode 计的查找次数（Linux `nlookup`）；最后释放时整体 FORGET。
    // 缺失时守护进程永远不知道 inode 已不再被引用，服务器端节点只增不减。
    lookups: AtomicU64,
    /// 持有连接上的一个 FORGET 预留；根 inode 没有查找计数，不持有。
    owns_forget: bool,
    // CACHE: 属性与其有效期（Linux `i_*` + `fi->i_time`）；过期后 metadata 重新 GETATTR。
    // 缺失时每次 stat/权限检查都要往返守护进程。
    attributes: Mutex<CachedAttributes>,
    // CACHE: 目录项名字 → inode 与 entry 有效期（Linux dentry + `d_time`）；本地变更与守护进程
    // 失效通知立即摘除对应项。缺失时每个路径分量每次解析都要一次 LOOKUP 往返。
    entries: Mutex<FallibleMap<Vec<u8>, CachedEntry>>,
    // OWNER: 打开本 inode 的服务器句柄（Linux `fi->write_files` 等）；页缓存读写与 fsync 没有 file
    // 上下文，从这里取任一可用句柄。只存 Weak，句柄寿命归 OFD。
    handles: Mutex<Vec<handle::RegisteredHandle>>,
    // OWNER: 守护进程未声明 FUSE_PARALLEL_DIROPS 时串行化本目录的查找、读目录与变更（Linux 对
    // 目录 i_rwsem 取排他）。缺失时不支持并发目录操作的守护进程会看到交错请求。
    directory_operations: TaskMutex<()>,
    // POLICY: 本目录发生过子项 LOOKUP（Linux FUSE_I_ADVISE_RDPLUS）；READDIRPLUS_AUTO 下据此决定
    // 下一次读目录是否带属性。缺失时 `ls` 只在第一批用 PLUS，`ls -l` 的后续批次逐项 LOOKUP。
    advise_plus: AtomicBool,
    // POLICY: 页缓存内容可能已落后于服务器（属性刷新发现 size/mtime 变化、失效通知遇到忙锁）；
    // 读入口与打开时重试作废。缺失时作废一旦因锁忙跳过，旧内容就一直留在缓存里。
    stale_data: AtomicBool,
    // OWNER: 正在释放的可写句柄写出剩余脏页时的临时句柄；只在 [`FuseInode::drain_writes`] 期间存在。
    // 缺失时最后一个句柄 Drop 中的写回找不到句柄（Weak 已不可升级）而以 EBADF 丢弃数据。
    draining: Mutex<Option<HandleRef>>,
    write_access: InodeWriteAccess,
}

impl FuseInode {
    fn build(
        mount: Arc<Mount>,
        nodeid: u64,
        generation: u64,
        kind: InodeType,
        cached: CachedAttributes,
        lookups: u64,
    ) -> Result<Arc<Self>, FileSystemError> {
        Arc::try_new(Self {
            mount,
            nodeid,
            generation,
            kind,
            lookups: AtomicU64::new(lookups),
            owns_forget: lookups != 0,
            attributes: Mutex::new(cached),
            entries: Mutex::new(FallibleMap::new()),
            handles: Mutex::new(Vec::new()),
            directory_operations: TaskMutex::new(()),
            advise_plus: AtomicBool::new(false),
            stale_data: AtomicBool::new(false),
            draining: Mutex::new(None),
            write_access: InodeWriteAccess::new(),
        })
        .map_err(|_| FileSystemError::OutOfMemory)
    }

    /// 由一次查找回复新建的 inode，接管调用者在连接上预留的 FORGET。
    pub(super) fn new(
        mount: Arc<Mount>,
        entry: &Entry,
        kind: InodeType,
    ) -> Result<Arc<Self>, FileSystemError> {
        let valid_until = protocol::deadline(now(), entry.attr_valid.0, entry.attr_valid.1);
        Self::build(
            mount,
            entry.nodeid,
            entry.generation,
            kind,
            CachedAttributes {
                attributes: entry.attributes,
                valid_until,
            },
            1,
        )
    }

    /// 挂载根：合成属性立即过期，第一次 stat 向守护进程取真实属性。
    pub(super) fn root(
        mount: Arc<Mount>,
        attributes: &Attributes,
    ) -> Result<Arc<Self>, FileSystemError> {
        let kind = attributes.kind().ok_or(FileSystemError::InvalidOperation)?;
        Self::build(
            mount,
            fuse::ROOT_ID,
            0,
            kind,
            CachedAttributes {
                attributes: *attributes,
                valid_until: 0,
            },
            0,
        )
    }

    pub(super) fn matches(&self, generation: u64, kind: InodeType) -> bool {
        self.generation == generation && self.kind == kind
    }

    pub(super) fn add_lookup(&self) {
        self.lookups.fetch_add(1, Ordering::Relaxed);
    }

    /// 记录守护进程给出的属性（Linux `fuse_change_attributes`）：普通文件的大小变化、或协商
    /// AUTO_INVAL_DATA 时的 mtime 变化说明内容已被别处改动，标记数据过期并尝试作废页缓存。
    pub(super) fn store_attributes(&self, attributes: &Attributes, valid: (u64, u32)) {
        let buffered = self.buffered_writes();
        let previous = {
            let mut cached = self.attributes.lock();
            let previous = cached.attributes;
            let mut next = *attributes;
            if buffered {
                // WRITEBACK_CACHE：大小与时间以内核为准（Linux `cache_mask`），服务器尚未看到缓冲的脏数据。
                (
                    next.size,
                    next.mtime,
                    next.mtime_nsec,
                    next.ctime,
                    next.ctime_nsec,
                ) = (
                    previous.size,
                    previous.mtime,
                    previous.mtime_nsec,
                    previous.ctime,
                    previous.ctime_nsec,
                );
            }
            *cached = CachedAttributes {
                attributes: next,
                valid_until: protocol::deadline(now(), valid.0, valid.1),
            };
            previous
        };
        if buffered {
            return;
        }
        let auto_invalidate = self
            .negotiated()
            .is_ok_and(|negotiated| negotiated.has(fuse::AUTO_INVAL_DATA));
        if self.kind == InodeType::File
            && (previous.size != attributes.size
                || auto_invalidate
                    && (previous.mtime, previous.mtime_nsec)
                        != (attributes.mtime, attributes.mtime_nsec))
        {
            self.stale_data.store(true, Ordering::Release);
            // 调用者可能正持本文件的写序列（write 后的属性刷新）；只在锁空闲时作废，否则留给下次。
            let _ = self.invalidate_data(false);
        }
    }

    pub(super) fn invalidate_attributes(&self) {
        self.attributes.lock().valid_until = 0;
    }

    /// 作废页缓存中只被缓存持有的页（Linux `invalidate_inode_pages2`），成功后清除过期标记。
    ///
    /// `wait` 见 [`crate::fs::page_cache::invalidate`]；不等待且页缓存忙时保留标记并返回成功。
    pub(super) fn invalidate_data(&self, wait: bool) -> Result<(), FileSystemError> {
        if self.kind != InodeType::File {
            return Ok(());
        }
        self.stale_data.store(true, Ordering::Release);
        match crate::fs::page_cache::invalidate(
            SharedFileId {
                filesystem: self.mount.filesystem_id,
                inode: self.nodeid,
            },
            wait,
        ) {
            Ok(()) => {
                self.stale_data.store(false, Ordering::Release);
                Ok(())
            }
            Err(FileSystemError::Busy) if !wait => Ok(()),
            Err(error) => Err(error),
        }
    }

    fn cached(&self) -> Attributes {
        self.attributes.lock().attributes
    }

    /// 有效期内的缓存属性，否则 GETATTR（Linux `fuse_update_attributes`）。
    fn attributes(&self) -> Result<Attributes, FileSystemError> {
        {
            let cached = self.attributes.lock();
            if now() < cached.valid_until {
                return Ok(cached.attributes);
            }
        }
        let reply = self.call(
            fuse::GETATTR,
            &[&[0u8; 16]],
            ReplyShape::fixed(fuse::ATTR_OUT_SIZE),
        )?;
        self.store_attr_out(&reply)
    }

    fn store_attr_out(&self, reply: &[u8]) -> Result<Attributes, FileSystemError> {
        let (valid, attributes) = protocol::attr_out(reply)?;
        // Linux fuse_stale_inode：类型改变的 nodeid 已不是这个 inode。
        if attributes.kind() != Some(self.kind) {
            return Err(FileSystemError::IoError);
        }
        self.store_attributes(&attributes, valid);
        Ok(attributes)
    }

    fn call(
        &self,
        opcode: u32,
        arguments: &[&[u8]],
        reply: ReplyShape,
    ) -> Result<Vec<u8>, FileSystemError> {
        self.mount
            .connection
            .call(opcode, self.nodeid, arguments, reply)
    }

    fn negotiated(&self) -> Result<Negotiated, FileSystemError> {
        self.mount
            .connection
            .negotiated()
            .copied()
            .ok_or(FileSystemError::Errno(errno::ENOTCONN as u16))
    }

    /// 守护进程未声明 FUSE_PARALLEL_DIROPS 时串行化目录操作。
    fn serialize(&self) -> Result<Option<TaskMutexGuard<'_, ()>>, FileSystemError> {
        if self
            .negotiated()
            .is_ok_and(|negotiated| negotiated.has(fuse::PARALLEL_DIROPS))
        {
            return Ok(None);
        }
        self.directory_operations
            .lock()
            .map(Some)
            .map_err(|_| FileSystemError::OutOfMemory)
    }

    /// 发给服务器的打开 flags：WRITEBACK_CACHE 下只写打开升级为读写（Linux `fuse_open`），缓冲写
    /// 填充部分页需要从同一句柄读回原内容。
    ///
    /// 只用于普通文件的 OPEN 与目录上的 CREATE（新建的总是普通文件），因此按协商结果而非本 inode 类型判断。
    fn server_open_flags(&self, flags: u32) -> u32 {
        let writeback = self
            .negotiated()
            .is_ok_and(|negotiated| negotiated.has(fuse::WRITEBACK_CACHE));
        if writeback && flags & crate::fs::O_ACCMODE == crate::fs::O_WRONLY {
            flags & !crate::fs::O_ACCMODE | crate::fs::O_RDWR
        } else {
            flags
        }
    }

    /// Linux `fuse_allow_current_process`：未声明 `allow_other` 时只允许挂载用户本人。
    fn allowed(&self, identity: &AccessIdentity) -> bool {
        let options = &self.mount.options;
        options.allow_other
            || identity.uid() == options.user_id && identity.gid() == options.group_id
    }

    fn set_attributes(&self, set: SetAttributes) -> Result<(), FileSystemError> {
        let reply = self.call(
            fuse::SETATTR,
            &[&set.encode()],
            ReplyShape::fixed(fuse::ATTR_OUT_SIZE),
        )?;
        self.store_attr_out(&reply).map(drop)
    }

    /// FSYNC/FSYNCDIR；守护进程不实现时视为成功（Linux `no_fsync`）。
    fn fsync(&self, opcode: u32, unsupported: Unsupported) -> Result<(), FileSystemError> {
        if self.mount.unsupported(unsupported) {
            return Ok(());
        }
        let Ok(handle) = self.handle(opcode == fuse::FSYNC) else {
            return Ok(());
        };
        let body = Fields::<16>::new().u64(handle.reference.handle).finish();
        match self.call(opcode, &[&body], ReplyShape::EMPTY) {
            Err(FileSystemError::Errno(code)) if code as isize == errno::ENOSYS => {
                self.mount.mark_unsupported(unsupported);
                Ok(())
            }
            result => result.map(drop),
        }
    }
}

impl Drop for FuseInode {
    fn drop(&mut self) {
        if self.owns_forget {
            self.mount
                .connection
                .forget(self.nodeid, *self.lookups.get_mut());
        }
        self.mount.retire(self.nodeid, self);
    }
}

/// `fuse_read_in` 与 `fuse_write_in` 共用的布局。
fn io_in(handle: HandleRef, offset: u64, size: u32, io_flags: u32) -> [u8; 40] {
    Fields::<40>::new()
        .u64(handle.handle)
        .u64(offset)
        .u32(size)
        .u32(io_flags)
        .u64(0)
        .u32(handle.flags)
        .finish()
}

impl Inode for FuseInode {
    fn read_xattr(&self, name: &[u8]) -> Result<Vec<u8>, FileSystemError> {
        self.remote_xattr(Some(name))
    }

    fn xattr_size(&self, name: &[u8]) -> Result<usize, FileSystemError> {
        self.remote_xattr_size(Some(name))
    }

    fn list_xattrs(&self, identity: &AccessIdentity) -> Result<Vec<u8>, FileSystemError> {
        self.require_access(identity, 0)?;
        self.remote_xattr(None)
    }

    fn xattr_list_size(&self, identity: &AccessIdentity) -> Result<usize, FileSystemError> {
        self.require_access(identity, 0)?;
        self.remote_xattr_size(None)
    }

    fn change_xattr(&self, change: crate::fs::XattrChange<'_>) -> Result<(), FileSystemError> {
        self.change_remote_xattr(change)
    }

    fn write_access(&self) -> Option<&InodeWriteAccess> {
        (self.kind == InodeType::File).then_some(&self.write_access)
    }

    fn filesystem_id(&self) -> usize {
        self.mount.filesystem_id
    }

    fn identity(&self) -> Result<(u64, u64), FileSystemError> {
        Ok((self.mount.filesystem_id as u64, self.nodeid))
    }

    /// `st_ino` 取 nodeid：VFS 以 inode 号区分同一 filesystem 内的 inode（rename 目标目录、
    /// dentry 索引），而守护进程的 `fuse_attr.ino` 不保证唯一。
    fn metadata(&self) -> Result<InodeMetadata, FileSystemError> {
        let attributes = self.attributes()?;
        Ok(InodeMetadata {
            filesystem: self.mount.filesystem_id as u64,
            inode: self.nodeid,
            kind: self.kind,
            mode: attributes.mode,
            links: attributes.links,
            uid: attributes.uid,
            gid: attributes.gid,
            size: attributes.size,
            blocks: attributes.blocks,
            block_size: if attributes.block_size == 0 {
                crate::memory::PAGE_SIZE as u32
            } else {
                attributes.block_size
            },
            atime: attributes.atime,
            mtime: attributes.mtime,
            ctime: attributes.ctime,
            fine_times: FineTimes {
                atime_nsec: attributes.atime_nsec,
                mtime_nsec: attributes.mtime_nsec,
                ctime_nsec: attributes.ctime_nsec,
                birth: None,
            },
            device: self.device_number(),
        })
    }

    /// Linux `fuse_permission`：
    /// 1. 非挂载用户且未 `allow_other` 一律 `EACCES`；
    /// 2. `default_permissions` 时按属性做内核 DAC；
    /// 3. 否则 access(2)/chdir 问守护进程（FUSE_ACCESS），执行普通文件要求有执行位，其余交给服务器。
    fn require_access(
        &self,
        identity: &AccessIdentity,
        requested: u8,
    ) -> Result<(), FileSystemError> {
        if !self.allowed(identity) {
            return Err(FileSystemError::AccessDenied);
        }
        if self.mount.options.default_permissions {
            return identity.require(self.metadata()?, requested);
        }
        if requested & (MAY_ACCESS | MAY_CHDIR) != 0 {
            if self.mount.unsupported(Unsupported::Access) {
                return Ok(());
            }
            let body = Fields::<8>::new().u32(u32::from(requested & 7)).finish();
            return match self.call(fuse::ACCESS, &[&body], ReplyShape::EMPTY) {
                Err(FileSystemError::Errno(code)) if code as isize == errno::ENOSYS => {
                    self.mount.mark_unsupported(Unsupported::Access);
                    Ok(())
                }
                result => result.map(drop),
            };
        }
        if requested & 1 != 0
            && self.kind == InodeType::File
            && self.attributes()?.mode & 0o111 == 0
        {
            return Err(FileSystemError::AccessDenied);
        }
        Ok(())
    }

    fn inode_type(&self) -> InodeType {
        self.kind
    }

    fn size(&self) -> u64 {
        self.cached().size
    }

    fn is_executable(&self) -> bool {
        self.cached().mode & 0o111 != 0
    }

    /// 普通文件 OPEN、目录 OPENDIR，句柄随 OFD 存活（Linux `fuse_open_common`）；未带
    /// FOPEN_KEEP_CACHE 的打开作废页缓存，保证打开时看到其他客户端的修改。其余类型按通用规则打开。
    fn open_backend(
        &self,
        flags: u32,
        identity: &AccessIdentity,
        _authority: &crate::namespace::NamespaceAuthority,
    ) -> Result<InodeOpen, InodeOpenError> {
        let (open, release) = match self.kind {
            InodeType::File => (fuse::OPEN, fuse::RELEASE),
            InodeType::Directory => (fuse::OPENDIR, fuse::RELEASEDIR),
            _ => return open_by_type(self, flags, identity),
        };
        // Linux：O_CREAT/O_EXCL/O_NOCTTY 已由 VFS 处理；未协商 ATOMIC_O_TRUNC 时截断另发 SETATTR。
        let flags = flags & !(O_CREAT | O_EXCL | O_NOCTTY | O_TRUNC);
        let flags = if self.kind == InodeType::File {
            self.server_open_flags(flags)
        } else {
            flags
        };
        let release_in = Fields::<24>::new().u64(0).u32(flags).finish();
        let release = self
            .mount
            .connection
            .reserve(release, self.nodeid, &[&release_in])?;
        let body = Fields::<8>::new().u32(flags).finish();
        let reply = self.call(open, &[&body], ReplyShape::fixed(fuse::OPEN_OUT_SIZE))?;
        let opened = protocol::open_out(&reply)?;
        let handle = self.attach_handle(&opened, flags, release)?;
        if self.kind == InodeType::File
            && (opened.flags & fuse::FOPEN_KEEP_CACHE == 0
                || self.stale_data.load(Ordering::Acquire))
        {
            self.invalidate_data(true)?;
        }
        Ok(InodeOpen::Storage(Some(handle)))
    }

    fn notification_link_state(&self) -> Option<(u64, u32)> {
        Some((self.nodeid, self.cached().links))
    }

    fn device_number(&self) -> Option<DeviceNumber> {
        matches!(
            self.kind,
            InodeType::CharacterDevice | InodeType::BlockDevice
        )
        .then(|| DeviceNumber::decode(u64::from(self.cached().rdev)))
    }

    /// AUTO_INVAL_DATA 时先刷新过期属性（可能标记过期），再处理尚未完成的作废（Linux
    /// `fuse_file_read_iter` → `fuse_update_attributes`）。
    fn revalidate_data(&self) -> Result<(), FileSystemError> {
        if self
            .negotiated()
            .is_ok_and(|negotiated| negotiated.has(fuse::AUTO_INVAL_DATA))
        {
            self.attributes()?;
        }
        if self.stale_data.load(Ordering::Acquire) {
            self.invalidate_data(false)?;
        }
        Ok(())
    }

    fn read_storage(&self, offset: u64, buffer: &mut [u8]) -> Result<usize, FileSystemError> {
        self.read_storage_pages(offset, &mut [buffer])
    }

    /// 协商的预读窗口（Linux `ra_pages = min(bdi, max_readahead / PAGE_SIZE)`），不超过单请求页数。
    fn readahead_pages(&self) -> usize {
        if self.mount.connection.congested() {
            return 1;
        }
        self.negotiated().map_or(1, |negotiated| {
            (negotiated.max_readahead / crate::memory::PAGE_SIZE as u32)
                .min(negotiated.max_pages)
                .max(1) as usize
        })
    }

    /// READ，按 `max_read` 与单请求页数分块，回复依次散布到各页；预读的整个窗口通常是一个请求。
    /// 短读是 EOF：其余字节补零并把缓存的大小收缩到实际读到的位置（Linux `fuse_short_read`）。
    fn read_storage_pages(
        &self,
        offset: u64,
        pages: &mut [&mut [u8]],
    ) -> Result<usize, FileSystemError> {
        let handle = self.handle(false)?;
        let negotiated = self.negotiated()?;
        let chunk = (negotiated.max_pages * crate::memory::PAGE_SIZE as u32)
            .min(self.mount.connection.max_read) as usize;
        let total: usize = pages.iter().map(|page| page.len()).sum();
        // 散布游标：当前页与页内位置。
        let (mut page, mut within) = (0, 0);
        let mut done = 0;
        while done < total {
            let size = chunk.min(total - done);
            let reply = self.read_handle(handle.reference, offset + done as u64, size)?;
            let mut source = &reply[..];
            while !source.is_empty() {
                let target = &mut pages[page][within..];
                let count = target.len().min(source.len());
                target[..count].copy_from_slice(&source[..count]);
                source = &source[count..];
                within += count;
                if within == pages[page].len() {
                    (page, within) = (page + 1, 0);
                }
            }
            done += reply.len();
            if reply.len() < size {
                for (position, rest) in pages.iter_mut().enumerate().skip(page) {
                    rest[if position == page { within } else { 0 }..].fill(0);
                }
                let end = offset + done as u64;
                let mut cached = self.attributes.lock();
                if end < cached.attributes.size {
                    cached.attributes.size = end;
                }
                break;
            }
        }
        Ok(total)
    }

    /// WRITE（write-through），按协商的写大小分块；扩展文件时更新缓存大小并作废属性。
    fn write_storage(
        &self,
        offset: u64,
        buffer: &[u8],
        _identity: Option<&AccessIdentity>,
    ) -> Result<usize, FileSystemError> {
        let handle = self.handle(true)?;
        let write_flags = if self.buffered_writes() {
            fuse::WRITE_CACHE
        } else {
            0
        };
        self.write_handle(handle.reference, offset, buffer, write_flags)
    }

    fn prepare_cached_write(&self, _identity: &AccessIdentity) -> Result<(), FileSystemError> {
        Ok(())
    }

    /// 协商了 WRITEBACK_CACHE 的普通文件：写入只进页缓存，由写回、close 与最后句柄释放交给守护进程。
    fn buffered_writes(&self) -> bool {
        self.kind == InodeType::File
            && self
                .negotiated()
                .is_ok_and(|negotiated| negotiated.has(fuse::WRITEBACK_CACHE))
    }

    /// 内核是大小与 mtime/ctime 的权威（Linux `i_size_write` + `file_update_time`）。
    fn extend_buffered_size(&self, size: u64) {
        let now = crate::timer::get_realtime_ns();
        let (seconds, nanoseconds) = (now / 1_000_000_000, (now % 1_000_000_000) as u32);
        let mut cached = self.attributes.lock();
        let attributes = &mut cached.attributes;
        attributes.size = attributes.size.max(size);
        (attributes.mtime, attributes.mtime_nsec) = (seconds, nanoseconds);
        (attributes.ctime, attributes.ctime_nsec) = (seconds, nanoseconds);
    }

    /// O_APPEND 在缓存的文件末尾写（Linux write-through 模式同样使用 `i_size`）。
    fn append_storage(
        &self,
        buffer: &[u8],
        identity: Option<&AccessIdentity>,
    ) -> Result<(u64, usize), FileSystemError> {
        let offset = self.size();
        self.write_storage(offset, buffer, identity)
            .map(|written| (offset, written))
    }

    fn truncate_storage(
        &self,
        size: u64,
        _identity: Option<&AccessIdentity>,
    ) -> Result<(), FileSystemError> {
        let mut set = SetAttributes::new(fuse::FATTR_SIZE);
        set.size = size;
        if let Ok(handle) = self.handle(true) {
            set.valid |= fuse::FATTR_FH;
            set.handle = handle.reference.handle;
        }
        self.set_attributes(set)?;
        // 缓冲写模式下属性刷新保留内核大小；截断本身就是新的内核大小。
        self.attributes.lock().attributes.size = size;
        Ok(())
    }

    /// FALLOCATE（mode 0）；守护进程不实现时返回 `EOPNOTSUPP`（Linux `no_fallocate`）。
    fn allocate_storage(
        &self,
        offset: u64,
        length: u64,
        _identity: Option<&AccessIdentity>,
    ) -> Result<(), FileSystemError> {
        if self.mount.unsupported(Unsupported::Fallocate) {
            return Err(FileSystemError::NotSupported);
        }
        let handle = self.handle(true)?;
        let body = Fields::<32>::new()
            .u64(handle.reference.handle)
            .u64(offset)
            .u64(length)
            .finish();
        match self.call(fuse::FALLOCATE, &[&body], ReplyShape::EMPTY) {
            Err(FileSystemError::Errno(code)) if code as isize == errno::ENOSYS => {
                self.mount.mark_unsupported(Unsupported::Fallocate);
                Err(FileSystemError::NotSupported)
            }
            Err(error) => Err(error),
            Ok(_) => {
                let mut cached = self.attributes.lock();
                cached.attributes.size = cached.attributes.size.max(offset + length);
                cached.valid_until = 0;
                Ok(())
            }
        }
    }

    fn sync_storage(&self) -> Result<(), FileSystemError> {
        match self.kind {
            InodeType::File => self.fsync(fuse::FSYNC, Unsupported::Fsync),
            InodeType::Directory => self.fsync(fuse::FSYNCDIR, Unsupported::FsyncDirectory),
            _ => Ok(()),
        }
    }

    fn set_times(&self, atime: Option<u64>, mtime: Option<u64>) -> Result<(), FileSystemError> {
        let mut set = SetAttributes::new(0);
        if let Some(atime) = atime {
            set.valid |= fuse::FATTR_ATIME;
            set.atime = atime;
        }
        if let Some(mtime) = mtime {
            set.valid |= fuse::FATTR_MTIME;
            set.mtime = mtime;
        }
        if set.valid == 0 {
            return Ok(());
        }
        self.set_attributes(set)?;
        // 缓冲写模式下属性刷新保留内核时间；显式设置的 mtime 就是新的内核值。
        if let Some(mtime) = mtime {
            let mut cached = self.attributes.lock();
            (cached.attributes.mtime, cached.attributes.mtime_nsec) = (mtime, 0);
        }
        Ok(())
    }

    /// chmod/chown → SETATTR。`default_permissions` 时按缓存属性在内核授权；否则把请求原样交给
    /// 服务器判定（Linux `ATTR_FORCE`）。
    fn change_owner_mode(&self, change: OwnerModeChange) -> Result<(), FileSystemError> {
        let attributes = self.attributes()?;
        let current = OwnerModeState::new(
            self.kind,
            attributes.mode as u16,
            attributes.uid,
            attributes.gid,
        );
        let target = if self.mount.options.default_permissions {
            change.authorize(current)?
        } else {
            change.requested(current)
        };
        let mut set = SetAttributes::new(0);
        if change.changes_mode() || u32::from(target.mode()) != attributes.mode {
            set.valid |= fuse::FATTR_MODE;
            set.mode = u32::from(target.mode());
        }
        if !change.changes_mode() {
            set.valid |= fuse::FATTR_UID | fuse::FATTR_GID;
            set.uid = target.uid();
            set.gid = target.gid();
        }
        self.set_attributes(set)
    }

    fn read_directory(
        &self,
        cursor: u64,
        visitor: &mut dyn DirectoryVisitor,
    ) -> Result<DirectoryRead, FileSystemError> {
        self.read_entries(cursor, visitor)
    }

    fn find_child(&self, name: &[u8]) -> Result<Arc<dyn Inode>, FileSystemError> {
        self.lookup(name)
    }

    fn create(
        &self,
        name: &[u8],
        kind: InodeType,
        metadata: CreateMetadata,
    ) -> Result<Arc<dyn Inode>, FileSystemError> {
        self.create_entry(name, kind, metadata)
    }

    fn create_open(
        &self,
        name: &[u8],
        metadata: CreateMetadata,
        flags: u32,
    ) -> Result<Option<crate::fs::inode::CreatedOpen>, FileSystemError> {
        self.create_and_open(name, metadata, flags)
    }

    fn mknod(
        &self,
        name: &[u8],
        kind: InodeType,
        metadata: CreateMetadata,
        device: Option<DeviceNumber>,
    ) -> Result<Arc<dyn Inode>, FileSystemError> {
        self.create_special(name, kind, metadata, device)
    }

    fn symlink(
        &self,
        name: &[u8],
        target: &[u8],
        _metadata: CreateMetadata,
    ) -> Result<Arc<dyn Inode>, FileSystemError> {
        self.create_symlink(name, target)
    }

    fn link(&self, name: &[u8], target: Arc<dyn Inode>) -> Result<(), FileSystemError> {
        self.create_link(name, target)
    }

    fn read_link(&self) -> Result<Vec<u8>, FileSystemError> {
        let target = self.call(fuse::READLINK, &[], ReplyShape::up_to(LINK_BUFFER))?;
        if target.len() >= LINK_BUFFER {
            return Err(FileSystemError::NameTooLong);
        }
        Ok(target)
    }

    /// Linux `fuse_entry_unlinked`：目录链接数清零，其他减一；属性作废以取得服务器的 ctime。
    fn unlinked(&self, directory: bool) {
        let mut cached = self.attributes.lock();
        cached.attributes.links = if directory {
            0
        } else {
            cached.attributes.links.saturating_sub(1)
        };
        cached.valid_until = 0;
    }

    fn unlink(&self, name: &[u8], remove_directory: bool) -> Result<(), FileSystemError> {
        self.remove_entry(name, remove_directory)
    }

    fn rename(
        &self,
        old_name: &[u8],
        new_parent_inode: u64,
        new_name: &[u8],
        no_replace: bool,
    ) -> Result<(), FileSystemError> {
        self.rename_entry(old_name, new_parent_inode, new_name, no_replace)
    }
}
