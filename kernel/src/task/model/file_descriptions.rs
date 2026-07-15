use alloc::sync::Arc;

use super::TaskControlBlock;
use crate::fs::{
    DetachedFileDescriptor, FileDescriptorError, OpenFileDescription, ProcFileDescriptorSnapshot,
    vfs,
};

const CLOEXEC_CLOSE_BATCH: usize = 32;

impl TaskControlBlock {
    pub(crate) fn fd_get(&self, fd: usize) -> Option<Arc<OpenFileDescription>> {
        self.process.files.lock().get(fd)
    }

    pub(crate) fn fd_allocate(
        &self,
        ofd: Arc<OpenFileDescription>,
        cloexec: bool,
    ) -> Result<usize, FileDescriptorError> {
        self.process
            .files
            .lock()
            .allocate(ofd, 0, cloexec, self.file_descriptor_limit())
    }

    pub(crate) fn fd_allocate_pair(
        &self,
        first: Arc<OpenFileDescription>,
        second: Arc<OpenFileDescription>,
        cloexec: bool,
    ) -> Result<(usize, usize), FileDescriptorError> {
        self.process.files.lock().allocate_pair(
            first,
            second,
            cloexec,
            self.file_descriptor_limit(),
        )
    }

    pub(crate) fn fd_close(&self, fd: usize) -> Result<(), ()> {
        let descriptor = self.process.files.lock().detach(fd)?;
        let ofd = descriptor.finish_close();
        vfs().release_record_locks_for_file(self.tgid(), &ofd);
        Ok(())
    }

    /// @description 在最后一个 Thread exit commit 后立即关闭 Process 的全部 fd。
    ///
    /// @return 无返回值；OFD Drop 在 files lock 外执行并可唤醒 pipe peer。
    pub(crate) fn close_all_files(&self) {
        let files = self.process.files.lock().take_all();
        vfs().release_process_record_locks(self.tgid());
        drop(files);
    }

    /// @description exec commit 逐个关闭 CLOEXEC descriptors，并执行 process-owned record-lock cleanup。
    ///
    /// @return 无返回值；非 CLOEXEC descriptors 与其 record locks 保持跨 exec 存活。
    pub(super) fn close_cloexec_files(&self) {
        let mut cursor = 0;
        let mut batch: [Option<DetachedFileDescriptor>; CLOEXEC_CLOSE_BATCH] =
            core::array::from_fn(|_| None);
        loop {
            let count = self
                .process
                .files
                .lock()
                .take_cloexec_batch(&mut cursor, &mut batch);
            if count == 0 {
                break;
            }
            for descriptor in &mut batch[..count] {
                let ofd = descriptor
                    .take()
                    .expect("CLOEXEC batch count exceeded detached entries")
                    .finish_close();
                vfs().release_record_locks_for_file(self.tgid(), &ofd);
            }
        }
    }

    pub(crate) fn fd_duplicate(
        &self,
        old: usize,
        minimum: usize,
        cloexec: bool,
    ) -> Result<usize, FileDescriptorError> {
        self.process
            .files
            .lock()
            .duplicate(old, minimum, cloexec, self.file_descriptor_limit())
    }

    pub(crate) fn fd_duplicate_to(
        &self,
        old: usize,
        new: usize,
        cloexec: bool,
    ) -> Result<usize, FileDescriptorError> {
        let replaced = {
            let mut files = self.process.files.lock();
            files.duplicate_to(old, new, cloexec, self.file_descriptor_limit())?
        };
        if let Some(descriptor) = replaced {
            let ofd = descriptor.finish_close();
            vfs().release_record_locks_for_file(self.tgid(), &ofd);
        }
        Ok(new)
    }

    pub(crate) fn fd_flags(&self, fd: usize) -> Result<u32, ()> {
        self.process.files.lock().descriptor_flags(fd)
    }

    pub(crate) fn fd_set_flags(&self, fd: usize, flags: u32) -> Result<(), ()> {
        self.process.files.lock().set_descriptor_flags(fd, flags)
    }

    /// @description 在 Process fd-table owner lock 内解析两个 descriptor 并执行一次操作。
    ///
    /// @param first 第一个 descriptor number。
    /// @param second 第二个 descriptor number。
    /// @param operation 只消费两个 live OFD identity、不得阻塞或再次获取本 Process fd-table。
    /// @return 任一 fd 不存在返回 `None`；否则返回 operation 结果。
    pub(crate) fn with_file_descriptions<R>(
        &self,
        first: usize,
        second: usize,
        operation: impl FnOnce(Arc<OpenFileDescription>, Arc<OpenFileDescription>) -> R,
    ) -> Option<R> {
        let files = self.process.files.lock();
        let first = files.get(first)?;
        let second = files.get(second)?;
        Some(operation(first, second))
    }

    /// @description 在 Process files lock 外解析 procfs fd targets，避免 VFS lock 进入 fd owner。
    /// @return 按 fd 递增的 target 快照；分配或 VFS 投影失败返回 None。
    pub(crate) fn process_file_descriptors(
        &self,
    ) -> Option<alloc::vec::Vec<ProcFileDescriptorSnapshot>> {
        let descriptions = self.process.files.lock().snapshot().ok()?;
        let mut snapshots = alloc::vec::Vec::new();
        snapshots.try_reserve_exact(descriptions.len()).ok()?;
        for (fd, ofd) in descriptions {
            snapshots.push(ProcFileDescriptorSnapshot {
                fd,
                target: ofd.proc_target().ok()?,
                opened: ofd.opened_ref(),
            });
        }
        Some(snapshots)
    }
}
