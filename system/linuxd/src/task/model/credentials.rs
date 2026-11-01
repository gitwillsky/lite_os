use alloc::{sync::Arc, vec::Vec};

use super::{
    TaskControlBlock,
    capabilities::{CapabilityExecIds, CapabilityState},
    thread_mm::MmMembership,
};
use crate::fs::AccessIdentity;
use syscall_abi::capability::Capability;

/// 原 leader owner 的可重定向只读 alias；Process/graph 共用同一 handle。
pub(in crate::execution::task) type LeaderCredentialsHandle =
    Arc<spin::Mutex<Arc<spin::Mutex<Credentials>>>>;

/// SIGCHLD 在 graph 的 generation 事务中读取原 leader 的 canonical real UID。
pub(in crate::execution::task) fn leader_signal_uid(handle: &LeaderCredentialsHandle) -> u32 {
    handle.lock().lock().real_uid
}
/// PIDFS 的退休 leader 查询捕获同一个原凭据 tuple；不借新 exec survivor 的 TCB。
pub(in crate::execution::task) fn leader_credential_snapshot(
    handle: &LeaderCredentialsHandle,
) -> crate::fs::ProcCredentialSnapshot {
    handle.lock().lock().snapshot()
}

const ROOT_ID: u32 = 0;
const UNCHANGED_ID: u32 = u32::MAX;

impl super::Process {
    /// 实际原 leader credential owner 的 UserNS；组信号投影不借 worker 的凭据。
    pub(in crate::execution::task) fn leader_user_namespace(
        &self,
    ) -> Arc<crate::namespace::UserNamespace> {
        self.leader_credentials.lock().lock().user_namespace.clone()
    }
}

/// 同一 ptrace 读取 policy 的原生凭据口径；proc 使用 FS，robust-list query 使用 REAL。
enum InspectionCredentials {
    Filesystem,
    Real,
}

/// Calling Thread 唯一拥有的 Linux 用户与组；group 值不可变，clone 只共享其内容。
#[derive(Debug, Clone)]
pub(in crate::execution::task) struct Credentials {
    real_uid: u32,
    effective_uid: u32,
    saved_uid: u32,
    fs_uid: u32,
    real_gid: u32,
    effective_gid: u32,
    saved_gid: u32,
    fs_gid: u32,
    // OWNER: immutable Arc 让 pathname permission snapshot 和 fork 只增加引用计数；
    // 如果保留 Vec clone，每个 pathname syscall 都可在 OOM 时 abort kernel。
    groups: Option<Arc<Vec<u32>>>,
    pub(super) capabilities: CapabilityState,
    // OWNER: credentials 的 user namespace；同一 Thread namespace 与 cap/IDs 原子发布。
    pub(super) user_namespace: Arc<crate::namespace::UserNamespace>,
}

impl TaskControlBlock {
    /// setns/unshare 使用唯一 credential transition；进入 namespace 重置 capability 与 securebits。
    pub(super) fn enter_user_namespace(&self, namespace: Arc<crate::namespace::UserNamespace>) {
        self.commit_identity_change(|next| {
            next.user_namespace = namespace;
            next.capabilities = CapabilityState::root();
            Ok(0)
        })
        .expect("prepared namespace credential commit cannot fail");
    }

    /// Calling Thread 的 namespace capability authority；global kernel IDs 不在此改成显示 ID。
    pub(crate) fn namespace_authority(&self) -> crate::namespace::NamespaceAuthority {
        match &self.kind {
            super::TaskKind::Kernel(thread) => thread.credentials.namespace_authority(),
            super::TaskKind::User { .. } => self.thread().credentials.lock().namespace_authority(),
        }
    }

    /// IPC EUID/EGID、fsgid、groups 和 capability authority 取同一真实 credential 快照。
    pub(crate) fn ipc_access(&self) -> crate::ipc::IpcAccess {
        self.ipc_access_with_real_uid().0
    }
    /// SHM_LOCK 的 ucounts 起点使用 real UID，与 DAC 的 EUID 必须来自同一凭据快照。
    pub(crate) fn ipc_access_with_real_uid(&self) -> (crate::ipc::IpcAccess, u32) {
        let credentials = self.thread().credentials.lock();
        (
            crate::ipc::IpcAccess {
                authority: credentials.namespace_authority(),
                filesystem_gid: credentials.fs_gid,
                groups: credentials.groups.clone(),
            },
            credentials.real_uid,
        )
    }

