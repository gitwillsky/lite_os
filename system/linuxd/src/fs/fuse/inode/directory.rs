//! FUSE 目录操作：名字缓存（Linux dentry 与 `d_time`）、LOOKUP、READDIR/READDIRPLUS 与命名空间变更。
use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::Ordering;

use syscall_abi::{
    errno, fuse,
    open::{O_NOCTTY, O_TRUNC},
};

use super::{FuseInode, io_in, now};
use crate::{
    fallible_tree::FallibleMap,
    fs::{
        CreateMetadata, DeviceNumber, DirectoryEntry, DirectoryRead, DirectoryVisit,
        DirectoryVisitor, FileSystemError, Inode, InodeOpen, InodeType,
        fuse::{
            connection::ReplyShape,
            filesystem::Unsupported,
            protocol::{self, DirectoryRecords, Entry, Fields},
        },
    },
};

/// READDIR 每次请求的缓冲区（Linux 使用一页）。
const DIRECTORY_BUFFER: u32 = 4096;
/// 一次 READDIRPLUS 回复最多的目录项数（每项至少是定长部分加 8 字节对齐的名字）。
const DIRECTORY_PLUS_RECORDS: usize =
    DIRECTORY_BUFFER as usize / (fuse::DIRENTPLUS_NAME_OFFSET + 8);

/// 一个缓存的名字。
pub(super) struct CachedEntry {
    inode: Arc<FuseInode>,
    valid_until: u64,
}

impl FuseInode {
    /// 守护进程的 INVAL_ENTRY/DELETE：摘除名字；DELETE 指明的子 inode 同时作废属性。
    pub(in crate::fs::fuse) fn invalidate_entry(&self, name: &[u8], child: Option<u64>) {
        let removed = self.entries.lock().remove(name);
        self.invalidate_attributes();
        if let Some(child) = child.and_then(|nodeid| self.mount.node(nodeid)) {
            child.invalidate_attributes();
        }
        drop(removed);
    }

    /// 卸载时断开目录项缓存持有的子 inode 引用。
    pub(in crate::fs::fuse) fn clear_entries(&self) {
        let removed = core::mem::take(&mut *self.entries.lock());
        drop(removed);
    }

