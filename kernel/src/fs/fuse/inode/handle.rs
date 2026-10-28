//! OPEN/OPENDIR/CREATE 得到的服务器文件句柄：per-open 状态、FLUSH/RELEASE，以及页缓存 I/O 的句柄选择。
use alloc::sync::{Arc, Weak};

use syscall_abi::{errno, fuse};

use super::FuseInode;
use crate::{
    fs::{
        FileSystemError, Inode, InodeType, StorageOpen,
        fuse::{
            connection::{ReplyShape, Reserved},
            filesystem::{Mount, Unsupported},
            protocol::{self, Fields},
        },
    },
    memory::SharedFileId,
};

/// OPEN/OPENDIR 得到的服务器文件句柄（Linux `fuse_file`）；随 OFD 最后释放发出 RELEASE/RELEASEDIR。
pub(super) struct Handle {
    mount: Arc<Mount>,
    nodeid: u64,
    handle: u64,
    flags: u32,
    /// 回复里的 FOPEN_* 标志。
    open_flags: u32,
    /// OPEN 得到的普通文件句柄；目录句柄没有 flush（Linux 目录 file_operations 不含 `flush`）。
    file: bool,
    readable: bool,
    writable: bool,
    release: Option<Reserved>,
}

impl StorageOpen for Handle {
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

/// 一次请求持有的句柄：打开中的句柄被 pin 到请求结束（Linux I/O 持有 `fuse_file` 引用），RELEASE
/// 不能越过在途的 READ/WRITE；正在释放、写出剩余脏页的句柄没有 pin。
pub(super) struct HandleLease {
    pub(super) reference: HandleRef,
    _pin: Option<Arc<Handle>>,
}

/// inode 句柄表的一项：访问方向随登记保存，挑选时不必在锁内升级 Weak。
pub(super) struct RegisteredHandle {
    handle: Weak<Handle>,
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
    /// 最后一个引用释放：可写普通文件先用本句柄写出剩余脏页（缓冲写与 mmap 脏页；Linux 由
    /// `fuse_flush`/`fuse_vma_close` 保证），再发 RELEASE。缺失时脏页在句柄消失后写回只能得到 EBADF，
    /// 数据丢失。写回失败无法报告，页保持 dirty。
    fn drop(&mut self) {
        if self.writable
            && self.file
            && let Some(inode) = self.mount.node(self.nodeid)
        {
            inode.drain_writes(HandleRef {
                handle: self.handle,
                flags: self.flags,
            });
        }
        if let Some(release) = self.release.take() {
            release.send();
        }
    }
}

impl FuseInode {
    /// 用即将释放的可写句柄写出本文件剩余脏页（见 [`Handle`] 的 Drop）。
    pub(super) fn drain_writes(&self, handle: HandleRef) {
        let previous = self.draining.lock().replace(handle);
        let _ = crate::fs::page_cache::write_back(SharedFileId {
            filesystem: self.mount.filesystem_id,
            inode: self.nodeid,
        });
        *self.draining.lock() = previous;
    }

    /// 页缓存读写与 fsync 使用的服务器句柄：任一满足访问方向的存活句柄；都没有时用正在释放、
    /// 写出剩余脏页的那个可写句柄。
    pub(super) fn handle(&self, writable: bool) -> Result<HandleLease, FileSystemError> {
        loop {
            // 锁内只按登记的访问方向挑选 Weak，升级在锁外：临时 Arc 若成为最后引用，其 Drop 会写回
            // 脏页并再次进入本函数取同一把锁。
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
            let Some(candidate) = candidate else {
                return (*self.draining.lock())
                    .filter(|_| writable)
                    .map(|reference| HandleLease {
                        reference,
                        _pin: None,
                    })
                    .ok_or(FileSystemError::Errno(errno::EBADF as u16));
            };
            // 挑选后到升级前它可能已被释放；重新挑选。
            if let Some(handle) = candidate.upgrade() {
                return Ok(HandleLease {
                    reference: HandleRef {
                        handle: handle.handle,
                        flags: handle.flags,
                    },
                    _pin: Some(handle),
                });
            }
        }
    }

    /// 把 OPEN/OPENDIR/CREATE 得到的服务器句柄登记为本 inode 的 per-open 状态。
    ///
    /// `release` 是打开前预留的 RELEASE；任何失败都让它随 Handle 或自身 Drop 发出，服务器句柄不泄漏。
    pub(super) fn attach_handle(
        &self,
        opened: &protocol::Opened,
        flags: u32,
        mut release: Reserved,
    ) -> Result<Arc<Handle>, FileSystemError> {
        release.patch(0, &opened.handle.to_ne_bytes());
        let access = flags & crate::fs::O_ACCMODE;
        let handle = Arc::try_new(Handle {
            mount: self.mount.clone(),
            nodeid: self.nodeid,
            handle: opened.handle,
            flags,
            open_flags: opened.flags,
            file: self.kind == InodeType::File,
            readable: access != crate::fs::O_WRONLY,
            writable: access != crate::fs::O_RDONLY,
            release: Some(release),
        })
        .map_err(|_| FileSystemError::OutOfMemory)?;
        self.register_handle(&handle)?;
        Ok(handle)
    }

    pub(super) fn register_handle(&self, handle: &Arc<Handle>) -> Result<(), FileSystemError> {
        let mut handles = self.handles.lock();
        handles.retain(|entry| entry.handle.strong_count() != 0);
        handles
            .try_reserve(1)
            .map_err(|_| FileSystemError::OutOfMemory)?;
        handles.push(RegisteredHandle {
            handle: Arc::downgrade(handle),
            readable: handle.readable,
            writable: handle.writable,
        });
        Ok(())
    }
}