    /// 同一次真实 credential 快照的 ucounts 起点；不在两个锁事务间分别读取 UserNS/real UID。
    pub(crate) fn user_resource_owner(&self) -> (Arc<crate::namespace::UserNamespace>, u32) {
        let credentials = self.thread().credentials.lock();
        (credentials.user_namespace.clone(), credentials.real_uid)
    }

    /// namespace identity snapshot 不保活 TCB/MM；仅 pin immutable ancestry/map owner。
    pub(crate) fn user_namespace(&self) -> Arc<crate::namespace::UserNamespace> {
        match &self.kind {
            super::TaskKind::Kernel(thread) => thread.credentials.user_namespace.clone(),
            super::TaskKind::User { .. } => self.thread().credentials.lock().user_namespace.clone(),
        }
    }

    /// 取得一次权限判断身份快照。
    ///
    /// # Parameters
    ///
    /// - `effective`: true 选择 filesystem ID，false 选择 real ID。
    ///
    /// # Returns
    ///
    /// 包含 supplementary groups 的独立快照。
    pub(crate) fn access_identity(&self, effective: bool) -> AccessIdentity {
        self.thread().credentials.lock().access_identity(effective)
    }

    /// 读取 real/effective UID 或 GID。
    pub(crate) fn credential_id(&self, uid: bool, effective: bool) -> u32 {
        let credentials = self.thread().credentials.lock();
        if uid {
            credentials.uid(effective)
        } else {
            credentials.gid(effective)
        }
    }

    /// 同一个实际 owner guard 冻结消息/peer credential 所需的三组 ID 与 capability authority。
    pub(crate) fn message_identity(&self) -> super::MessageIdentity {
        match &self.kind {
            super::TaskKind::Kernel(thread) => thread.credentials.message_identity(),
            super::TaskKind::User { .. } => self.thread().credentials.lock().message_identity(),
        }
    }

    /// 系统调用输出使用当前 user namespace；内部权限、计数与 SCM 仍使用 canonical ID。
    pub(crate) fn visible_credential_id(&self, uid: bool, effective: bool) -> u32 {
        let credentials = self.thread().credentials.lock();
        credentials.user_namespace.display_id(
            uid,
            if uid {
                credentials.uid(effective)
            } else {
                credentials.gid(effective)
            },
        )
    }

    /// 一次 credential 快照投影整个三元组，不能把不同 publication 的 namespace/IDs 混用。
    pub(crate) fn visible_credential_res_ids(&self, uid: bool) -> [u32; 3] {
        let credentials = self.thread().credentials.lock();
        let ids = if uid {
            credentials.resuids()
        } else {
            credentials.resgids()
        };
        ids.map(|id| credentials.user_namespace.display_id(uid, id))
    }

    /// 读取 real/effective/saved UID 或 GID 三元组。
    #[cfg(test)]
    pub(crate) fn credential_res_ids(&self, uid: bool) -> [u32; 3] {
        let credentials = self.thread().credentials.lock();
        if uid {
            credentials.resuids()
        } else {
            credentials.resgids()
        }
    }

    /// Thread-directed signal 使用指定 Thread 的身份；同线程组按 Linux 免 UID 检查。
    pub(crate) fn may_signal_thread(&self, target: &TaskControlBlock) -> bool {
        self.tgid() == target.tgid() || {
            let (ids, namespace) = match &target.kind {
                super::TaskKind::Kernel(thread) => (
                    thread.credentials.resuids(),
                    thread.credentials.user_namespace.clone(),
                ),
                super::TaskKind::User { .. } => {
                    let credentials = target.thread().credentials.lock();
                    (credentials.resuids(), credentials.user_namespace.clone())
                }
            };
            self.may_signal_identity(ids, &namespace)
        }
    }

