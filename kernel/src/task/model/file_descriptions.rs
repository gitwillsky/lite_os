use alloc::sync::Arc;

use super::TaskControlBlock;
use crate::file::{DetachedFileDescriptor, FileDescriptorError, OpenFileDescription};
use crate::fs::vfs;

const CLOEXEC_CLOSE_BATCH: usize = 32;

impl TaskControlBlock {
    pub(crate) fn fd_get(&self, fd: usize) -> Option<Arc<OpenFileDescription>> {
        self.fd_get_raw(fd).filter(|file| !file.is_path())
    }

    /// 原生 fget_raw：仅路径/属性/dup 操作可取得 O_PATH；普通 I/O 共用 fd_get 的能力过滤。
    pub(crate) fn fd_get_raw(&self, fd: usize) -> Option<Arc<OpenFileDescription>> {
        self.with_files(|files| files.get(fd))
    }

    pub(crate) fn fd_allocate(
        &self,
        ofd: Arc<OpenFileDescription>,
        minimum: usize,
        cloexec: bool,
    ) -> Result<usize, FileDescriptorError> {
        self.with_files(|files| {
            files.allocate(ofd.clone(), minimum, cloexec, self.file_descriptor_limit())
        })
    }

    /// seccomp ADDFD 在被过滤线程上下文接收实际 OFD；不持通知/graph 锁操作文件表。
    /// # Errors
    /// fd limit、指定 slot 无效、reservation 冲突与 radix OOM 独立返回。
    pub(crate) fn fd_receive(
        &self,
        file: Arc<OpenFileDescription>,
        destination: Option<usize>,
        cloexec: bool,
    ) -> Result<usize, FileDescriptorError> {
        let (fd, retired) = self.with_files(|files| {
            files.receive(
                file.clone(),
                destination,
                cloexec,
                self.file_descriptor_limit(),
            )
        })?;
        if let Some(descriptor) = retired {
            let old = descriptor.finish_close();
            vfs().release_record_locks_for_file(
                self.file_lock_owner().identity(),
                &old.lock_holder(),
            );
        }
        Ok(fd)
    }

    /// 在 FileTable owner 内一次性捕获 SCM_RIGHTS source descriptors。
    ///
    /// # Parameters
    ///
    /// - `descriptors`: 已完成 raw cmsg 解码的 fd numbers。
    ///
    /// # Returns
    ///
    /// 同序 OFD Arc 集合。
    ///
    /// # Errors
    ///
    /// 任一 fd 无效或 staging OOM 时不返回部分集合。
    pub(crate) fn fd_capture_many(
        &self,
        descriptors: &[usize],
    ) -> Result<alloc::vec::Vec<Arc<OpenFileDescription>>, FileDescriptorError> {
        self.with_files(|files| files.capture_many(descriptors))
    }

    pub(crate) fn fd_close(&self, fd: usize) -> Result<(), ()> {
        let descriptor = self.with_files(|files| files.detach(fd))?;
        let ofd = descriptor.finish_close();
        vfs().release_record_locks_for_file(self.file_lock_owner().identity(), &ofd.lock_holder());
        Ok(())
    }

    /// POSIX lock 提交后在同一表 guard 下复验 fd/OFD identity；共享表 close/reuse 可跨等待发生。
    ///
    /// # Errors
    ///
    /// fd 已关闭或指向另一 OFD 时，锁外清除本表在原 inode 上的全部锁并返回错误。
    pub(crate) fn finish_record_lock(
        &self,
        fd: usize,
        expected: &Arc<OpenFileDescription>,
    ) -> Result<(), ()> {
        let unchanged = self.with_files(|files| {
            files
                .get(fd)
                .is_some_and(|current| Arc::ptr_eq(&current, expected))
        });
        if unchanged {
            Ok(())
        } else {
            vfs().release_record_locks_for_file(
                self.file_lock_owner().identity(),
                &expected.lock_holder(),
            );
            Err(())
        }
    }

    /// exec commit 逐个关闭 CLOEXEC descriptors，并执行 file-table-owned record-lock cleanup。
    ///
    /// # Returns
    ///
    /// 无返回值；非 CLOEXEC descriptors 与其 record locks 保持跨 exec 存活。
    pub(super) fn close_cloexec_files(&self) {
        let mut cursor = 0;
        let mut batch: [Option<DetachedFileDescriptor>; CLOEXEC_CLOSE_BATCH] =
            core::array::from_fn(|_| None);
        loop {
            let count = self.with_files(|files| files.take_cloexec_batch(&mut cursor, &mut batch));
            if count == 0 {
                break;
            }
            for descriptor in &mut batch[..count] {
                let ofd = descriptor
                    .take()
                    .expect("CLOEXEC batch count exceeded detached entries")
                    .finish_close();
                vfs().release_record_locks_for_file(
                    self.file_lock_owner().identity(),
                    &ofd.lock_holder(),
                );
            }
        }
    }

    pub(crate) fn fd_duplicate(
        &self,
        old: usize,
        minimum: usize,
        cloexec: bool,
    ) -> Result<usize, FileDescriptorError> {
        self.with_files(|files| {
            files.duplicate(old, minimum, cloexec, self.file_descriptor_limit())
        })
    }

    pub(crate) fn fd_duplicate_to(
        &self,
        old: usize,
        new: usize,
        cloexec: bool,
    ) -> Result<usize, FileDescriptorError> {
        let replaced = self.with_files(|files| {
            files.duplicate_to(old, new, cloexec, self.file_descriptor_limit())
        })?;
        if let Some(descriptor) = replaced {
            let ofd = descriptor.finish_close();
            vfs().release_record_locks_for_file(
                self.file_lock_owner().identity(),
                &ofd.lock_holder(),
            );
        }
        Ok(new)
    }

    pub(crate) fn fd_flags(&self, fd: usize) -> Result<u32, ()> {
        self.with_files(|files| files.descriptor_flags(fd))
    }

    pub(crate) fn fd_set_flags(&self, fd: usize, flags: u32) -> Result<(), ()> {
        self.with_files(|files| files.set_descriptor_flags(fd, flags))
    }

    /// 在同一 file-table guard 内捕获两个 I/O-capable OFD，锁外执行一次操作。
    ///
    /// # Parameters
    ///
    /// - `first`: 第一个 descriptor number。
    /// - `second`: 第二个 descriptor number。
    /// - `operation`: 消费两个已 pin 的 live OFD identity；运行时不持 file-table guard。
    ///
    /// # Returns
    ///
    /// 任一 fd 不存在或为 O_PATH 返回 `None`；否则返回 operation 结果。
    pub(crate) fn with_file_descriptions<R>(
        &self,
        first: usize,
        second: usize,
        operation: impl FnOnce(Arc<OpenFileDescription>, Arc<OpenFileDescription>) -> R,
    ) -> Option<R> {
        let (first, second) =
            self.with_files(|files| Some((files.get(first)?, files.get(second)?)))?;
        if first.is_path() || second.is_path() {
            return None;
        }
        Some(operation(first, second))
    }
}