    /// 回复 `fuse_entry_out` 的请求（LOOKUP、MKNOD、MKDIR、SYMLINK、LINK）：先预留 FORGET，
    /// 回复交给 [`Mount::adopt`]，成功后缓存名字。
    pub(super) fn entry_request(
        &self,
        opcode: u32,
        arguments: &[&[u8]],
        name: &[u8],
    ) -> Result<Arc<FuseInode>, FileSystemError> {
        self.mount.connection.reserve_forgets(1)?;
        let entry = self
            .call(opcode, arguments, ReplyShape::fixed(fuse::ENTRY_OUT_SIZE))
            .and_then(|reply| Entry::parse(&reply));
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                self.mount.connection.forget(0, 0);
                return Err(error);
            }
        };
        let inode = self.mount.adopt(&entry)?;
        self.cache_entry(name, &inode, entry.entry_valid);
        Ok(inode)
    }

    /// 缓存一个名字；顺带清掉本目录已过期的项。分配失败只是不缓存。
    pub(super) fn cache_entry(&self, name: &[u8], inode: &Arc<FuseInode>, valid: (u64, u32)) {
        let now = now();
        let valid_until = protocol::deadline(now, valid.0, valid.1);
        if valid_until <= now {
            return;
        }
        let mut key = Vec::new();
        if key.try_reserve_exact(name.len()).is_err() {
            return;
        }
        key.extend_from_slice(name);
        let Ok(prepared) = FallibleMap::try_prepare(
            key,
            CachedEntry {
                inode: inode.clone(),
                valid_until,
            },
        ) else {
            return;
        };
        let (expired, replaced) = {
            let mut entries = self.entries.lock();
            let mut expired = Vec::new();
            entries.retain(|_, entry| {
                let keep = entry.valid_until > now;
                if !keep && expired.try_reserve(1).is_ok() {
                    expired.push(entry.inode.clone());
                }
                keep
            });
            let replaced = entries.take_entry(name);
            entries.commit_vacant(prepared);
            (expired, replaced)
        };
        // 子 inode 的最后引用可能在此释放并 FORGET；不在目录锁内析构。
        drop(expired);
        drop(replaced);
    }

    pub(super) fn cached_child(&self, name: &[u8]) -> Option<Arc<FuseInode>> {
        let entries = self.entries.lock();
        entries
            .get(name)
            .filter(|entry| entry.valid_until > now())
            .map(|entry| entry.inode.clone())
    }

    /// 本地变更后摘除名字；被删除 inode 的链接数由 VFS 经 [`Inode::unlinked`] 告知。
    pub(super) fn forget_name(&self, name: &[u8]) {
        let removed = self.entries.lock().remove(name);
        drop(removed);
    }

    pub(super) fn create_node(
        &self,
        name: &[u8],
        mode: u32,
        rdev: u32,
    ) -> Result<Arc<dyn Inode>, FileSystemError> {
        let _serial = self.serialize()?;
        let body = Fields::<16>::new().u32(mode).u32(rdev).finish();
        let argument = protocol::name(name)?;
        let inode = self.entry_request(fuse::MKNOD, &[&body, &argument], name)?;
        self.invalidate_attributes();
        Ok(inode)
    }

    /// READDIR/READDIRPLUS 直到 visitor 停止或目录结束；cursor 是守护进程给出的不透明 offset。
    ///
    /// READDIRPLUS（Linux `fuse_use_readdirplus`）：协商了 DO_READDIRPLUS 且未要求 AUTO，或 AUTO 下
    /// 从头读、或本目录刚发生过子项查找时使用。守护进程为回复里每个非 `.`/`..` 的项都计了一次
    /// 查找，因此即使 visitor 中途停止也要接管全部项（Linux `parse_dirplusfile`）。
    pub(super) fn read_entries(
        &self,
        cursor: u64,
        visitor: &mut dyn DirectoryVisitor,
    ) -> Result<DirectoryRead, FileSystemError> {
        if self.kind != InodeType::Directory {
            return Err(FileSystemError::NotDirectory);
        }
        let handle = self.handle(false)?;
        let _serial = self.serialize()?;
        let negotiated = self.negotiated()?;
        let mut cursor = cursor;
        let mut stopped = false;
        loop {
            let plus = negotiated.has(fuse::DO_READDIRPLUS)
                && (!negotiated.has(fuse::READDIRPLUS_AUTO)
                    || cursor == 0
                    || self.advise_plus.swap(false, Ordering::Relaxed));
            let opcode = if plus {
                self.mount
                    .connection
                    .reserve_forgets(DIRECTORY_PLUS_RECORDS)?;
                fuse::READDIRPLUS
            } else {
                fuse::READDIR
            };
            let reply = self.call(
                opcode,
                &[&io_in(handle.reference, cursor, DIRECTORY_BUFFER, 0)],
                ReplyShape::up_to(DIRECTORY_BUFFER as usize),
            );
            let mut reserved = if plus { DIRECTORY_PLUS_RECORDS } else { 0 };
            let result = reply.and_then(|reply| {
                if reply.is_empty() {
                    return Ok(true);
                }
                for record in DirectoryRecords::new(&reply, plus) {
                    let record = record?;
                    if let Some(entry) = record.entry.filter(|entry| {
                        entry.nodeid != 0 && record.name != b"." && record.name != b".."
                    }) {
                        if reserved == 0 {
                            return Err(FileSystemError::IoError);
                        }
                        reserved -= 1;
                        if let Ok(inode) = self.mount.adopt(&entry) {
                            self.cache_entry(record.name, &inode, entry.entry_valid);
                        }
                    }
                    if stopped {
                        continue;
                    }
                    let visited = visitor.visit(
                        record.offset,
                        DirectoryEntry {
                            inode: record.ino,
                            kind: record.kind,
                            name: record.name,
                        },
                    )?;
                    match visited {
                        DirectoryVisit::Continue => cursor = record.offset,
                        DirectoryVisit::Stop => stopped = true,
                    }
                }
                Ok(false)
            });
            self.mount.connection.unreserve_forgets(reserved);
            if result? {
                return Ok(DirectoryRead { cursor, eof: true });
            }
            if stopped {
                return Ok(DirectoryRead { cursor, eof: false });
            }
        }
    }

    pub(super) fn lookup(&self, name: &[u8]) -> Result<Arc<dyn Inode>, FileSystemError> {
        if self.kind != InodeType::Directory {
            return Err(FileSystemError::NotDirectory);
        }
        if let Some(inode) = self.cached_child(name) {
            return Ok(inode);
        }
        self.advise_plus.store(true, Ordering::Relaxed);
        let _serial = self.serialize()?;
        let argument = protocol::name(name)?;
        self.entry_request(fuse::LOOKUP, &[&argument], name)
            .map(|inode| inode as Arc<dyn Inode>)
    }

    /// 普通文件以 MKNOD(S_IFREG) 创建，随后的打开另发 OPEN（Linux 无 atomic_open 时的路径）。
    pub(super) fn create_entry(
        &self,
        name: &[u8],
        kind: InodeType,
        metadata: CreateMetadata,
    ) -> Result<Arc<dyn Inode>, FileSystemError> {
        let permissions = metadata.mode & 0o7777;
        match kind {
            InodeType::File => self.create_node(name, 0o100_000 | permissions, 0),
            InodeType::Directory => {
                let _serial = self.serialize()?;
                let body = Fields::<8>::new().u32(permissions).finish();
                let argument = protocol::name(name)?;
                let inode = self.entry_request(fuse::MKDIR, &[&body, &argument], name)?;
                self.invalidate_attributes();
                Ok(inode)
            }
            _ => Err(FileSystemError::InvalidOperation),
        }
    }

    /// FUSE_CREATE（Linux `fuse_create_open`）：一次往返创建并打开普通文件，回复 entry 与 open 句柄。
    ///
    /// 1. 先预留 FORGET 与 RELEASE，回复后任何失败都能把查找计数与服务器句柄还给守护进程；
    /// 2. 守护进程不实现 CREATE 时记录 `no_create` 并返回 `None`，VFS 退回 MKNOD + OPEN；
    /// 3. 新 inode 接管 FORGET 预留，句柄登记为它的 per-open 状态。
    pub(super) fn create_and_open(
        &self,
        name: &[u8],
        metadata: CreateMetadata,
        flags: u32,
    ) -> Result<Option<crate::fs::inode::CreatedOpen>, FileSystemError> {
        if self.mount.unsupported(Unsupported::Create) {
            return Ok(None);
        }
        let _serial = self.serialize()?;
        // Linux：O_NOCTTY 不交给服务器；未协商 ATOMIC_O_TRUNC 时去掉 O_TRUNC（新文件本就为空）。
        let flags = self.server_open_flags(flags & !(O_NOCTTY | O_TRUNC));
        let release_in = Fields::<24>::new().u64(0).u32(flags).finish();
        let mut release = self
            .mount
            .connection
            .reserve(fuse::RELEASE, 0, &[&release_in])?;
        self.mount.connection.reserve_forgets(1)?;
        let body = Fields::<16>::new()
            .u32(flags)
            .u32(0o100_000 | metadata.mode & 0o7777)
            .finish();
        let argument = protocol::name(name)?;
        let reply = self.call(
            fuse::CREATE,
            &[&body, &argument],
            ReplyShape::fixed(fuse::ENTRY_OUT_SIZE + fuse::OPEN_OUT_SIZE),
        );
        let parsed = reply.and_then(|reply| {
            Ok((
                Entry::parse(&reply[..fuse::ENTRY_OUT_SIZE])?,
                protocol::open_out(&reply[fuse::ENTRY_OUT_SIZE..])?,
            ))
        });
        let (entry, opened) = match parsed {
            Ok(parsed) => parsed,
            Err(FileSystemError::Errno(code)) if code as isize == errno::ENOSYS => {
                self.mount.connection.forget(0, 0);
                self.mount.mark_unsupported(Unsupported::Create);
                return Ok(None);
            }
            Err(error) => {
                self.mount.connection.forget(0, 0);
                return Err(error);
            }
        };
        release.set_nodeid(entry.nodeid);
        let inode = match self.mount.adopt(&entry) {
            Ok(inode) => inode,
            Err(error) => {
                release.patch(0, &opened.handle.to_ne_bytes());
                release.send();
                return Err(error);
            }
        };
        self.cache_entry(name, &inode, entry.entry_valid);
        self.invalidate_attributes();
        let handle = inode.attach_handle(&opened, flags, release)?;
        Ok(Some((inode, InodeOpen::Storage(Some(handle)))))
    }

    pub(super) fn create_special(
        &self,
        name: &[u8],
        kind: InodeType,
        metadata: CreateMetadata,
        device: Option<DeviceNumber>,
    ) -> Result<Arc<dyn Inode>, FileSystemError> {
        let format = match (kind, device) {
            (InodeType::Fifo, None) => 0o010_000,
            (InodeType::Socket, None) => 0o140_000,
            (InodeType::CharacterDevice, Some(_)) => 0o020_000,
            (InodeType::BlockDevice, Some(_)) => 0o060_000,
            _ => return Err(FileSystemError::InvalidOperation),
        };
        let rdev = device.map_or(Ok(0), |device| {
            u32::try_from(device.encode()).map_err(|_| FileSystemError::InvalidOperation)
        })?;
        self.create_node(name, format | metadata.mode & 0o7777, rdev)
    }

    pub(super) fn create_symlink(
        &self,
        name: &[u8],
        target: &[u8],
    ) -> Result<Arc<dyn Inode>, FileSystemError> {
        let _serial = self.serialize()?;
        let entry_name = protocol::name(name)?;
        let link = protocol::name(target)?;
        let inode = self.entry_request(fuse::SYMLINK, &[&entry_name, &link], name)?;
        self.invalidate_attributes();
        Ok(inode)
    }

    pub(super) fn create_link(
        &self,
        name: &[u8],
        target: Arc<dyn Inode>,
    ) -> Result<(), FileSystemError> {
        if target.filesystem_id() != self.mount.filesystem_id {
            return Err(FileSystemError::CrossDevice);
        }
        let _serial = self.serialize()?;
        let body = Fields::<8>::new().u64(target.identity()?.1).finish();
        let argument = protocol::name(name)?;
        self.entry_request(fuse::LINK, &[&body, &argument], name)?;
        self.invalidate_attributes();
        Ok(())
    }

    pub(super) fn remove_entry(
        &self,
        name: &[u8],
        remove_directory: bool,
    ) -> Result<(), FileSystemError> {
        let _serial = self.serialize()?;
        let argument = protocol::name(name)?;
        let opcode = if remove_directory {
            fuse::RMDIR
        } else {
            fuse::UNLINK
        };
        self.call(opcode, &[&argument], ReplyShape::EMPTY)?;
        self.forget_name(name);
        self.invalidate_attributes();
        Ok(())
    }

    /// RENAME；`no_replace` 用 RENAME2(RENAME_NOREPLACE)，守护进程不支持时返回 `EINVAL`
    /// （Linux `no_rename2`）。两个名字都从缓存摘除，被覆盖的目标按删除处理。
    pub(super) fn rename_entry(
        &self,
        old_name: &[u8],
        new_parent_inode: u64,
        new_name: &[u8],
        no_replace: bool,
    ) -> Result<(), FileSystemError> {
        let _serial = self.serialize()?;
        let old = protocol::name(old_name)?;
        let new = protocol::name(new_name)?;
        if no_replace {
            if self.mount.unsupported(Unsupported::Rename2) {
                return Err(FileSystemError::InvalidOperation);
            }
            let body = Fields::<16>::new()
                .u64(new_parent_inode)
                .u32(fuse::RENAME_NOREPLACE)
                .finish();
            match self.call(fuse::RENAME2, &[&body, &old, &new], ReplyShape::EMPTY) {
                Err(FileSystemError::Errno(code)) if code as isize == errno::ENOSYS => {
                    self.mount.mark_unsupported(Unsupported::Rename2);
                    return Err(FileSystemError::InvalidOperation);
                }
                result => result?,
            };
        } else {
            let body = Fields::<8>::new().u64(new_parent_inode).finish();
            self.call(fuse::RENAME, &[&body, &old, &new], ReplyShape::EMPTY)?;
        }
        self.forget_name(old_name);
        self.invalidate_attributes();
        let new_parent = if new_parent_inode == self.nodeid {
            None
        } else {
            self.mount.node(new_parent_inode)
        };
        let target = new_parent.as_deref().unwrap_or(self);
        target.forget_name(new_name);
        target.invalidate_attributes();
        Ok(())
    }
}
