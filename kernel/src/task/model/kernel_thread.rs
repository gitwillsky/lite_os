//! 内核执行体的真实 PID membership 与一次性主体；不拥有用户 Process。
use super::{Credentials, KernelThreadBody, ProcessId};
use crate::namespace::{PidIdentity, PidRegistration};
use crate::sync::TaskMutexWaitPreparation;
use alloc::sync::Arc;
use spin::Mutex;

pub(super) struct KernelThread {
    // OWNER: boot-created kernel entity 的真实 immutable 初始凭据；无用户 credential transitions。
    pub(super) credentials: Credentials,
    // OWNER: native nsproxy 的实际 concrete owners；proc observer 不保活已退出的 membership。
    // 无 fs/files/mm，Mutex 只允许读取或退出时 take；退休 tuple 在 guard 外析构。
    pub(super) namespaces: Mutex<Option<super::namespaces::NamespaceMembership>>,
    pub(super) name: &'static str,
    // OWNER: TGID 只 pin 不变 identity；编号占用由 pid registration 独占，真实 exit 显式 take。
    tgid: ProcessId,
    pid: Mutex<Option<PidRegistration>>,
    pub(in crate::task) pidfd_source: Mutex<alloc::sync::Weak<crate::fs::pidfs::PidFdSource>>,
    // OWNER: 首次调度消费一次，返回即进入实际 exit；不能由 continuation 重入。
    body: Mutex<Option<KernelThreadBody>>,
    // OWNER: 内核线程代表用户任务访问其 fd 表（io_uring worker）时使用的预置 waiter，语义同
    // `ThreadContext::file_wait`；缺失时表查找没有可用的 waiter，只能 panic。
    pub(super) file_wait: Mutex<Option<TaskMutexWaitPreparation>>,
}
impl KernelThread {
    pub(super) fn new(
        pid: PidRegistration,
        name: &'static str,
        body: KernelThreadBody,
        namespaces: super::namespaces::NamespaceMembership,
        file_wait: TaskMutexWaitPreparation,
    ) -> Self {
        Self {
            credentials: Credentials::root(),
            namespaces: Mutex::new(Some(namespaces)),
            name,
            tgid: ProcessId::from_identity(pid.identity().clone()),
            pid: Mutex::new(Some(pid)),
            pidfd_source: Mutex::new(alloc::sync::Weak::new()),
            body: Mutex::new(Some(body)),
            file_wait: Mutex::new(Some(file_wait)),
        }
    }
    pub(super) fn identity(&self) -> &Arc<PidIdentity> {
        self.tgid.identity()
    }
    pub(super) fn take_body(&self) -> KernelThreadBody {
        self.body
            .lock()
            .take()
            .unwrap_or_else(|| panic!("kernel thread {} body started twice", self.name))
    }
    /// 主体返回后、撤销 Processor current 前调用；observer 不延长实际编号占用。
    pub(super) fn retire_pid(&self) {
        let namespaces = self
            .namespaces
            .lock()
            .take()
            .expect("kernel nsproxy retired twice");
        drop(namespaces);
        let retired = self.pid.lock().take().expect("kernel PID retired twice");
        drop(retired);
    }
}
