//! Calling Thread 的可退休 namespace membership；namespace 内容仍属于各具体 lower owner。
use super::{TaskControlBlock, TaskKind};
use crate::{
    memory::MemoryError,
    namespace::{NamespaceError, TimeNamespace, UserNamespace, UtsNamespace},
};
use alloc::sync::Arc;
use spin::Mutex;

/// Membership tuple 可共享 concrete namespaces，但外层 handle 只属于一个 Thread。
/// user namespace 独占于 Credentials；不能在这里保留可变的第二份 user membership。
#[derive(Clone, Debug)]
pub(super) struct NamespaceMembership {
    pub(super) uts: Arc<UtsNamespace>,
    pub(super) ipc: Arc<crate::ipc::IpcNamespace>,
    pub(super) net: Arc<crate::socket::NetNamespace>,
    pub(super) mount: Arc<crate::fs::MountNamespace>,
    pub(super) time: Arc<TimeNamespace>,
    pub(super) time_for_children: Arc<TimeNamespace>,
    pub(super) pid_for_children: Arc<crate::namespace::PidNamespace>,
}
impl NamespaceMembership {
    /// nsproxy 内的标准 namespace selector；PID/user 仍由实际 PID/cred owner 投影。
    fn reference(
        &self,
        selector: crate::namespace::NamespaceSelector,
    ) -> Option<crate::namespace::NamespaceReference> {
        use crate::namespace::{NamespaceReference, NamespaceSelector, NamespaceType};
        Some(match selector {
            NamespaceSelector::PidForChildren => {
                NamespaceReference::new(self.pid_for_children.clone())
            }
            NamespaceSelector::TimeForChildren => {
                NamespaceReference::new(self.time_for_children.clone())
            }
            NamespaceSelector::Namespace(NamespaceType::Time) => {
                NamespaceReference::new(self.time.clone())
            }
            NamespaceSelector::Namespace(NamespaceType::Uts) => {
                NamespaceReference::new(self.uts.clone())
            }
            NamespaceSelector::Namespace(NamespaceType::Ipc) => {
                NamespaceReference::new(self.ipc.clone())
            }
            NamespaceSelector::Namespace(NamespaceType::Net) => {
                NamespaceReference::new(self.net.clone())
            }
            NamespaceSelector::Namespace(NamespaceType::Mount) => {
                NamespaceReference::new(self.mount.clone())
            }
            NamespaceSelector::Namespace(NamespaceType::User | NamespaceType::Pid) => return None,
        })
    }
}
/// task_lock 等价 owner：fs handle 与 namespace tuple 同时发布、同时捕获。
/// Process 仅别名同一 outer Arc；不增加 CLONE_FS inner strong_count。
#[derive(Debug)]
pub(super) struct FsNamespaceState {
    pub(super) fs: Option<Arc<Mutex<super::process_resources::FsContext>>>,
    pub(super) membership: Option<NamespaceMembership>,
}
pub(super) type FsNamespaceHandle = Arc<Mutex<FsNamespaceState>>;

/// 所有 outer allocation 在出版前完成；失败不修改原 Thread tuple。
pub(super) fn try_fs_namespace_handle(
    membership: NamespaceMembership,
    fs: Arc<Mutex<super::process_resources::FsContext>>,
) -> Result<FsNamespaceHandle, MemoryError> {
    Arc::try_new(Mutex::new(FsNamespaceState {
        fs: Some(fs),
        membership: Some(membership),
    }))
    .map_err(|_| MemoryError::OutOfMemory)
}
/// boot creator 的实际初始 namespace tuple；没有 fs/files/mm，也不另建 namespace 对象。
/// 用户初始 Thread 和 kernel worker 在创建时捕获同一批 concrete owners。
pub(super) fn initial_membership() -> NamespaceMembership {
    NamespaceMembership {
        uts: crate::namespace::initial_uts(),
        ipc: crate::ipc::initial_namespace(),
        net: crate::socket::initial_net(),
        mount: crate::fs::vfs().initial_namespace(),
        time: crate::namespace::initial_time(),
        time_for_children: crate::namespace::initial_time(),
        pid_for_children: crate::namespace::initial_pid(),
    }
}
pub(super) fn initial_fs_namespace_handle(
    fs: Arc<Mutex<super::process_resources::FsContext>>,
) -> Result<FsNamespaceHandle, MemoryError> {
    try_fs_namespace_handle(initial_membership(), fs)
}
/// clone/unshare 的未发布具体 namespace tuple；user membership 只在 Credentials 发布。
pub(crate) struct PreparedCloneNamespaces {
    membership: NamespaceMembership,
    user: Option<Arc<UserNamespace>>,
    fs: Option<super::process_resources::FsContext>,
}
/// Thread clone 的资源 tuple；syscall 校验 raw flags，constructor 只消费具体已准备 namespace。
pub(crate) struct ThreadCloneResources {
    pub(crate) share_fs: bool,
    pub(crate) share_files: bool,
    pub(crate) share_sem_undo: bool,
    pub(crate) namespaces: PreparedCloneNamespaces,
}
impl PreparedCloneNamespaces {
    pub(in crate::task) fn pid_for_children(&self) -> Arc<crate::namespace::PidNamespace> {
        self.membership.pid_for_children.clone()
    }
    pub(super) fn fs_context(
        &self,
        task: &TaskControlBlock,
        share: bool,
    ) -> Result<Arc<Mutex<super::process_resources::FsContext>>, MemoryError> {
        let Some(snapshot) = &self.fs else {
            return task.clone_fs_context(share);
        };
        assert!(!share, "NEWNS cannot share FS context");
        Arc::try_new(Mutex::new(snapshot.clone())).map_err(|_| MemoryError::OutOfMemory)
    }