    /// Process-directed signal 使用原 leader owner，不能随 live sibling 的退出顺序换身份。
    pub(in crate::execution::task) fn may_signal_process(&self, target: &super::Process) -> bool {
        self.tgid() == target.tgid.key() || {
            let (ids, namespace) = target.process_credential_uids();
            self.may_signal_identity(ids, &namespace)
        }
    }

    fn may_signal_identity(
        &self,
        target: [u32; 3],
        namespace: &Arc<crate::namespace::UserNamespace>,
    ) -> bool {
        let credentials = self.thread().credentials.lock();
        let sender = credentials.resuids();
        credentials
            .namespace_authority()
            .capable(namespace, Capability::KILL)
            || [sender[0], sender[1]]
                .iter()
                .any(|uid| *uid == target[0] || *uid == target[2])
    }

    /// proc exe/fd 使用原 leader credential handle；同线程组免检。
    pub(in crate::execution::task) fn may_inspect_process(&self, target: &super::Process) -> bool {
        self.tgid() == target.tgid.key() || {
            let credentials = target.process_credential_snapshot();
            let mm = target.process_memory_inspection();
            self.may_inspect_identity(credentials, mm, InspectionCredentials::Filesystem)
        }
    }

    /// proc root/cwd 的指定 Thread 使用实际 credential owner，不能借另一 leader 的身份。
    pub(in crate::execution::task) fn may_inspect_thread(&self, target: &TaskControlBlock) -> bool {
        self.tgid() == target.tgid() || {
            let credentials = target.credential_snapshot();
            let mm = target.memory_inspection();
            self.may_inspect_identity(credentials, mm, InspectionCredentials::Filesystem)
        }
    }

    fn may_inspect_identity(
        &self,
        target: crate::fs::ProcCredentialSnapshot,
        mm: MmMembership,
        mode: InspectionCredentials,
    ) -> bool {
        let caller = self.credential_snapshot();
        let effective = caller.capabilities.sets.effective;
        let (uid, gid, capabilities) = match mode {
            InspectionCredentials::Filesystem => (caller.fsids[0], caller.fsids[1], effective),
            InspectionCredentials::Real => (
                caller.uids[0],
                caller.gids[0],
                caller.capabilities.sets.permitted,
            ),
        };
        let authority = crate::namespace::NamespaceAuthority {
            namespace: caller.user_namespace.clone(),
            effective_uid: caller.uids[1],
            effective_gid: caller.gids[1],
            effective_capabilities: effective,
        };
        let identity_allowed = authority.capable(&target.user_namespace, Capability::SYS_PTRACE)
            || (Arc::ptr_eq(&caller.user_namespace, &target.user_namespace)
                && target.uids.iter().all(|target| *target == uid)
                && target.gids.iter().all(|target| *target == gid)
                && target.capabilities.sets.permitted & !capabilities == 0);
        identity_allowed
            && match mm {
                MmMembership::Live(mm) => {
                    mm.dumpable() || authority.capable(mm.user_namespace(), Capability::SYS_PTRACE)
                }
                MmMembership::Retired { user_dumpable } => {
                    user_dumpable
                        || authority
                            .capable(&crate::namespace::initial_user(), Capability::SYS_PTRACE)
                }
            }
    }

    /// Linux cap_ptrace_traceme：parent 的 permitted 子集或目标 UserNS 的 SYS_PTRACE。
    /// 自愿跟踪不复用 ATTACH 的 UID/dumpability 检查。
    pub(in crate::execution::task) fn may_trace_me(&self, target: &TaskControlBlock) -> bool {
        let caller = self.credential_snapshot();
        let target = target.credential_snapshot();
        (Arc::ptr_eq(&caller.user_namespace, &target.user_namespace)
            && target.capabilities.sets.permitted & !caller.capabilities.sets.permitted == 0)
            || self
                .namespace_authority()
                .capable(&target.user_namespace, Capability::SYS_PTRACE)
    }

    /// native REALCREDS ptrace credential policy；READ/ATTACH 调用方串行化目标的完整 exec commit。
    pub(in crate::execution::task) fn may_inspect_thread_real(
        &self,
        target: &TaskControlBlock,
    ) -> bool {
        let credentials = target.credential_snapshot();
        let mm = target.memory_inspection();
        self.may_inspect_real_identity(target.tgid(), credentials, mm)
    }

