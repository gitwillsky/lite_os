use alloc::sync::Arc;

use super::TaskControlBlock;
use crate::{devices::tty::Terminal, fs::OpenedFile, memory::MemoryError};
use spin::Mutex;

pub(super) const DEFAULT_UMASK: u32 = 0o022;

/// Linux CLONE_FS 共享的 root/cwd/umask owner；每个 Thread handle 可独立脱离。
#[derive(Clone)]
pub(super) struct FsContext {
    // OWNER: FsContext 独占 VFS opened cwd identity；只保存 inode 会使 rename 后的
    // getcwd 与相对 lookup 分裂。
    pub(super) cwd: Arc<OpenedFile>,
    // OWNER: absolute lookup/symlink 与 dotdot 的 caller root；与 cwd 同锁快照。
    pub(super) root: Arc<OpenedFile>,
    // OWNER: umask 属于共享 fs context；raw setuid/setgroups 的线程凭据不拥有它。
    pub(super) umask: u32,
}

impl core::fmt::Debug for FsContext {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("FsContext")
            .field("umask", &self.umask)
            .finish_non_exhaustive()
    }
}

impl TaskControlBlock {
    /// 同锁取得 fs root/cwd；VFS 不得自行查询 current Task 或补回 global root。
    pub(crate) fn lookup_origin(&self) -> crate::fs::LookupOrigin {
        let handle = self.thread().fs_namespaces.lock();
        let context = handle
            .fs
            .as_ref()
            .expect("live Thread requires fs context")
            .lock();
        crate::fs::LookupOrigin::new(context.root.clone(), context.cwd.clone())
    }

    /// proc root/cwd snapshot；已退休的 main Thread 不得重新通过 worker 恢复 fs handle。
    pub(in crate::execution::task) fn proc_directory(
        &self,
        kind: crate::fs::ProcDirectory,
    ) -> Option<Arc<OpenedFile>> {
        if self.is_kernel_thread() {
            return None;
        }
        let handle = self.thread().fs_namespaces.lock();
        let context = handle.fs.as_ref()?.lock();
        Some(match kind {
            crate::fs::ProcDirectory::Root => context.root.clone(),
            crate::fs::ProcDirectory::Cwd => context.cwd.clone(),
        })
    }

    /// chroot 只替换 root，保留 cwd/旧 dirfd；最终 opened-entry Drop 在 owner 锁外。
    pub(crate) fn set_root_directory(&self, opened: Arc<OpenedFile>) {
        let retired = {
            let handle = self.thread().fs_namespaces.lock();
            core::mem::replace(
                &mut handle
                    .fs
                    .as_ref()
                    .expect("live Thread requires fs context")
                    .lock()
                    .root,
                opened,
            )
        };
        drop(retired);
    }

    /// 原子替换共享 fs context 的工作目录 identity，旧 opened-entry 锁外退休。
    ///
    /// # Parameters
    ///
    /// - `opened`: 已由 VFS 证明为目录的 opened entry。
    pub(crate) fn set_working_directory(&self, opened: Arc<OpenedFile>) {
        let retired = {
            let handle = self.thread().fs_namespaces.lock();
            core::mem::replace(
                &mut handle
                    .fs
                    .as_ref()
                    .expect("live Thread requires fs context")
                    .lock()
                    .cwd,
                opened,
            )
        };
        drop(retired);
    }

    /// 在共享 fs context 中替换创建 mask，返回旧 mask。
    pub(crate) fn replace_umask(&self, mask: u32) -> u32 {
        let handle = self.thread().fs_namespaces.lock();
        core::mem::replace(
            &mut handle
                .fs
                .as_ref()
                .expect("live Thread requires fs context")
                .lock()
                .umask,
            mask & 0o777,
        )
    }

    /// 按当前共享 fs context 的 mask 投影创建 mode。
    pub(crate) fn creation_mode(&self, mode: u32) -> u32 {
        let handle = self.thread().fs_namespaces.lock();
        mode & !handle
            .fs
            .as_ref()
            .expect("live Thread requires fs context")
            .lock()
            .umask
            & 0o7777
    }

    /// 在 constructor 发布前按 CLONE_FS 准备共享或独立 fs owner；分配不持 identity 锁。
    pub(super) fn clone_fs_context(
        &self,
        share: bool,
    ) -> Result<Arc<Mutex<FsContext>>, MemoryError> {
        if share {
            let state = self.thread().fs_namespaces.lock();
            return Ok(state.fs.as_ref().expect("live fs context").clone());
        }
        let copied = self.fs_context_snapshot();
        Arc::try_new(Mutex::new(copied)).map_err(|_| MemoryError::OutOfMemory)
    }

    /// 只复制 immutable path pins 与 umask，不额外增加 FsContext 的实际 CLONE_FS 用户数。
    pub(super) fn fs_context_snapshot(&self) -> FsContext {
        let state = self.thread().fs_namespaces.lock();
        state.fs.as_ref().expect("live fs context").lock().clone()
    }

    /// 退出完成前退休 caller fs handle；外部 TCB/wake Arc 不能延长 cwd/mount ownership。
    pub(super) fn retire_fs_context(&self) {
        let retired = self.thread().fs_namespaces.lock().fs.take();
        drop(retired);
    }

    /// 返回当前 Process 可继承的 controlling Terminal identity。
    ///
    /// # Returns
    ///
    /// 与 `/dev/tty` 当前解析目标相同的 Arc。
    pub(in crate::execution::task) fn terminal(&self) -> Option<Arc<Terminal>> {
        self.process().terminal.lock().clone()
    }
}

// Process 生命周期内唯一投影；graph/proc observer 不保活任何 TCB。
impl super::Process {
    /// graph 拥有 session 事务；只交换实际 controlling handle，旧资源由 caller 在 graph 外释放。
    pub(in crate::execution::task) fn replace_terminal(
        &self,
        next: Option<Arc<Terminal>>,
    ) -> Option<Arc<Terminal>> {
        core::mem::replace(&mut *self.terminal.lock(), next)
    }

    /// caller 保持 target Terminal 的有效 borrow/pin；只摘同一对象的 handle，Arc 不可能最后释放。
    pub(in crate::execution::task) fn clear_terminal_if(&self, expected: &Terminal) {
        let retired = {
            let mut owner = self.terminal.lock();
            if owner
                .as_ref()
                .is_some_and(|terminal| core::ptr::eq(terminal.as_ref(), expected))
            {
                owner.take()
            } else {
                None
            }
        };
        drop(retired);
    }

    pub(in crate::execution::task) fn terminal_proc_identity(
        &self,
        view: &crate::namespace::PidNamespace,
    ) -> (u32, isize) {
        self.terminal
            .lock()
            .as_ref()
            .map_or((0, -1), |terminal| terminal.proc_identity(view))
    }
}