    pub(super) fn handle(
        &self,
        fs: Arc<Mutex<super::process_resources::FsContext>>,
        share_memory: bool,
    ) -> Result<FsNamespaceHandle, MemoryError> {
        let mut membership = self.membership.clone();
        if !share_memory {
            membership.time = membership.time_for_children.clone();
            // copy_namespaces 的 timens_on_fork 同样在最终 fork publication 前冻结。
            membership.time.enter();
        }
        try_fs_namespace_handle(membership, fs)
    }
    pub(super) fn user(&self) -> Option<Arc<UserNamespace>> {
        self.user.clone()
    }
}

impl TaskControlBlock {
    /// namespace allocation 与权限只在 publication 前执行；NEWUSER 不允许从 chroot 内创建。
    #[allow(
        clippy::too_many_arguments,
        reason = "同一次原子 publication 的独立领域输入，参数语义与所有权已逐项说明"
    )]
    pub(crate) fn prepare_clone_namespaces(
        &self,
        new_user: bool,
        new_uts: bool,
        new_mount: bool,
        new_time: bool,
        new_pid: bool,
        new_ipc: bool,
        new_net: bool,
    ) -> Result<PreparedCloneNamespaces, NamespaceError> {
        let authority = self.namespace_authority();
        // copy_fs 先于 copy_namespaces；共享 sibling 的后续 chdir 不能吸入 clone 后才出现的旧 view。
        let mut frozen_fs = new_mount.then(|| self.fs_context_snapshot());
        let user = if new_user {
            let at_namespace_root = crate::fs::vfs()
                .is_namespace_root(&self.mount_namespace(), &self.lookup_origin().root());
            Some(UserNamespace::child(
                &authority,
                at_namespace_root,
                self.namespace_rlimits(),
            )?)
        } else {
            None
        };
        let mut membership = self.namespace_snapshot();
        if new_pid {
            if !Arc::ptr_eq(&self.pid_namespace(), &membership.pid_for_children) {
                return Err(NamespaceError::Invalid);
            }
            let owner = user.as_ref().unwrap_or(&authority.namespace).clone();
            if user.is_none()
                && !authority.capable(&owner, syscall_abi::capability::Capability::SYS_ADMIN)
            {
                return Err(NamespaceError::Permission);
            }
            membership.pid_for_children = membership
                .pid_for_children
                .child(owner, authority.effective_uid)?;
        }
        if new_ipc {
            let owner = user.as_ref().unwrap_or(&authority.namespace).clone();
            if user.is_none()
                && !authority.capable(&owner, syscall_abi::capability::Capability::SYS_ADMIN)
            {
                return Err(NamespaceError::Permission);
            }
            membership.ipc = crate::ipc::IpcNamespace::create(owner, authority.effective_uid)?;
        }
        if new_net {
            let owner = user.as_ref().unwrap_or(&authority.namespace).clone();
            if user.is_none()
                && !authority.capable(&owner, syscall_abi::capability::Capability::SYS_ADMIN)
            {
                return Err(NamespaceError::Permission);
            }
            membership.net = crate::socket::create_net_namespace(owner, authority.effective_uid)?;
        }
        if new_time {
            let owner = user.as_ref().unwrap_or(&authority.namespace).clone();
            if user.is_none()
                && !authority.capable(&owner, syscall_abi::capability::Capability::SYS_ADMIN)
            {
                return Err(NamespaceError::Permission);
            }
            membership.time_for_children = membership
                .time_for_children
                .copy_for(owner, authority.effective_uid)?;
        }
        if new_uts {
            let owner = user.as_ref().unwrap_or(&authority.namespace).clone();
            // NEWUSER 初始化完整能力；否则要求当前 owning user namespace 的 SYS_ADMIN。
            if user.is_none()
                && !authority.capable(&owner, syscall_abi::capability::Capability::SYS_ADMIN)
            {
                return Err(NamespaceError::Permission);
            }
            membership.uts = membership.uts.copy_for(owner, authority.effective_uid)?;
        }
        if let Some(snapshot) = &mut frozen_fs {
            let owner = user.as_ref().unwrap_or(&authority.namespace).clone();
            if user.is_none()
                && !authority.capable(&owner, syscall_abi::capability::Capability::SYS_ADMIN)
            {
                return Err(NamespaceError::Permission);
            }
            let origin = crate::fs::LookupOrigin::new(snapshot.root.clone(), snapshot.cwd.clone());
            let (copied, origin) =
                membership
                    .mount
                    .copy_for(owner, authority.effective_uid, origin)?;
            (snapshot.root, snapshot.cwd) = origin.into_parts();
            membership.mount = copied;
        }
        Ok(PreparedCloneNamespaces {
            membership,
            user,
            fs: frozen_fs,
        })
    }

    /// 单线程 NEWUSER 已由 graph 证明；全部 fallible fs/files/namespace 准备先于提交。
    #[allow(
        clippy::too_many_arguments,
        reason = "同一次原子 publication 的独立领域输入，参数语义与所有权已逐项说明"
    )]
    pub(crate) fn unshare_namespaces(
        &self,
        fs: bool,
        files: bool,
        new_user: bool,
        new_uts: bool,
        new_mount: bool,
        new_time: bool,
        new_pid: bool,
        new_ipc: bool,
        new_net: bool,
        sysv_sem: bool,
    ) -> Result<(), NamespaceError> {
        if new_user && !crate::task::process_table::is_single_threaded(self) {
            return Err(NamespaceError::Invalid);
        }
        let prepared = self.prepare_clone_namespaces(
            new_user, new_uts, new_mount, new_time, new_pid, new_ipc, new_net,
        )?;
        let detach_fs = (fs || new_user)
            && Arc::strong_count(
                self.thread()
                    .fs_namespaces
                    .lock()
                    .fs
                    .as_ref()
                    .expect("live Thread requires fs context"),
            ) > 1;
        let detached_fs = if detach_fs || new_mount {
            Some(
                prepared
                    .fs_context(self, false)
                    .map_err(|_| NamespaceError::OutOfMemory)?,
            )
        } else {
            None
        };
        let detached_files = if files {
            self.prepare_file_table_unshare()
                .map_err(|_| NamespaceError::OutOfMemory)?
        } else {
            None
        };
        let detached_undo = if new_ipc || sysv_sem {
            Some(crate::ipc::SemaphoreUndoHandle::new().map_err(|_| NamespaceError::OutOfMemory)?)
        } else {
            None
        };
        // 全部 preparation 已成功，先完成旧 namespace 的 SEM_UNDO，后续 publication 不再失败。
        if let Some(next) = detached_undo {
            self.replace_semaphore_undo(next);
        }
        self.commit_file_table_unshare(detached_files);
        if let Some(user) = prepared.user {
            self.enter_user_namespace(user);
        }
        let (retired_fs, retired) = {
            let mut state = self.thread().fs_namespaces.lock();
            let retired_fs = detached_fs.and_then(|next| state.fs.replace(next));
            let retired = state.membership.replace(prepared.membership);
            (retired_fs, retired)
        };
        drop(retired);
        drop(retired_fs);
        Ok(())
    }

    /// 标准 setns；所有权限/共享约束先于唯一 membership/credential owner 替换。
    pub(crate) fn set_namespace(
        &self,
        target: crate::namespace::NamespaceReference,
    ) -> Result<(), NamespaceError> {
        use crate::namespace::NamespaceType;
        let authority = self.namespace_authority();
        match target.kind() {
            NamespaceType::Net => {
                let net = target.downcast::<crate::socket::NetNamespace>()?;
                if !authority.capable(
                    &authority.namespace,
                    syscall_abi::capability::Capability::SYS_ADMIN,
                ) || !authority.capable(
                    &net.user_namespace(),
                    syscall_abi::capability::Capability::SYS_ADMIN,
                ) {
                    return Err(NamespaceError::Permission);
                }
                let retired = {
                    let mut state = self.thread().fs_namespaces.lock();
                    core::mem::replace(
                        &mut state
                            .membership
                            .as_mut()
                            .expect("live namespace membership")
                            .net,
                        net,
                    )
                };
                drop(retired);
            }
            NamespaceType::Ipc => {
                let ipc = target.downcast::<crate::ipc::IpcNamespace>()?;
                if !authority.capable(
                    &authority.namespace,
                    syscall_abi::capability::Capability::SYS_ADMIN,
                ) || !authority.capable(
                    &ipc.user_namespace(),
                    syscall_abi::capability::Capability::SYS_ADMIN,
                ) {
                    return Err(NamespaceError::Permission);
                }
                let undo = crate::ipc::SemaphoreUndoHandle::new()
                    .map_err(|_| NamespaceError::OutOfMemory)?;
                self.replace_semaphore_undo(undo);
                let retired = {
                    let mut state = self.thread().fs_namespaces.lock();
                    core::mem::replace(
                        &mut state
                            .membership
                            .as_mut()
                            .expect("live namespace membership")
                            .ipc,
                        ipc,
                    )
                };
                drop(retired);
            }
            NamespaceType::Pid => {
                let pid = target.downcast::<crate::namespace::PidNamespace>()?;
                if !authority.capable(
                    &authority.namespace,
                    syscall_abi::capability::Capability::SYS_ADMIN,
                ) || !authority.capable(
                    &pid.user_namespace(),
                    syscall_abi::capability::Capability::SYS_ADMIN,
                ) {
                    return Err(NamespaceError::Permission);
                }
                if !pid.is_descendant_of(&self.pid_namespace()) {
                    return Err(NamespaceError::Invalid);
                }
                let retired = {
                    let mut state = self.thread().fs_namespaces.lock();
                    core::mem::replace(
                        &mut state
                            .membership
                            .as_mut()
                            .expect("live namespace membership")
                            .pid_for_children,
                        pid,
                    )
                };
                drop(retired);
            }
            NamespaceType::Time => {
                let time = target.downcast::<TimeNamespace>()?;
                if !crate::task::process_table::is_single_threaded(self) {
                    return Err(NamespaceError::Multithreaded);
                }
                if !authority.capable(
                    &authority.namespace,
                    syscall_abi::capability::Capability::SYS_ADMIN,
                ) || !authority.capable(
                    &time.user_namespace(),
                    syscall_abi::capability::Capability::SYS_ADMIN,
                ) {
                    return Err(NamespaceError::Permission);
                }
                time.enter();
                let retired = {
                    let mut state = self.thread().fs_namespaces.lock();
                    let membership = state
                        .membership
                        .as_mut()
                        .expect("live namespace membership");
                    (
                        core::mem::replace(&mut membership.time, time.clone()),
                        core::mem::replace(&mut membership.time_for_children, time),
                    )
                };
                drop(retired);
            }
            NamespaceType::User => {
                let user = target.downcast::<UserNamespace>()?;
                if Arc::ptr_eq(&authority.namespace, &user)
                    || !crate::task::process_table::is_single_threaded(self)
                    || Arc::strong_count(
                        self.thread()
                            .fs_namespaces
                            .lock()
                            .fs
                            .as_ref()
                            .expect("live Thread requires fs context"),
                    ) != 1
                {
                    return Err(NamespaceError::Invalid);
                }
                if !authority.capable(&user, syscall_abi::capability::Capability::SYS_ADMIN) {
                    return Err(NamespaceError::Permission);
                }
                self.enter_user_namespace(user);
            }
            NamespaceType::Mount => {
                let mount = target.downcast::<crate::fs::MountNamespace>()?;
                if !authority.capable(
                    &authority.namespace,
                    syscall_abi::capability::Capability::SYS_ADMIN,
                ) || !authority.capable(
                    &authority.namespace,
                    syscall_abi::capability::Capability::SYS_CHROOT,
                ) || !authority.capable(
                    &mount.user_namespace(),
                    syscall_abi::capability::Capability::SYS_ADMIN,
                ) {
                    return Err(NamespaceError::Permission);
                }
                if Arc::strong_count(
                    self.thread()
                        .fs_namespaces
                        .lock()
                        .fs
                        .as_ref()
                        .expect("live fs context"),
                ) != 1
                {
                    return Err(NamespaceError::Invalid);
                }
                let root = crate::fs::vfs()
                    .namespace_root(&mount)
                    .map_err(|_| NamespaceError::OutOfMemory)?;
                let (retired_namespace, retired_root, retired_cwd) = {
                    let mut state = self.thread().fs_namespaces.lock();
                    let fs = state.fs.as_ref().expect("live fs context");
                    if Arc::strong_count(fs) != 1 {
                        return Err(NamespaceError::Invalid);
                    }
                    let (retired_root, retired_cwd) = {
                        let mut fs = fs.lock();
                        (
                            core::mem::replace(&mut fs.root, root.clone()),
                            core::mem::replace(&mut fs.cwd, root),
                        )
                    };
                    let retired_namespace = core::mem::replace(
                        &mut state
                            .membership
                            .as_mut()
                            .expect("live namespace membership")
                            .mount,
                        mount,
                    );
                    (retired_namespace, retired_root, retired_cwd)
                };
                drop(retired_cwd);
                drop(retired_root);
                drop(retired_namespace);
            }
            NamespaceType::Uts => {
                let uts = target.downcast::<UtsNamespace>()?;
                if !authority.capable(
                    &authority.namespace,
                    syscall_abi::capability::Capability::SYS_ADMIN,
                ) || !authority.capable(
                    &uts.user_namespace(),
                    syscall_abi::capability::Capability::SYS_ADMIN,
                ) {
                    return Err(NamespaceError::Permission);
                }
                let retired = {
                    let mut live = self.thread().fs_namespaces.lock();
                    core::mem::replace(
                        &mut live
                            .membership
                            .as_mut()
                            .expect("live namespace membership")
                            .uts,
                        uts,
                    )
                };
                drop(retired);
            }
        }
        Ok(())
    }

    pub(super) fn namespace_snapshot(&self) -> NamespaceMembership {
        self.thread()
            .fs_namespaces
            .lock()
            .membership
            .as_ref()
            .expect("live Thread requires namespace membership")
            .clone()
    }
    /// Read-only callers 只 pin actual namespace，不借 Process 或其他 Thread fallback。
    pub(crate) fn uts_namespace(&self) -> Arc<UtsNamespace> {
        self.thread()
            .fs_namespaces
            .lock()
            .membership
            .as_ref()
            .expect("live Thread requires namespace membership")
            .uts
            .clone()
    }
    /// Calling Thread 的 actual IPC object owner；syscall pins 此 namespace，不按 fd 或显示 ID 重解析。
    pub(crate) fn ipc_namespace(&self) -> Arc<crate::ipc::IpcNamespace> {
        self.thread()
            .fs_namespaces
            .lock()
            .membership
            .as_ref()
            .expect("live namespace membership")
            .ipc
            .clone()
    }
    /// Calling Thread 的唯一网络域 association；旧 socket 保留自己的对象引用。
    pub(crate) fn net_namespace(&self) -> Arc<crate::socket::NetNamespace> {
        self.thread()
            .fs_namespaces
            .lock()
            .membership
            .as_ref()
            .expect("live namespace membership")
            .net
            .clone()
    }
    /// proc/PIDFD 在实际 execution variant 的唯一 nsproxy owner 下取一次投影。
    /// guard 内只提升被选择的 namespace Arc；没有可用 membership 时不借初始域或 sibling。
    fn with_proc_namespaces<R>(
        &self,
        project: impl FnOnce(&NamespaceMembership) -> R,
    ) -> Option<R> {
        match &self.kind {
            TaskKind::User { .. } => self
                .thread()
                .fs_namespaces
                .lock()
                .membership
                .as_ref()
                .map(project),
            TaskKind::Kernel(thread) => thread.namespaces.lock().as_ref().map(project),
        }
    }
    /// proc NET lookup 捕获实际 target 的 nsproxy；已退出执行体返回 None。
    pub(in crate::task) fn proc_net_namespace(&self) -> Option<Arc<crate::socket::NetNamespace>> {
        self.with_proc_namespaces(|membership| membership.net.clone())
    }
    pub(crate) fn mount_namespace(&self) -> Arc<crate::fs::MountNamespace> {
        self.thread()
            .fs_namespaces
            .lock()
            .membership
            .as_ref()
            .expect("live Thread requires namespace membership")
            .mount
            .clone()
    }
    /// 时钟与 timer replacement 只捕获固定 scalar，不让 task membership 变成 timer 的 owner。
    pub(crate) fn time_offsets(&self) -> crate::namespace::TimeOffsets {
        self.thread()
            .fs_namespaces
            .lock()
            .membership
            .as_ref()
            .expect("live Thread requires namespace membership")
            .time
            .snapshot()
    }
    pub(in crate::task) fn proc_time_for_children(&self) -> Option<Arc<TimeNamespace>> {
        self.with_proc_namespaces(|membership| membership.time_for_children.clone())
    }
    /// exec 进入 for_children；只在新映像提交已不可失败、siblings 已退休后调用。
    pub(super) fn enter_exec_time_namespace(&self) {
        let retired = {
            let mut state = self.thread().fs_namespaces.lock();
            let membership = state
                .membership
                .as_mut()
                .expect("exec requires namespace membership");
            membership.time_for_children.enter();
            core::mem::replace(&mut membership.time, membership.time_for_children.clone())
        };
        drop(retired);
    }
    /// Linux proc mounts open 在同一 task resource lock 下捕获 target namespace 与 target fs root。
    pub(in crate::task) fn proc_mount_snapshot(
        &self,
    ) -> Option<(Arc<crate::fs::MountNamespace>, Arc<crate::fs::OpenedFile>)> {
        if self.is_kernel_thread() {
            return None;
        }
        let state = self.thread().fs_namespaces.lock();
        let membership = state.membership.as_ref()?;
        let root = state.fs.as_ref()?.lock().root.clone();
        Some((membership.mount.clone(), root))
    }
    pub(in crate::task) fn proc_namespace(
        &self,
        selector: crate::namespace::NamespaceSelector,
    ) -> Option<crate::namespace::NamespaceReference> {
        use crate::namespace::{NamespaceReference, NamespaceSelector, NamespaceType};
        match selector {
            // PID 与 user 属实际 PID/cred，不随 exit_task_namespaces 退休。
            NamespaceSelector::Namespace(NamespaceType::Pid) => {
                Some(NamespaceReference::new(self.pid_namespace()))
            }
            NamespaceSelector::Namespace(NamespaceType::User) => {
                Some(NamespaceReference::new(self.user_namespace()))
            }
            _ => self
                .with_proc_namespaces(|membership| membership.reference(selector))
                .flatten(),
        }
    }
    /// PIDFS namespace ioctl 在目标仍有实际 nsproxy membership 时捕获 namespace，再按 FSCREDS 授权。
    /// 与 proc 共享具体 owner，不重建 namespace 或保活已退出 Thread 的旧 membership。
    pub(in crate::task) fn pidfd_namespace(
        &self,
        caller: &TaskControlBlock,
        selector: Option<crate::namespace::NamespaceSelector>,
    ) -> Result<crate::namespace::NamespaceReference, crate::fs::device::DeviceError> {
        use crate::fs::device::DeviceError;
        use syscall_abi::errno;
        // 固定 Linux pidfd_get_namespace 在 task_lock 下捕获一次 nsproxy，再释放锁授权。
        // cred/PID 引用先在各自 owner 下提升，随后与实际 nsproxy 是否存活一起线性化。
        let scalar = match selector {
            Some(crate::namespace::NamespaceSelector::Namespace(
                crate::namespace::NamespaceType::User | crate::namespace::NamespaceType::Pid,
            )) => selector.and_then(|selector| self.proc_namespace(selector)),
            _ => None,
        };
        let namespace = self
            .with_proc_namespaces(|membership| {
                scalar.or_else(|| selector.and_then(|selector| membership.reference(selector)))
            })
            .ok_or(DeviceError::Errno(errno::ESRCH))?;
        if !caller.may_inspect_thread(self) {
            return Err(DeviceError::Errno(errno::EACCES));
        }
        namespace.ok_or(DeviceError::Errno(if selector.is_some() {
            errno::ESRCH
        } else {
            errno::EOPNOTSUPP
        }))
    }
    pub(super) fn retire_namespaces(&self) {
        let retired = self.thread().fs_namespaces.lock().membership.take();
        drop(retired);
    }
}