    /// Live/retired target 共享同一 REALCREDS policy；target 的真实 aliases 已在查询时冻结。
    pub(super) fn may_inspect_real_identity(
        &self,
        tgid: usize,
        credentials: crate::fs::ProcCredentialSnapshot,
        mm: MmMembership,
    ) -> bool {
        self.tgid() == tgid
            || self.may_inspect_identity(credentials, mm, InspectionCredentials::Real)
    }

    /// 以一次 caller credential 快照判断 Linux scheduler 修改权限。
    ///
    /// # Parameters
    ///
    /// - `target`: 待修改的 Thread；credentials 由各 Thread 独立拥有。
    ///
    /// # Returns
    ///
    /// 无权限返回 `None`；同 UID 且 permitted 足够返回 `Some(false)`；SYS_NICE 返回 true。
    pub(in crate::execution::task) fn scheduler_privilege_for(
        &self,
        target: &TaskControlBlock,
    ) -> Option<bool> {
        let caller = self.thread().credentials.lock();
        let caller_euid = caller.effective_uid;
        let caller_permitted = caller.capabilities.snapshot().sets.permitted;
        let authority = caller.namespace_authority();
        drop(caller);
        let target = target.thread().credentials.lock();
        let privileged = authority.capable(&target.user_namespace, Capability::SYS_NICE);
        (privileged
            || ((caller_euid == target.real_uid || caller_euid == target.effective_uid)
                && Arc::ptr_eq(&authority.namespace, &target.user_namespace)
                && target.capabilities.snapshot().sets.permitted & !caller_permitted == 0))
            .then_some(privileged)
    }

    /// Calling Thread 的唯一 scalar identity candidate→security consequence→publication。
    fn commit_identity_change(
        &self,
        update: impl FnOnce(&mut Credentials) -> Result<u32, CredentialUpdateError>,
    ) -> Result<u32, CredentialUpdateError> {
        let mm = self.mm_snapshot(); // 不带 creation/graph/credential guard 取得 mmget。
        let (result, retired) = crate::execution::task::process_table::with_credential_transition(
            self,
            |transition| {
                let mut live = self.thread().credentials.lock();
                let mut next = live.clone(); // 只 clone immutable group Arc，不分配。
                let result = update(&mut next)?;
                if live.changes_security(&next) {
                    if let Some(mm) = &mm {
                        mm.set_dumpable(false);
                    }
                    transition.clear_parent_death();
                }
                Ok((result, core::mem::replace(&mut *live, next)))
            },
        )?;
        drop(retired);
        Ok(result)
    }

    /// 原子执行 setuid 或 setgid credential transition。
    pub(crate) fn set_credential_id(
        &self,
        uid: bool,
        value: u32,
    ) -> Result<(), CredentialUpdateError> {
        self.commit_identity_change(|next| {
            let value = next
                .user_namespace
                .to_kernel(uid, value)
                .ok_or(CredentialUpdateError::Invalid)?;
            if uid {
                next.set_uid(value)
            } else {
                next.set_gid(value)
            }
            .map(|()| 0)
        })
        .map(|_| ())
    }

    /// 原子执行 setresuid 或 setresgid 三元组并刷新 fs ID。
    pub(crate) fn set_credential_res_ids(
        &self,
        uid: bool,
        values: [u32; 3],
    ) -> Result<(), CredentialUpdateError> {
        self.commit_identity_change(|next| {
            let mut mapped = [UNCHANGED_ID; 3];
            for (output, value) in mapped.iter_mut().zip(values) {
                if value != UNCHANGED_ID {
                    *output = next
                        .user_namespace
                        .to_kernel(uid, value)
                        .ok_or(CredentialUpdateError::Invalid)?;
                }
            }
            if uid {
                next.set_resuids(mapped)
            } else {
                next.set_resgids(mapped)
            }
            .map(|()| 0)
            .map_err(|()| CredentialUpdateError::Permission)
        })
        .map(|_| ())
    }

