//! 标准 ucounts：namespace 与 inotify admission 按 UserNS、创建者 kuid 和祖先 owner 链收费。
use super::{NamespaceAuthority, NamespaceError, UserNamespace};
use crate::fallible_tree::FallibleMap;
use alloc::{sync::Arc, vec::Vec};
use spin::Mutex;
use syscall_abi::capability::Capability;

/// 当前具体 namespace owner 支持的标准 user sysctl resource；不能为未实现 owner 虚报计数。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum UserResource {
    User,
    Uts,
    Mount,
    Time,
    Pid,
    Ipc,
    InotifyInstances,
    InotifyWatches,
    Net,
}
impl UserResource {
    const COUNT: usize = 9;
    fn index(self) -> usize {
        match self {
            Self::User => 0,
            Self::Uts => 1,
            Self::Mount => 2,
            Self::Time => 3,
            Self::Pid => 4,
            Self::Ipc => 5,
            Self::InotifyInstances => 6,
            Self::InotifyWatches => 7,
            Self::Net => 8,
        }
    }
    pub(crate) fn name(self) -> &'static [u8] {
        match self {
            Self::User => b"max_user_namespaces",
            Self::Uts => b"max_uts_namespaces",
            Self::Mount => b"max_mnt_namespaces",
            Self::Time => b"max_time_namespaces",
            Self::Pid => b"max_pid_namespaces",
            Self::Ipc => b"max_ipc_namespaces",
            Self::InotifyInstances => b"max_inotify_instances",
            Self::InotifyWatches => b"max_inotify_watches",
            Self::Net => b"max_net_namespaces",
        }
    }
}
struct QuotaState {
    limits: [u32; UserResource::COUNT],
    // OWNER: 各真实 kuid 的唯一非零 ucounts；node preparation/retirement 均在 guard 外。
    users: FallibleMap<u32, [u32; UserResource::COUNT]>,
}
/// 每个 UserNamespace 唯一拥有标准 user resource limits 和真实 admission 数量。
pub(super) struct UserQuotas(Mutex<QuotaState>);
impl UserQuotas {
    pub(super) fn new(limit: u32) -> Self {
        Self(Mutex::new(QuotaState {
            limits: [limit; UserResource::COUNT],
            users: FallibleMap::new(),
        }))
    }
    pub(super) fn initialize_inotify(&self, watches: u32) {
        let mut state = self.0.lock();
        state.limits[UserResource::InotifyInstances.index()] = 128;
        state.limits[UserResource::InotifyWatches.index()] = watches;
    }
    fn increment(&self, uid: u32, resource: UserResource) -> Result<(), NamespaceError> {
        let kind = resource.index();
        let mut prepared: Option<crate::fallible_tree::NodeSlot<u32, [u32; UserResource::COUNT]>> =
            None;
        let result = loop {
            let mut state = self.0.lock();
            let count = state.users.get(&uid).map_or(0, |counts| counts[kind]);
            if count >= state.limits[kind] {
                break Err(NamespaceError::QuotaLimit);
            }
            if let Some(counts) = state.users.get_mut(&uid) {
                counts[kind] += 1;
                break Ok(());
            }
            if let Some(node) = prepared.take() {
                let mut counts = [0; UserResource::COUNT];
                counts[kind] = 1;
                state.users.commit_vacant(node.fill(uid, counts));
                break Ok(());
            }
            drop(state);
            // 原生 inc_ucounts 的 allocation 失败与 limit 失败均返回 NULL；namespace creator 映射 ENOSPC。
            prepared =
                Some(FallibleMap::try_reserve_node().map_err(|_| NamespaceError::QuotaLimit)?);
        };
        drop(prepared);
        result
    }
    fn decrement(&self, uid: u32, resource: UserResource) {
        let retired = {
            let mut state = self.0.lock();
            let counts = state
                .users
                .get_mut(&uid)
                .expect("actual user resource charge missing");
            counts[resource.index()] = counts[resource.index()]
                .checked_sub(1)
                .expect("user resource charge underflow");
            if *counts == [0; UserResource::COUNT] {
                state.users.take_entry(&uid)
            } else {
                None
            }
        };
        drop(retired);
    }
    pub(super) fn limit(&self, resource: UserResource) -> u32 {
        self.0.lock().limits[resource.index()]
    }
    fn set_limit(&self, resource: UserResource, limit: u32) {
        self.0.lock().limits[resource.index()] = limit;
    }
}

