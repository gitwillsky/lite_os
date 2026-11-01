//! OPEN/OPENDIR/CREATE 得到的服务器文件句柄：per-open 状态、FLUSH/RELEASE，以及页缓存 I/O 的句柄选择。
use alloc::sync::{Arc, Weak};
use core::ops::Deref;

use syscall_abi::{errno, fuse};

use super::FuseInode;
use crate::{
    fs::{
        AccessIdentity, DirectIo, FileSystemError, Inode, InodeType, StorageOpen,
        fuse::{
            connection::{OpenedBacking, ReplyShape, Reserved},
            filesystem::{Mount, Unsupported},
            protocol::{self, Fields},
        },
    },
    memory::SharedFileId,
};

/// 应用打开对象；最后一个 OFD/映射引用负责写回，I/O 临时引用不执行打开对象的清理。
pub(super) struct Handle {
    remote: Arc<RemoteHandle>,
}

impl Deref for Handle {
    type Target = RemoteHandle;

    fn deref(&self) -> &Self::Target {
        &self.remote
    }
}

/// 服务器 fh 与预留 RELEASE；由打开对象及在途 I/O 保活，最后释放只发送 RELEASE。
/// 独立引用使页缓存 READ/WRITE 的清理不会重入它们正在等待的写序列。
pub(super) struct RemoteHandle {
    mount: Arc<Mount>,
    nodeid: u64,
    handle: u64,
    flags: u32,
    /// 回复里的 FOPEN_* 标志。
    open_flags: u32,
    backing: Option<OpenedBacking>,
    /// OPEN 得到的普通文件句柄；目录句柄没有 flush（Linux 目录 file_operations 不含 `flush`）。
    file: bool,
    readable: bool,
    writable: bool,
    release: Option<Reserved>,
}

impl StorageOpen for Handle {
    fn backing_file(&self) -> Option<Arc<dyn crate::fs::BackingOpen>> {
        self.backing.as_ref().map(|backing| backing.opened.clone())
    }
    fn direct_io(self: Arc<Self>) -> Option<Arc<dyn DirectIo>> {
        (self.file && (self.open_flags & fuse::FOPEN_DIRECT_IO != 0 || self.backing.is_some()))
            .then_some(self as Arc<dyn DirectIo>)
    }

    fn prepare_mapping(&self, shared: bool) -> Result<(), FileSystemError> {
        if self.backing.is_some() {
            return Ok(());
        }
        if self.open_flags & fuse::FOPEN_DIRECT_IO == 0 {
            return Ok(());
        }
        // 未协商 DIRECT_IO_ALLOW_MMAP；Linux 对只读 MAP_SHARED 也返回 ENODEV。
        if shared {
            return Err(FileSystemError::Errno(errno::ENODEV as u16));
        }
        crate::fs::page_cache::invalidate(
            SharedFileId {
                filesystem: self.mount.filesystem_id,
                inode: self.nodeid,
            },
            true,
        )
    }

    /// FUSE_FLUSH（Linux `fuse_flush`）：每次关闭一个 fd 发送，守护进程据此报告延迟的写错误并释放该
    /// 文件表在服务器上的 POSIX 锁。守护进程不实现时记录 `no_flush`，此后不再发送。
    fn flush(&self, owner: u64) -> Result<(), FileSystemError> {
        // WRITEBACK_CACHE：先把缓冲的写交给守护进程，FLUSH 才能报告它们的错误（Linux `fuse_flush`）。
        if self.file
            && self.writable
            && let Some(inode) = self.mount.node(self.nodeid)
            && inode.buffered_writes()
        {
            crate::fs::page_cache::write_back(SharedFileId {
                filesystem: self.mount.filesystem_id,
                inode: self.nodeid,
            })?;
        }
        if !self.file
            || self.open_flags & fuse::FOPEN_NOFLUSH != 0
            || self.mount.unsupported(Unsupported::Flush)
        {
            return Ok(());
        }
        let body = Fields::<24>::new()
            .u64(self.handle)
            .u64(0)
            .u64(owner)
            .finish();
        match self
            .mount
            .connection
            .call(fuse::FLUSH, self.nodeid, &[&body], ReplyShape::EMPTY)
        {
            Err(FileSystemError::Errno(code)) if code as isize == errno::ENOSYS => {
                self.mount.mark_unsupported(Unsupported::Flush);
                Ok(())
            }
            result => result.map(drop),
        }
    }
}