    /// Linux setfsuid/setfsgid 始终返回旧值；拒绝与查询不改变任何身份。
    pub(crate) fn set_filesystem_id(&self, uid: bool, value: u32) -> u32 {
        self.commit_identity_change(|next| {
            let mapped = next
                .user_namespace
                .to_kernel(uid, value)
                .unwrap_or(UNCHANGED_ID);
            let old = next.set_fs_id(uid, mapped);
            Ok(next.user_namespace.display_id(uid, old))
        })
        .expect("scalar setfsid candidate cannot fail")
    }

    /// 无分配地捕获当前 Thread 的不可变 group 内容。
    pub(crate) fn supplementary_groups(&self) -> Option<Arc<Vec<u32>>> {
        self.thread().credentials.lock().groups.clone()
    }

    /// 同一锁下捕获当前 Thread 的 UID/GID/group tuple，供 procfs 投影。
    pub(in crate::execution::task) fn credential_snapshot(
        &self,
    ) -> crate::fs::ProcCredentialSnapshot {
        match &self.kind {
            super::TaskKind::Kernel(thread) => thread.credentials.snapshot(),
            super::TaskKind::User { .. } => self.thread().credentials.lock().snapshot(),
        }
    }

    /// 固定 Linux task_dump_owner；仅返回 scalar，不把 MM 或 credential guard 交给 FS。
    pub(in crate::execution::task) fn proc_inode_owner(&self, mode: u32) -> (u32, u32) {
        if self.is_kernel_thread() {
            return (0, 0);
        }
        let credentials = self.credential_snapshot();
        if mode == 0o040555 {
            return (credentials.uids[1], credentials.gids[1]);
        }
        let Some(mm) = self.mm_snapshot() else {
            return (0, 0);
        };
        if mm.dumpable() {
            return (credentials.uids[1], credentials.gids[1]);
        }
        let namespace = mm.user_namespace();
        (
            namespace.to_kernel(true, 0).unwrap_or(0),
            namespace.to_kernel(false, 0).unwrap_or(0),
        )
    }

    /// 为 calling Thread 替换 supplementary groups，锁外完成验证、排序和可失败准备。
    pub(crate) fn set_supplementary_groups(
        &self,
        mut groups: Vec<u32>,
    ) -> Result<(), CredentialUpdateError> {
        let namespace = self.user_namespace();
        if !self.has_capability(Capability::SETGID) || !namespace.may_setgroups() {
            return Err(CredentialUpdateError::Permission);
        }
        for group in &mut groups {
            *group = namespace
                .to_kernel(false, *group)
                .ok_or(CredentialUpdateError::Invalid)?;
        }
        groups.sort_unstable();
        let prepared = if groups.is_empty() {
            None
        } else {
            Some(Arc::try_new(groups).map_err(|_| CredentialUpdateError::OutOfMemory)?)
        };
        let old = {
            let mut credentials = self.thread().credentials.lock();
            if !credentials.capabilities.capable(Capability::SETGID)
                || !Arc::ptr_eq(&credentials.user_namespace, &namespace)
                || !credentials.user_namespace.may_setgroups()
            {
                return Err(CredentialUpdateError::Permission);
            }
            core::mem::replace(&mut credentials.groups, prepared)
        };
        // 大 group vector 和失败的 prepared 值都在 identity owner guard 之外退休。
        drop(old);
        Ok(())
    }

    pub(super) fn prepare_exec_credentials(
        &self,
        mode: u32,
        uid: u32,
        gid: u32,
        file: Option<syscall_abi::file_capability::FileCapabilityData>,
    ) -> Result<PreparedExecCredentials, super::capabilities::CapabilityError> {
        let nnp =
            self.no_new_privileges() || super::super::process_table::ptrace::restrict_exec(self);
        self.thread()
            .credentials
            .lock()
            .prepare_exec(mode, uid, gid, nnp, file)
    }

