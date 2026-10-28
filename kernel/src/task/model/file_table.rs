//! 可共享文件表的单 owner；Thread handle 与表内容的生命周期相互独立。
use alloc::sync::Arc;
use spin::Mutex;

use super::TaskControlBlock;
use crate::{file::FileDescriptorTable, fs::vfs, memory::MemoryError, sync::TaskMutex};

/// Thread 私有的可脱离 handle；Process 仅别名保活原 leader 的同一 handle。
pub(super) type FileTableHandle = Arc<Mutex<Option<Arc<FileTableOwner>>>>;

/// 文件表、descriptor publication 与 POSIX lock identity 的唯一生命周期 owner。
pub(super) struct FileTableOwner {
    table: TaskMutex<FileDescriptorTable>,
}

/// 普通 open 的消费式 fd reservation；pin 原表，退出路径只撤销该表的不可见 slot。
///
/// Path lookup、device/FIFO open 和 truncate 期间不持 table guard；没有假 OFD，亦不分配
/// rollback bookkeeping。成功 publish 与失败 Drop 均使用 caller 的预置 file waiter。
pub(crate) struct OpenFdReservation<const N: usize = 1> {
    owner: Arc<FileTableOwner>,
    descriptors: [Option<usize>; N],
}
impl<const N: usize> OpenFdReservation<N> {
    /// 返回 reservation 独占的 fd；copyout 完成前所有 slot 仍不可见。
    pub(crate) fn descriptors(&self) -> [usize; N] {
        self.descriptors
            .map(|fd| fd.expect("open reservation consumed twice"))
    }

    /// 在原 owner 的一次 guard 内无分配发布全部 OFD，其他线程不能观察半对 descriptor。
    pub(crate) fn publish_many(
        mut self,
        descriptions: [Arc<crate::file::OpenFileDescription>; N],
    ) -> [usize; N] {
        let descriptors = self.descriptors();
        self.owner.with_table(|files| {
            for (fd, file) in descriptors.into_iter().zip(descriptions) {
                files.publish_open(fd, file);
            }
        });
        self.descriptors.fill(None);
        descriptors
    }
}
impl OpenFdReservation<1> {
    /// 单个 open 与多 fd transaction 使用同一 publication 算法。
    pub(crate) fn publish(self, file: Arc<crate::file::OpenFileDescription>) -> usize {
        self.publish_many([file])[0]
    }
}
impl<const N: usize> Drop for OpenFdReservation<N> {
    fn drop(&mut self) {
        for descriptor in &mut self.descriptors {
            if let Some(fd) = descriptor.take() {
                let cancelled = self.owner.with_table(|files| files.cancel_reserved(fd));
                drop(cancelled);
            }
        }
    }
}

impl core::fmt::Debug for FileTableOwner {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("FileTableOwner")
            .finish_non_exhaustive()
    }
}

impl FileTableOwner {
    pub(super) fn try_new(table: FileDescriptorTable) -> Result<Arc<Self>, MemoryError> {
        Arc::try_new(Self {
            table: TaskMutex::new(table),
        })
        .map_err(|_| MemoryError::OutOfMemory)
    }

    /// 只在稳定 Arc value 的 lifetime 内使用；最后 Drop 先清锁再释放地址，禁止 ABA。
    pub(super) fn identity(&self) -> usize {
        core::ptr::from_ref(self).addr()
    }

    /// 实际 FileTable owner 的 proc fd 投影；锁外分配/冻结 pathname target。
    fn proc_file_descriptors(
        &self,
    ) -> Result<alloc::vec::Vec<crate::fs::ProcFileDescriptorSnapshot>, crate::fs::FileSystemError>
    {
        let descriptions = self
            .with_table(|files| files.snapshot())
            .map_err(|_| crate::fs::FileSystemError::OutOfMemory)?;
        let mut snapshots = alloc::vec::Vec::new();
        snapshots
            .try_reserve_exact(descriptions.len())
            .map_err(|_| crate::fs::FileSystemError::OutOfMemory)?;
        for (fd, ofd) in descriptions {
            snapshots.push(crate::fs::ProcFileDescriptorSnapshot {
                fd,
                target: ofd.opened_ref(),
            });
        }
        Ok(snapshots)
    }

