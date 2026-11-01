//! linuxd 后台线程在 Linux 视图中的身份（`/proc` 中的内核线程）：真实 PID 与一次性主体，
//! 不拥有用户进程。主体在承载它的服务线程上运行，返回即退出。
use super::{Credentials, KernelThreadBody, ProcessId};
use crate::namespace::{PidIdentity, PidRegistration};
use crate::sync::{Mutex, TaskMutexWaitPreparation};
use alloc::sync::Arc;

pub(super) struct KernelThread {
    // OWNER: 内核线程的不可变初始凭据。
    pub(super) credentials: Credentials,
    // OWNER: nsproxy 的实际成员；退出时取走并在锁外析构。
    pub(super) namespaces: Mutex<Option<super::namespaces::NamespaceMembership>>,
    pub(super) name: &'static str,
    // OWNER: TGID 只固定不变的身份；编号占用由 pid registration 独占，退出时取走。
    tgid: ProcessId,
    pid: Mutex<Option<PidRegistration>>,
    pub(in crate::task) pidfd_source: Mutex<alloc::sync::Weak<crate::fs::pidfs::PidFdSource>>,
    // OWNER: 首次运行时取走一次。
    body: Mutex<Option<KernelThreadBody>>,
    // OWNER: 代表用户任务访问其 fd 表（io_uring worker）时的预置等待者，语义同 `ThreadContext::file_wait`。
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

    /// 主体返回后归还 PID 与 nsproxy。
    pub(super) fn retire_pid(&self) {
        let namespaces = self.namespaces.lock().take().expect("kernel nsproxy retired twice");
        drop(namespaces);
        let retired = self.pid.lock().take().expect("kernel PID retired twice");
        drop(retired);
    }
}