impl DirectIo for Handle {
    fn read(&self, offset: u64, output: &mut [u8]) -> Result<usize, FileSystemError> {
        if self.open_flags & fuse::FOPEN_DIRECT_IO == 0
            && let Some(backing) = &self.backing
        {
            return backing.opened.read(offset, output);
        }
        let inode = self
            .mount
            .node(self.nodeid)
            .ok_or(FileSystemError::NotFound)?;
        let chunk = (inode.negotiated()?.max_pages * crate::memory::PAGE_SIZE as u32)
            .min(self.mount.connection.max_read) as usize;
        let mut done = 0;
        while done < output.len() {
            let count = chunk.min(output.len() - done);
            let reply = match inode.read_handle(self.reference(), offset + done as u64, count) {
                Ok(reply) => reply,
                Err(_) if done != 0 => break,
                Err(error) => return Err(error),
            };
            output[done..done + reply.len()].copy_from_slice(&reply);
            done += reply.len();
            if reply.len() < count {
                break;
            }
        }
        Ok(done)
    }

    fn write(
        &self,
        offset: u64,
        input: &[u8],
        identity: &AccessIdentity,
    ) -> Result<usize, FileSystemError> {
        let inode = self
            .mount
            .node(self.nodeid)
            .ok_or(FileSystemError::NotFound)?;
        if self.open_flags & fuse::FOPEN_DIRECT_IO == 0
            && let Some(backing) = &self.backing
        {
            let written = backing.opened.write(offset, input, identity)?;
            if written != 0 {
                let mut cached = inode.attributes.lock();
                cached.attributes.size = cached.attributes.size.max(offset + written as u64);
                cached.valid_until = 0;
            }
            return Ok(written);
        }
        let flags = if identity.has_capability(syscall_abi::capability::Capability::FSETID) {
            0
        } else {
            fuse::WRITE_KILL_SUIDGID
        };
        inode.write_handle(self.reference(), offset, input, flags)
    }

    fn size(&self) -> Result<u64, FileSystemError> {
        let inode = self
            .mount
            .node(self.nodeid)
            .ok_or(FileSystemError::NotFound)?;
        if self.open_flags & fuse::FOPEN_DIRECT_IO == 0
            && let Some(backing) = &self.backing
        {
            let size = backing.opened.size()?;
            let mut cached = inode.attributes.lock();
            cached.attributes.size = size;
            cached.valid_until = 0;
            return Ok(size);
        }
        inode.invalidate_attributes();
        inode.metadata().map(|metadata| metadata.size)
    }
}

impl Handle {
    fn reference(&self) -> HandleRef {
        HandleRef {
            handle: self.handle,
            flags: self.flags,
        }
    }
}

/// 在途 READ/WRITE 的 fh 引用；保证 RELEASE 不越过请求，析构不写回或等待页缓存。
pub(super) struct HandleLease {
    pub(super) reference: HandleRef,
    _pin: Arc<RemoteHandle>,
}

/// inode 句柄表的一项：访问方向随登记保存，挑选时不必在锁内升级 Weak。
pub(super) struct RegisteredHandle {
    registration: Option<Weak<dyn crate::fs::BackingFile>>,
    direct: bool,
    handle: Weak<RemoteHandle>,
    readable: bool,
    writable: bool,
}

/// 发请求时使用的服务器句柄：句柄号与打开 flags（`fuse_read_in`/`fuse_write_in` 需要两者）。
#[derive(Debug, Clone, Copy)]
pub(super) struct HandleRef {
    pub(super) handle: u64,
    pub(super) flags: u32,
}

impl Drop for Handle {
    /// 打开对象退休前写回缓冲写与 mmap 脏页；其 remote 强引用仍可由 inode 升级。
    /// 写回失败无法从 Drop 报告，页保持 dirty；flush/fsync 的错误正常返回调用者。
    fn drop(&mut self) {
        if self.writable && self.file {
            let _ = crate::fs::page_cache::write_back(SharedFileId {
                filesystem: self.mount.filesystem_id,
                inode: self.nodeid,
            });
        }
    }
}

impl Drop for RemoteHandle {
    fn drop(&mut self) {
        if let Some(release) = self.release.take() {
            release.send();
        }
    }
}