    /// caller 的预置 waiter 令查找与不可失败 cleanup 不因临时 OOM 返回伪造 EBADF。
    /// 取得 table guard 后立即归还 waiter；closure 不得重新取同一表或完成 FD 最终析构。
    fn with_table<R>(&self, operation: impl FnOnce(&mut FileDescriptorTable) -> R) -> R {
        let caller = crate::task::current_task().expect("file table requires current Thread");
        let mut preparation = caller
            .file_wait()
            .lock()
            .take()
            .expect("nested file table waiter acquisition");
        let mut table = self.table.lock_prepared(&mut preparation);
        let previous = caller.file_wait().lock().replace(preparation);
        assert!(
            previous.is_none(),
            "file table waiter owner replaced during acquisition"
        );
        operation(&mut table)
    }
}

impl Drop for FileTableOwner {
    fn drop(&mut self) {
        // 1. 进程退出关闭剩余 fd（Linux close_files → filp_close）：每个 fd 一次 flush，错误不可报告。
        //    最后 Drop 独占本表，try_lock 必然成功；flush 可等待 filesystem，但不取其他文件表。
        let owner = self.identity();
        if let Some(table) = self.table.try_lock() {
            for file in table.published_files() {
                let _ = file.flush(owner);
            }
        }
        // 2. Arc address 尚未释放，table guard/handle guard 均不存在；notify 可进入其他 owner。
        vfs().release_file_table_record_locks(owner);
    }
}

pub(super) fn try_file_handle(owner: Arc<FileTableOwner>) -> Result<FileTableHandle, MemoryError> {
    Arc::try_new(Mutex::new(Some(owner))).map_err(|_| MemoryError::OutOfMemory)
}

impl TaskControlBlock {
    /// pidfd_getfd 在目标现有 exec guard 下执行 native REALCREDS 授权和 actual file-table capture。
    /// OOM/interrupt 不进入 descriptor publication；退出目标返回 ESRCH，无效目标 fd 为 EBADF。
    pub(in crate::task) fn capture_pidfd_file(
        &self,
        caller: &TaskControlBlock,
        descriptor: usize,
    ) -> Result<Arc<crate::file::OpenFileDescription>, crate::task::PidFdError> {
        use crate::task::PidFdError;
        if self.is_kernel_thread() {
            return Err(if caller.may_inspect_thread_real(self) {
                PidFdError::NotFound
            } else {
                PidFdError::Permission
            });
        }
        let _exec = self
            .credential_guard()
            .lock_killable()
            .map_err(|error| match error {
                crate::sync::TaskMutexLockError::OutOfMemory => PidFdError::NoMemory,
                crate::sync::TaskMutexLockError::Interrupted => PidFdError::Interrupted,
            })?;
        if !caller.may_inspect_thread_real(self) {
            return Err(PidFdError::Permission);
        }
        if !crate::task::process_table::pidfd::file_accessible(self) {
            return Err(PidFdError::NotFound);
        }
        let table = self.file_table().ok_or(PidFdError::NotFound)?;
        table
            .with_table(|files| files.get(descriptor))
            .ok_or(PidFdError::BadDescriptor)
    }
    fn file_table(&self) -> Option<Arc<FileTableOwner>> {
        if self.is_kernel_thread() {
            None
        } else {
            self.thread().files.lock().clone()
        }
    }

    /// 在 filesystem 打开前预留不可见 fd，并 pin 精确 FileTable owner。
    /// # Errors
    /// fd resource limit 或 radix backing OOM；失败不改变 filesystem/device。
    pub(crate) fn fd_prepare_open(
        &self,
        cloexec: bool,
    ) -> Result<OpenFdReservation, crate::file::FileDescriptorError> {
        self.fd_prepare_opens::<1>(cloexec)
    }

    /// 在一个原 FileTable owner 上预留 N 个 fd；失败只撤销此次 transaction 的不可见 slot。
    /// # Errors
    /// fd limit 或 radix OOM；全部 reservation 由同一 token 保活，cleanup 不分配。
    pub(crate) fn fd_prepare_opens<const N: usize>(
        &self,
        cloexec: bool,
    ) -> Result<OpenFdReservation<N>, crate::file::FileDescriptorError> {
        let owner = self.file_table().expect("live Thread requires file table");
        let mut reservation = OpenFdReservation {
            owner,
            descriptors: [None; N],
        };
        let limit = self.file_descriptor_limit();
        reservation.owner.with_table(|files| {
            for descriptor in &mut reservation.descriptors {
                *descriptor = Some(files.reserve_open(cloexec, limit)?);
            }
            Ok::<(), crate::file::FileDescriptorError>(())
        })?;
        Ok(reservation)
    }