    pub(super) fn commit_exec_credentials(
        &self,
        prepared: PreparedExecCredentials,
        readable: bool,
    ) {
        let mm = self.address_space();
        let retired =
            crate::execution::task::process_table::with_credential_transition(self, |transition| {
                let mut live = self.thread().credentials.lock();
                // Linux setup_new_exec 用旧 current R/E，随后 commit_creds 仍可因 E/FS/newP
                // 变化再次禁用。不能在 commit 后用最终 IDs 或 AT_SECURE 重新启用。
                mm.set_dumpable(readable && prepared.provisional_dumpable);
                if live.changes_security(&prepared.credentials) {
                    mm.set_dumpable(false);
                }
                if prepared.clear_parent_death {
                    transition.clear_parent_death();
                }
                core::mem::replace(&mut *live, prepared.credentials)
            });
        drop(retired);
    }

    /// 为 calling Thread 永久启用 Linux no_new_privs；不影响已存在的 sibling。
    ///
    /// # Returns
    ///
    /// 无返回值；重复设置幂等，fork/clone 复制此值，exec 不清除。
    pub(crate) fn enable_no_new_privileges(&self) {
        self.thread().security.enable_no_new_privileges();
    }

    /// 返回 calling Thread 的 Linux no_new_privs 状态。
    pub(crate) fn no_new_privileges(&self) -> bool {
        self.thread().security.no_new_privileges()
    }
}

impl Credentials {
    fn message_identity(&self) -> super::MessageIdentity {
        super::MessageIdentity::new(self.namespace_authority(), self.resuids(), self.resgids())
    }

    pub(super) fn namespace_authority(&self) -> crate::namespace::NamespaceAuthority {
        crate::namespace::NamespaceAuthority {
            namespace: self.user_namespace.clone(),
            effective_uid: self.effective_uid,
            effective_gid: self.effective_gid,
            effective_capabilities: self.capabilities.snapshot().sets.effective,
        }
    }

    pub(in crate::execution::task) fn capability_snapshot(
        &self,
    ) -> syscall_abi::capability::CapabilitySnapshot {
        self.capabilities.snapshot()
    }
    pub(super) fn root() -> Self {
        Self {
            real_uid: ROOT_ID,
            effective_uid: ROOT_ID,
            saved_uid: ROOT_ID,
            fs_uid: ROOT_ID,
            real_gid: ROOT_ID,
            effective_gid: ROOT_ID,
            saved_gid: ROOT_ID,
            fs_gid: ROOT_ID,
            groups: None,
            capabilities: CapabilityState::root(),
            user_namespace: crate::namespace::initial_user(),
        }
    }

    pub(super) fn cloned_namespace(&mut self, namespace: Arc<crate::namespace::UserNamespace>) {
        self.user_namespace = namespace;
        self.capabilities = CapabilityState::root();
    }

    pub(super) fn snapshot(&self) -> crate::fs::ProcCredentialSnapshot {
        crate::fs::ProcCredentialSnapshot {
            uids: self.resuids(),
            gids: self.resgids(),
            fsids: [self.fs_uid, self.fs_gid],
            groups: self.groups.clone(),
            capabilities: self.capabilities.snapshot(),
            user_namespace: self.user_namespace.clone(),
        }
    }

    pub(super) fn access_identity(&self, effective: bool) -> AccessIdentity {
        AccessIdentity::new(
            if effective {
                self.fs_uid
            } else {
                self.real_uid
            },
            if effective {
                self.fs_gid
            } else {
                self.real_gid
            },
            self.groups.clone(),
            self.user_namespace.clone(),
            self.capabilities.access_effective(
                effective,
                self.real_uid,
                self.user_namespace.to_kernel(true, 0),
            ),
            self.effective_uid,
        )
    }

    pub(super) fn uid(&self, effective: bool) -> u32 {
        if effective {
            self.effective_uid
        } else {
            self.real_uid
        }
    }

    pub(super) fn gid(&self, effective: bool) -> u32 {
        if effective {
            self.effective_gid
        } else {
            self.real_gid
        }
    }

    pub(super) fn resuids(&self) -> [u32; 3] {
        [self.real_uid, self.effective_uid, self.saved_uid]
    }

    pub(super) fn resgids(&self) -> [u32; 3] {
        [self.real_gid, self.effective_gid, self.saved_gid]
    }