/// 实际 resource admission 独占收费 token；失败回滚与实际退休归还已收费的祖先。
/// Arc 只 pin actual user namespace，不保活 TCB；child namespace 只能引用祖先，不形成 cycle。
pub(crate) struct UserResourceCharge {
    chain: Vec<(Arc<UserNamespace>, u32)>,
    charged: usize,
    resource: UserResource,
}
impl UserResourceCharge {
    /// 原收费者；watch admission 固定于 instance 的创建凭据，SCM_RIGHTS recipient 不改变 owner。
    pub(crate) fn owner(&self) -> (Arc<UserNamespace>, u32) {
        let (namespace, uid) = &self.chain[0];
        (namespace.clone(), *uid)
    }
    pub(crate) fn acquire(
        owner: &Arc<UserNamespace>,
        uid: u32,
        resource: UserResource,
    ) -> Result<Self, NamespaceError> {
        let mut token = Self {
            chain: Vec::new(),
            charged: 0,
            resource,
        };
        token
            .chain
            .try_reserve_exact(usize::from(owner.level()) + 1)
            .map_err(|_| NamespaceError::QuotaLimit)?;
        let mut namespace = owner.clone();
        let mut charged_uid = uid;
        loop {
            token.chain.push((namespace.clone(), charged_uid));
            let Some(parent) = namespace.parent() else {
                break;
            };
            charged_uid = namespace.owner();
            namespace = parent;
        }
        for (namespace, uid) in &token.chain {
            namespace.quotas.increment(*uid, resource)?;
            token.charged += 1;
        }
        Ok(token)
    }
}
impl Drop for UserResourceCharge {
    fn drop(&mut self) {
        for (namespace, uid) in &self.chain[..self.charged] {
            namespace.quotas.decrement(*uid, self.resource);
        }
    }
}
impl UserNamespace {
    pub(crate) fn resource_limit(&self, resource: UserResource) -> u32 {
        self.quotas.limit(resource)
    }
    /// user sysctl 标准 0..INT_MAX 范围；CAP_SYS_RESOURCE 必须属于被修改的 user namespace。
    pub(crate) fn set_resource_limit(
        self: &Arc<Self>,
        resource: UserResource,
        limit: u32,
        caller: &NamespaceAuthority,
    ) -> Result<(), NamespaceError> {
        if !caller.capable(self, Capability::SYS_RESOURCE) {
            return Err(NamespaceError::Permission);
        }
        if limit > i32::MAX as u32 {
            return Err(NamespaceError::Invalid);
        }
        self.quotas.set_limit(resource, limit);
        Ok(())
    }
}
/// fs/inotify 的全局 0644 sysctl 修改同一 initial UserNS ledger；不用 capability 豁免 DAC。
pub(crate) fn set_initial_inotify_limit(
    resource: UserResource,
    limit: u32,
    caller: &NamespaceAuthority,
) -> Result<(), NamespaceError> {
    if caller.effective_uid != 0 {
        return Err(NamespaceError::Permission);
    }
    if !matches!(
        resource,
        UserResource::InotifyInstances | UserResource::InotifyWatches
    ) || limit > i32::MAX as u32
    {
        return Err(NamespaceError::Invalid);
    }
    super::initial_user().quotas.set_limit(resource, limit);
    Ok(())
}
/// 固定 fork_init policy：RAM 可支持的 kernel stacks 的八分之一，20..FUTEX_TID_MASK，取一半为初始配额。
/// Composition root 显式传入已初始化 allocator 的容量与实际 stack bytes，无隐式 memory/task 依赖。
pub(crate) fn initial_namespace_limit(total_bytes: usize, stack_bytes: usize) -> u32 {
    assert!(stack_bytes != 0, "kernel stack capacity must be known");
    ((total_bytes / stack_bytes / 8).clamp(20, 0x3fff_ffff) / 2) as u32
}