    pub(super) fn with_files<R>(&self, operation: impl FnOnce(&mut FileDescriptorTable) -> R) -> R {
        let owner = self.file_table().expect("live Thread requires file table");
        owner.with_table(operation)
    }

    pub(super) fn clone_file_table(&self, share: bool) -> Result<FileTableHandle, MemoryError> {
        let current = self.file_table().expect("live Thread requires file table");
        let owner = if share {
            current
        } else {
            let copied = current
                .with_table(|table| table.try_clone())
                .map_err(|_| MemoryError::OutOfMemory)?;
            FileTableOwner::try_new(copied)?
        };
        try_file_handle(owner)
    }

    /// 原 Thread 自己的实际文件表；exit_files 后返回空，不借其他 Thread 的表。
    pub(in crate::task) fn proc_file_descriptors(
        &self,
    ) -> Result<alloc::vec::Vec<crate::fs::ProcFileDescriptorSnapshot>, crate::fs::FileSystemError>
    {
        self.file_table().map_or_else(
            || Ok(alloc::vec::Vec::new()),
            |owner| owner.proc_file_descriptors(),
        )
    }

    /// fdinfo 只捕获该 Thread 的实际文件表，并在 guard 外读取 OFD；退出后不借 sibling 的表。
    pub(in crate::task) fn proc_file_information(
        &self,
        fd: usize,
        view: &crate::namespace::PidNamespace,
    ) -> Result<Option<alloc::vec::Vec<u8>>, crate::fs::FileSystemError> {
        let Some(owner) = self.file_table() else {
            return Ok(None);
        };
        let Some((file, flags)) = owner.with_table(|table| table.get_with_flags(fd)) else {
            return Ok(None);
        };
        file.file_information(flags, view).map(Some)
    }

    /// proc Thread FDSize 读取自身 handle，退出已退休时返回0；不借用 Process leader 的表。
    pub(in crate::task) fn file_table_capacity(&self) -> usize {
        self.file_table()
            .map_or(0, |owner| owner.with_table(|files| files.slot_capacity()))
    }

    pub(super) fn retire_file_table(&self) {
        let retired = self.thread().files.lock().take();
        drop(retired);
    }

    pub(super) fn prepare_file_table_unshare(
        &self,
    ) -> Result<Option<Arc<FileTableOwner>>, MemoryError> {
        let current = self.file_table().expect("live Thread requires file table");
        // current snapshot 占一个引用；只有 caller 的 handle 占另一个时保留原 POSIX owner。
        if Arc::strong_count(&current) == 2 {
            return Ok(None);
        }
        let copied = current
            .with_table(|table| table.try_clone())
            .map_err(|_| MemoryError::OutOfMemory)?;
        FileTableOwner::try_new(copied).map(Some)
    }

    pub(super) fn commit_file_table_unshare(&self, prepared: Option<Arc<FileTableOwner>>) {
        if let Some(prepared) = prepared {
            let retired = self.thread().files.lock().replace(prepared);
            drop(retired);
        }
    }

    pub(crate) fn file_lock_owner(&self) -> crate::fs::RecordLockOwner {
        crate::fs::RecordLockOwner::new(
            self.file_table()
                .expect("live Thread requires file table")
                .identity(),
            self.tgid(),
        )
    }
}

// Process 生命周期内唯一投影；graph/proc observer 不保活任何 TCB。
impl super::Process {
    /// zombie leader 的 outer handle 仍属于实际 Process；退休为空目录，observer 不保活资源。
    pub(in crate::task) fn proc_file_descriptors(
        &self,
    ) -> Result<alloc::vec::Vec<crate::fs::ProcFileDescriptorSnapshot>, crate::fs::FileSystemError>
    {
        let leader = self.leader_files.lock().clone();
        let owner = leader.lock().clone();
        owner.map_or_else(
            || Ok(alloc::vec::Vec::new()),
            |owner| owner.proc_file_descriptors(),
        )
    }

    pub(super) fn with_process_files<R>(
        &self,
        operation: impl FnOnce(&mut FileDescriptorTable) -> R,
    ) -> Option<R> {
        let leader = self.leader_files.lock().clone();
        let owner = leader.lock().clone()?;
        Some(owner.with_table(operation))
    }
}