    pub(super) fn set_uid(&mut self, uid: u32) -> Result<(), CredentialUpdateError> {
        if uid == UNCHANGED_ID {
            return Err(CredentialUpdateError::Invalid);
        }
        let old = self.resuids();
        if self.capabilities.capable(Capability::SETUID) {
            self.real_uid = uid;
            self.effective_uid = uid;
            self.saved_uid = uid;
        } else if uid == self.real_uid || uid == self.saved_uid {
            self.effective_uid = uid;
        } else {
            return Err(CredentialUpdateError::Permission);
        }
        self.fs_uid = self.effective_uid;
        self.capabilities
            .fix_setuid(old, self.resuids(), self.user_namespace.to_kernel(true, 0));
        Ok(())
    }

    pub(super) fn set_gid(&mut self, gid: u32) -> Result<(), CredentialUpdateError> {
        if gid == UNCHANGED_ID {
            return Err(CredentialUpdateError::Invalid);
        }
        if self.capabilities.capable(Capability::SETGID) {
            self.real_gid = gid;
            self.effective_gid = gid;
            self.saved_gid = gid;
            self.fs_gid = self.effective_gid;
            return Ok(());
        }
        if matches!(gid, value if value == self.real_gid || value == self.saved_gid) {
            self.effective_gid = gid;
            self.fs_gid = self.effective_gid;
            Ok(())
        } else {
            Err(CredentialUpdateError::Permission)
        }
    }

    pub(super) fn set_resuids(&mut self, ids: [u32; 3]) -> Result<(), ()> {
        let permitted = self.capabilities.capable(Capability::SETUID)
            || ids.iter().all(|id| {
                *id == UNCHANGED_ID
                    || matches!(*id, value if value == self.real_uid || value == self.effective_uid || value == self.saved_uid)
            });
        if !permitted {
            return Err(());
        }
        let old = self.resuids();
        if ids[0] != UNCHANGED_ID {
            self.real_uid = ids[0];
        }
        if ids[1] != UNCHANGED_ID {
            self.effective_uid = ids[1];
        }
        if ids[2] != UNCHANGED_ID {
            self.saved_uid = ids[2];
        }
        self.fs_uid = self.effective_uid;
        self.capabilities
            .fix_setuid(old, self.resuids(), self.user_namespace.to_kernel(true, 0));
        Ok(())
    }

    pub(super) fn set_resgids(&mut self, ids: [u32; 3]) -> Result<(), ()> {
        let permitted = self.capabilities.capable(Capability::SETGID)
            || ids.iter().all(|id| {
                *id == UNCHANGED_ID
                    || matches!(*id, value if value == self.real_gid || value == self.effective_gid || value == self.saved_gid)
            });
        if !permitted {
            return Err(());
        }
        if ids[0] != UNCHANGED_ID {
            self.real_gid = ids[0];
        }
        if ids[1] != UNCHANGED_ID {
            self.effective_gid = ids[1];
        }
        if ids[2] != UNCHANGED_ID {
            self.saved_gid = ids[2];
        }
        self.fs_gid = self.effective_gid;
        Ok(())
    }

    /// Linux setfsid 返回旧值，拒绝时保持身份；不把失败编码成 errno。
    pub(super) fn set_fs_id(&mut self, uid: bool, value: u32) -> u32 {
        let old = if uid { self.fs_uid } else { self.fs_gid };
        let ids = if uid { self.resuids() } else { self.resgids() };
        let capability = if uid {
            Capability::SETUID
        } else {
            Capability::SETGID
        };
        if value != UNCHANGED_ID
            && (ids.contains(&value) || value == old || self.capabilities.capable(capability))
        {
            if uid {
                self.fs_uid = value;
                self.capabilities
                    .fix_fsuid(old, value, self.user_namespace.to_kernel(true, 0));
            } else {
                self.fs_gid = value;
            }
        }
        old
    }

    /// commit_creds 的 security consequence；只能在 candidate 发布前检查。
    fn changes_security(&self, next: &Self) -> bool {
        self.effective_uid != next.effective_uid
            || self.effective_gid != next.effective_gid
            || self.fs_uid != next.fs_uid
            || self.fs_gid != next.fs_gid
            || !self.covers_capabilities(next)
    }

