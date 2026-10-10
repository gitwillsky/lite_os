//! User namespace 的 immutable ancestry 与唯一 map/setgroups publication owner。
use super::quota::{UserQuotas, UserResource, UserResourceCharge};
use super::{IdMap, NamespaceError};
use alloc::sync::Arc;
use spin::Mutex;
use syscall_abi::capability::Capability;

/// proc map 控制文件种类；不把 UID/GID 与 setgroups 写成另一路 credential policy。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum UserNamespaceMap {
    Uid,
    Gid,
    Setgroups,
}
struct MappingState {
    uid: Option<Arc<IdMap>>,
    gid: Option<Arc<IdMap>>,
    setgroups_allowed: bool,
}

/// Namespace 的身份、parent/owner 永不改变；UID/GID maps 只成功发布一次，setgroups deny 不可撤销。
///
/// state guard 只 clone immutable map Arc 或提交已准备 Arc/scalar，不分配、不读 credentials，
/// 不做 user-copy；credential→map 是唯一锁序。ancestry 按固定基线限制 parent level>32，最后 Drop 不递归进入 task。
pub(crate) struct UserNamespace {
    id: u64,
    parent: Option<Arc<UserNamespace>>,
    owner: u32,
    level: u8,
    parent_could_setfcap: bool,
    mappings: Mutex<MappingState>,
    pub(super) quotas: UserQuotas,
    pub(super) rlimit_counts: super::rlimit_counts::RlimitCounters,
    _charge: Option<UserResourceCharge>,
}
impl core::fmt::Debug for UserNamespace {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("UserNamespace")
            .field("id", &self.id)
            .field("level", &self.level)
            .finish_non_exhaustive()
    }
}

