use alloc::{sync::Arc, vec::Vec};

use super::TaskControlBlock;
use crate::fs::{
    CancelledFileReservation, DetachedFileDescriptor, FileDescriptorError, OpenFileDescription,
    ProcFileDescriptorSnapshot, vfs,
};

const CLOEXEC_CLOSE_BATCH: usize = 32;

/// @description 一次 recvmsg 独占的 SCM_RIGHTS fd reservations。
///
/// descriptors 在 transaction 完成全部 name/control/msghdr copyout 前不可由 lookup、close、
/// fork 或 procfs 观察。消费式 `publish` 是唯一成功出口；其他退出路径由 Drop 撤销全部 slot，
/// 且 OFD cleanup 始终发生在 Process files lock 外。
pub(crate) struct ReceivedFdTransaction<'task> {
    task: &'task TaskControlBlock,
    descriptors: Vec<usize>,
}

impl ReceivedFdTransaction<'_> {
    /// @description 在当前 transaction 中占用下一个 lookup 不可见的最低 fd slot。
    /// @param file transport 已转交给 receiver 的 OFD。
    /// @param cloexec 对应 MSG_CMSG_CLOEXEC。
    /// @return 成功 reservation 返回 true；fd limit 截断返回 false。
    /// @errors fd-table backing OOM 返回 OutOfMemory；已有 reservations 由 transaction 保持。
    pub(crate) fn reserve(
        &mut self,
        file: Arc<OpenFileDescription>,
        cloexec: bool,
    ) -> Result<bool, FileDescriptorError> {
        let limit = self.task.file_descriptor_limit();
        let reservation = {
            self.task
                .process
                .files
                .lock()
                .reserve_received(file, cloexec, limit)
        };
        match reservation {
            Ok(fd) => {
                self.descriptors.push(fd);
                Ok(true)
            }
            Err((FileDescriptorError::Limit, file)) => {
                drop(file);
                Ok(false)
            }
            Err((error, file)) => {
                drop(file);
                Err(error)
            }
        }
    }

    /// @description 返回按 SCM_RIGHTS 输入顺序预留的最低可用 fd numbers。
    /// @return 只读 descriptor slice；transaction 完成前对应 slots 仍不可见。
    pub(crate) fn descriptors(&self) -> &[usize] {
        &self.descriptors
    }

    /// @description 在全部 recvmsg copyout 成功后无分配发布整批 descriptors。
    /// @return 无返回值；消费 transaction，禁止成功 publication 后再次 rollback。
    pub(crate) fn publish(mut self) {
        self.task.fd_publish_received(&self.descriptors);
        self.descriptors.clear();
    }
}

impl Drop for ReceivedFdTransaction<'_> {
    fn drop(&mut self) {
        while let Some(fd) = self.descriptors.pop() {
            let cancelled = self.task.fd_cancel_received(fd);
            // fd-table guard 已在 fd_cancel_received 返回前释放；OFD cleanup 不得在表锁内发生。
            drop(cancelled);
        }
    }
}

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

    /// @description 在 Process files owner 内一次性捕获 SCM_RIGHTS source descriptors。
    /// @param descriptors 已完成 raw cmsg 解码的 fd numbers。
    /// @return 同序 OFD Arc 集合。
    /// @errors 任一 fd 无效或 staging OOM 时不返回部分集合。
    pub(crate) fn fd_capture_many(
        &self,
        descriptors: &[usize],
    ) -> Result<alloc::vec::Vec<Arc<OpenFileDescription>>, FileDescriptorError> {
        self.process.files.lock().capture_many(descriptors)
    }

    /// @description 创建一次 recvmsg 独占、尚未包含 reservation 的 SCM_RIGHTS transaction。
    /// @param capacity 本条 control message 最多能够发布的 fd 数量。
    /// @return 预留好 rollback bookkeeping 的 transaction；caller 必须先 reserve 全部 fd，再 copyout。
    /// @errors transaction staging OOM 时不改变 fd table。
    pub(crate) fn fd_prepare_received(
        &self,
        capacity: usize,
    ) -> Result<ReceivedFdTransaction<'_>, FileDescriptorError> {
        let mut descriptors = Vec::new();
        descriptors
            .try_reserve_exact(capacity)
            .map_err(|_| FileDescriptorError::OutOfMemory)?;
        Ok(ReceivedFdTransaction {
            task: self,
            descriptors,
        })
    }

    /// @description 无分配公开已完成全部 recvmsg copyout 的 receive reservations。
    /// @param descriptors 当前 transaction 唯一拥有的完整 reserved slot slice。
    /// @return 无返回值；错误 token fail-stop。
    fn fd_publish_received(&self, descriptors: &[usize]) {
        self.process.files.lock().publish_received(descriptors);
    }

    /// @description 回滚 copyout 失败的 receive reservation。
    /// @param fd 当前 recvmsg transaction 唯一拥有的 reserved slot。
    /// @return 锁外 cleanup capability；caller 丢弃即可完成 descriptor_refs cleanup。
    fn fd_cancel_received(&self, fd: usize) -> CancelledFileReservation {
        self.process.files.lock().cancel_received(fd)
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
