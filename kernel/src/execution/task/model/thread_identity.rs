//! Thread 当前执行 identity 与 Process TGID 的投影；编号归 PID namespace，graph links 决定占用。
use super::{Arc, TaskControlBlock, TaskKind};
use crate::{execution::task::pid::PidIdentity, namespace::PidNamespace};

impl super::Process {
    /// 只借用该 Process 的同一 TGID observer，供退出 leader 的组信号 view 投影。
    pub(in crate::execution::task) fn tgid_identity(&self) -> &Arc<PidIdentity> {
        self.tgid.identity()
    }
}

impl TaskControlBlock {
    /// 内核 graph/scheduler 的稳定 identity key；只在 exec survivor promotion 时改指原 TGID。
    pub(crate) fn tid(&self) -> usize {
        self.execution.identity.lock().key()
    }
    pub(crate) fn tgid(&self) -> usize {
        if self.is_kernel_thread() {
            self.tid()
        } else {
            self.process().tgid.key()
        }
    }
    /// 当前 TGID identity；用户 syscall 与内核资源退休共用，不增加 PID registration。
    pub(crate) fn tgid_identity(&self) -> &Arc<PidIdentity> {
        match &self.kind {
            TaskKind::User { process, .. } => process.tgid.identity(),
            TaskKind::Kernel(thread) => thread.identity(),
        }
    }
    pub(crate) fn pid_namespace(&self) -> Arc<PidNamespace> {
        self.execution.identity.lock().namespace()
    }
    /// Linux 正编号解析为稳定 graph key；namespace association 存在不保证目标当前 live。
    pub(crate) fn resolve_pid(&self, number: usize) -> Option<usize> {
        let number = u32::try_from(number).ok().filter(|number| *number != 0)?;
        self.pid_namespace()
            .lookup(number)
            .map(|key| key.identity())
    }
    /// graph snapshot pin 当前实际 TID；observer 不延迟编号回收。
    pub(crate) fn tid_identity(&self) -> Arc<PidIdentity> {
        self.execution.identity.lock().clone()
    }
    pub(in crate::execution::task) fn is_namespace_init(&self) -> bool {
        !self.is_kernel_thread() && self.tgid_identity().is_namespace_init()
    }
    /// calling Thread 可见 PID/TID；namespace 编号不会被拿来索引 kernel graph。
    pub(crate) fn visible_tid(&self) -> u32 {
        let current = self.execution.identity.lock();
        current.number_in(&current.namespace())
    }
    pub(crate) fn visible_tgid(&self) -> u32 {
        let namespace = self.pid_namespace();
        self.tgid_identity().number_in(&namespace)
    }
    /// 必须在 graph 无失败重发布两个 Thread index 的同一 guard 下调用；旧 pin 锁外退休。
    pub(in crate::execution::task) fn promote_exec_identity(&self) -> Arc<PidIdentity> {
        assert!(!self.is_kernel_thread());
        let mut current = self.execution.identity.lock();
        core::mem::replace(&mut *current, self.process().tgid.identity().clone())
    }
}