/// 一次 credential 快照的 namespace-capability authority；ID 为 global kuid/kgid。
/// f_cred 与当前 writer 各自保留该值，不能在 proc map write 时补取 opener 的新凭据。
#[derive(Clone, Debug)]
pub(crate) struct NamespaceAuthority {
    pub(crate) namespace: Arc<UserNamespace>,
    pub(crate) effective_uid: u32,
    pub(crate) effective_gid: u32,
    pub(crate) effective_capabilities: u64,
}
impl NamespaceAuthority {
    /// 固定 commoncap：自己 namespace 查 pE，parent namespace 的 owner 对其后代有能力，
    /// 不允许 child capability 授予任何 ancestor/global resource。
    pub(crate) fn capable(&self, target: &Arc<UserNamespace>, capability: Capability) -> bool {
        let mut current = target;
        loop {
            if Arc::ptr_eq(&self.namespace, current) {
                return self.effective_capabilities & capability.mask() != 0;
            }
            let Some(parent) = current.parent.as_ref() else {
                return false;
            };
            if Arc::ptr_eq(&self.namespace, parent) && self.effective_uid == current.owner {
                return true;
            }
            if current.level <= self.namespace.level {
                return false;
            }
            current = parent;
        }
    }
}
impl UserNamespace {
    pub(super) fn initial(limit: u32) -> Result<Arc<Self>, NamespaceError> {
        Self::allocate(
            3,
            None,
            0,
            true,
            true,
            limit,
            None,
            super::NamespaceRlimits {
                locked_memory: u64::MAX,
                pending_signals: u64::MAX,
            },
        )
    }
    #[allow(
        clippy::too_many_arguments,
        reason = "同一次原子 publication 的独立领域输入，参数语义与所有权已逐项说明"
    )]
    fn allocate(
        id: u64,
        parent: Option<Arc<Self>>,
        owner: u32,
        parent_could_setfcap: bool,
        setgroups_allowed: bool,
        limit: u32,
        charge: Option<UserResourceCharge>,
        rlimits: super::NamespaceRlimits,
    ) -> Result<Arc<Self>, NamespaceError> {
        let level = parent.as_ref().map_or(0, |parent| parent.level + 1);
        Arc::try_new(Self {
            id,
            parent,
            owner,
            level,
            parent_could_setfcap,
            quotas: UserQuotas::new(limit),
            rlimit_counts: super::rlimit_counts::RlimitCounters::new(rlimits),
            _charge: charge,
            mappings: Mutex::new(MappingState {
                uid: None,
                gid: None,
                setgroups_allowed,
            }),
        })
        .map_err(|_| NamespaceError::OutOfMemory)
    }
    /// 准备未发布 child；creator EUID/EGID 必须都在 parent map 中，chroot 条件由 fs membership 检查。
    pub(crate) fn child(
        creator: &NamespaceAuthority,
        at_namespace_root: bool,
        rlimits: super::NamespaceRlimits,
    ) -> Result<Arc<Self>, NamespaceError> {
        let parent = &creator.namespace;
        if parent.level > 32 {
            return Err(NamespaceError::NestingLimit);
        }
        let charge =
            UserResourceCharge::acquire(parent, creator.effective_uid, UserResource::User)?;
        if !at_namespace_root
            || parent.map_kernel_id(true, creator.effective_uid).is_none()
            || parent.map_kernel_id(false, creator.effective_gid).is_none()
        {
            return Err(NamespaceError::Permission);
        }
        let allowed = parent.mappings.lock().setgroups_allowed;
        Self::allocate(
            super::next_namespace_id(),
            Some(parent.clone()),
            creator.effective_uid,
            creator.capable(parent, Capability::SETFCAP),
            allowed,
            i32::MAX as u32,
            Some(charge),
            rlimits,
        )
    }
    /// current_in_userns：self 是 target 或其后代；只读取 immutable parent ancestry。
    pub(crate) fn is_within(self: &Arc<Self>, target: &Arc<Self>) -> bool {
        let mut current = self.clone();
        loop {
            if Arc::ptr_eq(&current, target) {
                return true;
            }
            let Some(parent) = current.parent() else {
                return false;
            };
            current = parent;
        }
    }

    /// cred_cap_issubset 的跨 namespace 关系：ancestor EUID 拥有 descendant 或其一个祖先。
    /// 与 capability bits 比较不同；进入自己创建的 child 不属于 privilege gain。
    pub(crate) fn owned_descendant(self: &Arc<Self>, ancestor: &Arc<Self>, uid: u32) -> bool {
        let mut current = self;
        while let Some(parent) = current.parent.as_ref() {
            if Arc::ptr_eq(parent, ancestor) && current.owner == uid {
                return true;
            }
            current = parent;
        }
        false
    }

    pub(super) fn level(&self) -> u8 {
        self.level
    }
    pub(crate) fn id(&self) -> u64 {
        self.id
    }
    pub(crate) fn parent(&self) -> Option<Arc<Self>> {
        self.parent.clone()
    }
    pub(crate) fn owner(&self) -> u32 {
        self.owner
    }
    pub(crate) fn is_initial(&self) -> bool {
        self.parent.is_none()
    }
    pub(crate) fn map(&self, uid: bool) -> Option<Arc<IdMap>> {
        let state = self.mappings.lock();
        if uid {
            state.uid.clone()
        } else {
            state.gid.clone()
        }
    }
    pub(crate) fn to_kernel(&self, uid: bool, local: u32) -> Option<u32> {
        self.range_to_kernel(uid, local, 1)
    }
    pub(crate) fn range_to_kernel(&self, uid: bool, local: u32, count: u32) -> Option<u32> {
        if self.is_initial() {
            return local
                .checked_add(count)
                .filter(|_| count != 0)
                .map(|_| local);
        }
        self.map(uid)?.range_to_kernel(local, count)
    }
    pub(crate) fn map_kernel_id(&self, uid: bool, kernel: u32) -> Option<u32> {
        if kernel == u32::MAX {
            return None;
        }
        if self.is_initial() {
            Some(kernel)
        } else {
            self.map(uid)?.map_kernel_id(kernel)
        }
    }
    pub(crate) fn display_id(&self, uid: bool, kernel: u32) -> u32 {
        self.map_kernel_id(uid, kernel).unwrap_or(65534)
    }
    pub(crate) fn may_setgroups(&self) -> bool {
        let state = self.mappings.lock();
        state.setgroups_allowed && (self.is_initial() || state.gid.is_some())
    }
    pub(crate) fn setgroups_allowed(&self) -> bool {
        self.mappings.lock().setgroups_allowed
    }

    /// 标准 uid_map/gid_map write；解析与两向index准备在锁外，最后一次无失败 publication。
    /// Opener 只允许处于 target 或其 parent；current 与 opener 均须满足 privileged parent check。
    pub(crate) fn publish_map(
        self: &Arc<Self>,
        uid: bool,
        bytes: &[u8],
        opener: &NamespaceAuthority,
        writer: &NamespaceAuthority,
    ) -> Result<(), NamespaceError> {
        let parent = self.parent.as_ref().ok_or(NamespaceError::Permission)?;
        if (!Arc::ptr_eq(&opener.namespace, self) && !Arc::ptr_eq(&opener.namespace, parent))
            || !opener.capable(self, Capability::SYS_ADMIN)
        {
            return Err(NamespaceError::Permission);
        }
        // 固定 map_write 的错误顺序；锁外准备后 publication 仍须复验唯一写入。
        if self.map(uid).is_some() {
            return Err(NamespaceError::Permission);
        }
        let mut extents = IdMap::parse(bytes)?;
        if uid
            && extents.iter().any(|extent| extent.lower_first == 0)
            && if Arc::ptr_eq(&opener.namespace, self) {
                !self.parent_could_setfcap
            } else {
                !opener.capable(parent, Capability::SETFCAP)
            }
        {
            return Err(NamespaceError::Permission);
        }
        let capability = if uid {
            Capability::SETUID
        } else {
            Capability::SETGID
        };
        let privileged = opener.capable(parent, capability) && writer.capable(parent, capability);
        let own_single = extents.len() == 1
            && extents[0].count == 1
            && self.owner == opener.effective_uid
            && parent.to_kernel(uid, extents[0].lower_first)
                == Some(if uid {
                    opener.effective_uid
                } else {
                    opener.effective_gid
                });
        if !privileged && !own_single {
            return Err(NamespaceError::Permission);
        }
        for extent in &mut extents {
            extent.lower_first = parent
                .range_to_kernel(uid, extent.lower_first, extent.count)
                .ok_or(NamespaceError::Permission)?;
        }
        let map =
            Arc::try_new(IdMap::prepare(extents)?).map_err(|_| NamespaceError::OutOfMemory)?;
        let mut state = self.mappings.lock();
        if (if uid { &state.uid } else { &state.gid }).is_some()
            || (!uid && !privileged && state.setgroups_allowed)
        {
            return Err(NamespaceError::Permission);
        }
        if uid {
            state.uid = Some(map);
        } else {
            state.gid = Some(map);
        }
        Ok(())
    }
    /// setgroups 的 allow/deny 控制；deny 只可在 gid_map 尚未发布时提交，不能重新 allow。
    pub(crate) fn control_setgroups(
        self: &Arc<Self>,
        allow: bool,
        opener: &NamespaceAuthority,
    ) -> Result<(), NamespaceError> {
        if !opener.capable(self, Capability::SYS_ADMIN) {
            return Err(NamespaceError::Permission);
        }
        let mut state = self.mappings.lock();
        if allow {
            if !state.setgroups_allowed {
                return Err(NamespaceError::Permission);
            }
        } else {
            if self.is_initial() || state.gid.is_some() {
                return Err(NamespaceError::Permission);
            }
            state.setgroups_allowed = false;
        }
        Ok(())
    }
    /// 取得 parent-local range 格式所需的不可变 records；initial map 不分配巨型 identity array。
    pub(crate) fn mapping_extents(&self, uid: bool) -> Option<Arc<IdMap>> {
        self.map(uid)
    }
}