impl FuseInode {
    /// 页缓存读写与 fsync 选择满足访问方向的存活服务器句柄。
    /// 返回值保活 fh，但不持有会执行同步写回的应用打开对象；无可用句柄返回 EBADF。
    pub(super) fn handle(&self, writable: bool) -> Result<HandleLease, FileSystemError> {
        loop {
            // 锁内只挑选 Weak，升级在锁外，最后一个临时引用的 RELEASE 也不在句柄表锁内发送。
            let candidate = self
                .handles
                .lock()
                .iter()
                .find(|entry| {
                    entry.handle.strong_count() != 0
                        && if writable {
                            entry.writable
                        } else {
                            entry.readable
                        }
                })
                .map(|entry| entry.handle.clone());
            let candidate = candidate.ok_or(FileSystemError::Errno(errno::EBADF as u16))?;
            // 挑选后到升级前它可能已被释放；重新挑选。
            if let Some(handle) = candidate.upgrade() {
                return Ok(HandleLease {
                    reference: HandleRef {
                        handle: handle.handle,
                        flags: handle.flags,
                    },
                    _pin: handle,
                });
            }
        }
    }

    /// 把 OPEN/OPENDIR/CREATE 得到的服务器句柄登记为本 inode 的 per-open 状态。
    ///
    /// `release` 是打开前预留的 RELEASE；任何失败都让它随 RemoteHandle 或自身 Drop 发出，服务器句柄不泄漏。
    pub(super) fn attach_handle(
        &self,
        opened: &protocol::Opened,
        flags: u32,
        mut release: Reserved,
    ) -> Result<Arc<Handle>, FileSystemError> {
        release.patch(0, &opened.handle.to_ne_bytes());
        let backing = if opened.flags & fuse::FOPEN_PASSTHROUGH != 0 {
            let allowed =
                fuse::FOPEN_PASSTHROUGH | fuse::FOPEN_DIRECT_IO | fuse::FOPEN_NOFLUSH | (1 << 6);
            if self.kind != InodeType::File || opened.flags & !allowed != 0 {
                release.send();
                return Err(FileSystemError::InvalidOperation);
            }
            match self.mount.connection.open_backing(opened.backing_id, flags) {
                Ok(backing) => Some(backing),
                Err(error) => {
                    release.send();
                    return Err(error);
                }
            }
        } else {
            None
        };
        let access = flags & crate::fs::O_ACCMODE;
        let remote = Arc::try_new(RemoteHandle {
            mount: self.mount.clone(),
            nodeid: self.nodeid,
            handle: opened.handle,
            flags,
            open_flags: opened.flags,
            backing,
            file: self.kind == InodeType::File,
            readable: access != crate::fs::O_WRONLY,
            writable: access != crate::fs::O_RDONLY,
            release: Some(release),
        })
        .map_err(|_| FileSystemError::OutOfMemory)?;
        // 多一份可失败的冷路径 Arc，把应用清理与 I/O fh 寿命分开；失败仍由 remote 释放预留 RELEASE。
        self.register_handle(&remote)?;
        Arc::try_new(Handle { remote }).map_err(|_| FileSystemError::OutOfMemory)
    }

    fn register_handle(&self, handle: &Arc<RemoteHandle>) -> Result<(), FileSystemError> {
        let mut handles = self.handles.lock();
        handles.retain(|entry| entry.handle.strong_count() != 0);
        let registration = handle
            .backing
            .as_ref()
            .map(|backing| Arc::downgrade(&backing.registration));
        for active in handles.iter() {
            let valid = match (&registration, &active.registration) {
                (Some(new), Some(old)) => Weak::ptr_eq(new, old),
                (Some(_), None) => active.direct,
                (None, Some(_)) => false,
                (None, None) => true,
            };
            if !valid {
                return Err(FileSystemError::InvalidOperation);
            }
        }
        handles
            .try_reserve(1)
            .map_err(|_| FileSystemError::OutOfMemory)?;
        handles.push(RegisteredHandle {
            registration,
            direct: handle.open_flags & fuse::FOPEN_DIRECT_IO != 0,
            handle: Arc::downgrade(handle),
            readable: handle.readable,
            writable: handle.writable,
        });
        Ok(())
    }
}