    fn covers_capabilities(&self, next: &Self) -> bool {
        if Arc::ptr_eq(&self.user_namespace, &next.user_namespace) {
            next.capabilities.snapshot().sets.permitted
                & !self.capabilities.snapshot().sets.permitted
                == 0
        } else {
            next.user_namespace
                .owned_descendant(&self.user_namespace, self.effective_uid)
        }
    }

    /// 准备完整 exec credential transaction；只 clone immutable groups，不修改 live owner。
    pub(super) fn prepare_exec(
        &self,
        mode: u32,
        uid: u32,
        gid: u32,
        nnp: bool,
        file: Option<syscall_abi::file_capability::FileCapabilityData>,
    ) -> Result<PreparedExecCredentials, super::capabilities::CapabilityError> {
        let mut next = self.clone();
        let mapped_inode = self.user_namespace.map_kernel_id(true, uid).is_some()
            && self.user_namespace.map_kernel_id(false, gid).is_some();
        if !nnp && mapped_inode && mode & 0o4000 != 0 {
            next.effective_uid = uid;
        }
        if !nnp && mapped_inode && mode & 0o2010 == 0o2010 {
            next.effective_gid = gid;
        }
        let id_changed = next.effective_uid != self.effective_uid
            || !self.access_identity(true).in_group(next.effective_gid);
        let outcome = next.capabilities.exec(CapabilityExecIds {
            uids: [next.real_uid, next.effective_uid],
            gids: [next.real_gid, next.effective_gid],
            root: self.user_namespace.to_kernel(true, 0),
            id_changed,
            nnp,
            file,
        })?;
        if outcome.downgrade_ids {
            next.effective_uid = next.real_uid;
            next.effective_gid = next.real_gid;
        }
        next.saved_uid = next.effective_uid;
        next.saved_gid = next.effective_gid;
        next.fs_uid = next.effective_uid;
        next.fs_gid = next.effective_gid;
        let clear_parent_death = outcome.secure || self.changes_security(&next);
        Ok(PreparedExecCredentials {
            credentials: next,
            secure: outcome.secure,
            clear_parent_death,
            provisional_dumpable: self.real_uid == self.effective_uid
                && self.real_gid == self.effective_gid,
        })
    }
}

/// credential replacement 的 permission 与 owner allocation 失败分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CredentialUpdateError {
    Permission,
    Invalid,
    OutOfMemory,
}

/// 建栈、secureexec 和 PONR commit 共用的唯一未发布凭据结果。
pub(super) struct PreparedExecCredentials {
    pub(super) credentials: Credentials,
    secure: bool,
    clear_parent_death: bool,
    provisional_dumpable: bool,
}
impl PreparedExecCredentials {
    pub(super) fn stack_credentials(&self) -> crate::memory::InitialStackCredentials {
        crate::memory::InitialStackCredentials {
            uid: self
                .credentials
                .user_namespace
                .display_id(true, self.credentials.real_uid),
            euid: self
                .credentials
                .user_namespace
                .display_id(true, self.credentials.effective_uid),
            gid: self
                .credentials
                .user_namespace
                .display_id(false, self.credentials.real_gid),
            egid: self
                .credentials
                .user_namespace
                .display_id(false, self.credentials.effective_gid),
            secure: self.secure,
        }
    }
}

// Process 生命周期内唯一投影；graph/proc observer 不保活任何 TCB。
impl super::Process {
    fn process_credential_uids(&self) -> ([u32; 3], Arc<crate::namespace::UserNamespace>) {
        // 只 pin immutable namespace ancestry，不复制/退休 supplementary groups；pin 在 graph guard 外析构。
        let leader = self.leader_credentials.lock();
        let credentials = leader.lock();
        (credentials.resuids(), credentials.user_namespace.clone())
    }
    pub(in crate::execution::task) fn process_credential_snapshot(
        &self,
    ) -> crate::fs::ProcCredentialSnapshot {
        let leader = self.leader_credentials.lock();
        leader.lock().snapshot()
    }
}
