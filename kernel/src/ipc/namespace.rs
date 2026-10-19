//! IPC namespace 是实际 System V 对象、IDs/key 索引和限制的唯一 owner；不拥有 Task membership。
mod description;
mod shared_memory;
use super::ids::{IPC_ID_SLOTS, IpcIds, PreparedIpcObject};
use super::semaphore::PreparedSemaphores;
use super::{
    IpcAccess, IpcError, IpcPermission, MessageQueue, MessageQueueSnapshot, SemaphoreSet,
    SemaphoreSnapshot,
};
use crate::namespace::{
    NamespaceError, NamespaceOwner, NamespaceType, UserNamespace, UserResource, UserResourceCharge,
};
use alloc::sync::Arc;
pub(crate) use description::{IpcDescription, IpcTable};
use spin::{Mutex, Once};
use syscall_abi::{
    capability::Capability,
    ipc::{IPC_CREAT, IPC_EXCL},
};

/// Linux IPC namespace 的 message sysctl 字段；新 namespace 使用标准初态，不复制 parent 的调优值。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum MessageLimit {
    Maximum,
    QueueBytes,
    Queues,
}
impl MessageLimit {
    pub(crate) fn name(self) -> &'static [u8] {
        match self {
            Self::Maximum => b"msgmax",
            Self::QueueBytes => b"msgmnb",
            Self::Queues => b"msgmni",
        }
    }
    fn index(self) -> usize {
        match self {
            Self::Maximum => 0,
            Self::QueueBytes => 1,
            Self::Queues => 2,
        }
    }
}
/// 标准 IPC sysctl selector；proc inode/open fd 只保存 selector 与实际 target owner。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum SharedMemoryLimit {
    MaximumSize,
    TotalPages,
    Segments,
    ForcedRemove,
}
impl SharedMemoryLimit {
    pub(crate) fn name(self) -> &'static [u8] {
        match self {
            Self::MaximumSize => b"shmmax",
            Self::TotalPages => b"shmall",
            Self::Segments => b"shmmni",
            Self::ForcedRemove => b"shm_rmid_forced",
        }
    }
}
/// 标准 IPC sysctl selector；proc inode/open fd 只保存 selector 与实际 target owner。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum IpcLimit {
    Message(MessageLimit),
    Semaphores,
    SharedMemory(SharedMemoryLimit),
}
impl IpcLimit {
    pub(crate) const ALL: [Self; 8] = [
        Self::Message(MessageLimit::Maximum),
        Self::Message(MessageLimit::QueueBytes),
        Self::Message(MessageLimit::Queues),
        Self::Semaphores,
        Self::SharedMemory(SharedMemoryLimit::MaximumSize),
        Self::SharedMemory(SharedMemoryLimit::TotalPages),
        Self::SharedMemory(SharedMemoryLimit::Segments),
        Self::SharedMemory(SharedMemoryLimit::ForcedRemove),
    ];
    pub(crate) fn name(self) -> &'static [u8] {
        match self {
            Self::Message(limit) => limit.name(),
            Self::Semaphores => b"sem",
            Self::SharedMemory(limit) => limit.name(),
        }
    }
}
pub(crate) enum IpcLimitValue {
    Message(u32),
    Semaphores([i32; 4]),
    SharedMemory(u64),
}
struct MessageRegistry {
    ids: IpcIds<MessageQueue>,
    limits: [u32; 3],
}
struct SemaphoreRegistry {
    ids: IpcIds<SemaphoreSet>,
    // OWNER: 只计实际 registry admission 的 semaphore 资源；RMID 在同一事务归还，Arc observer 不收费。
    used: usize,
    limits: [i32; 4],
}
pub(super) struct SharedMemoryRegistry {
    ids: IpcIds<super::SharedMemorySegment>,
    used_pages: usize,
    maximum_size: usize,
    maximum_pages: usize,
    maximum_segments: usize,
    forced_remove: bool,
}
/// 一份 nsfs identity/owning UserNS、收费 token 与实际消息队列 registry；不反向引用 TCB。
pub(crate) struct IpcNamespace {
    id: u64,
    user: Arc<UserNamespace>,
    messages: Mutex<MessageRegistry>,
    semaphores: Mutex<SemaphoreRegistry>,
    shared_memory: Mutex<SharedMemoryRegistry>,
    _charge: Option<UserResourceCharge>,
}
impl core::fmt::Debug for IpcNamespace {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("IpcNamespace")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}
impl NamespaceOwner for IpcNamespace {
    fn kind(&self) -> NamespaceType {
        NamespaceType::Ipc
    }
    fn id(&self) -> u64 {
        self.id
    }
    fn owning_user(&self) -> Option<Arc<UserNamespace>> {
        Some(self.user.clone())
    }
}
// OWNER: boot 在首个 Task 前发布实际 initial IPC namespace；所有初始任务共享同一对象。
static INITIAL_IPC: Once<Arc<IpcNamespace>> = Once::new();
pub(crate) fn initialize_namespaces() -> Result<(), NamespaceError> {
    if INITIAL_IPC.get().is_some() {
        return Ok(());
    }
    let initial = IpcNamespace::allocate(5, crate::namespace::initial_user(), None)?;
    INITIAL_IPC.call_once(|| initial);
    Ok(())
}
pub(crate) fn initial_namespace() -> Arc<IpcNamespace> {
    INITIAL_IPC
        .get()
        .expect("IPC namespace initialized before Task")
        .clone()
}
impl IpcNamespace {
    fn allocate(
        id: u64,
        user: Arc<UserNamespace>,
        charge: Option<UserResourceCharge>,
    ) -> Result<Arc<Self>, NamespaceError> {
        Arc::try_new(Self {
            id,
            user,
            messages: Mutex::new(MessageRegistry {
                ids: IpcIds::new(),
                limits: [8192, 16384, 32000],
            }),
            semaphores: Mutex::new(SemaphoreRegistry {
                ids: IpcIds::new(),
                used: 0,
                limits: [32000, 1024000000, 500, 32000],
            }),
            shared_memory: Mutex::new(SharedMemoryRegistry {
                ids: IpcIds::new(),
                used_pages: 0,
                maximum_size: usize::MAX - (1 << 24),
                maximum_pages: usize::MAX - (1 << 24),
                maximum_segments: 4096,
                forced_remove: false,
            }),
            _charge: charge,
        })
        .map_err(|_| NamespaceError::OutOfMemory)
    }
    pub(crate) fn create(user: Arc<UserNamespace>, uid: u32) -> Result<Arc<Self>, NamespaceError> {
        let charge = UserResourceCharge::acquire(&user, uid, UserResource::Ipc)?;
        Self::allocate(crate::namespace::next_namespace_id(), user, Some(charge))
    }
    pub(crate) fn user_namespace(&self) -> Arc<UserNamespace> {
        self.user.clone()
    }
    pub(crate) fn message_limit(&self, limit: MessageLimit) -> u32 {
        self.messages.lock().limits[limit.index()]
    }
    /// 固定 IPC sysctl 0644 test_perm；CAP_SYS_ADMIN 不豁免，按 owning UserNS 映射的 root EUID。
    pub(crate) fn may_set_limits(&self, caller: &crate::namespace::NamespaceAuthority) -> bool {
        caller.effective_uid == self.user.to_kernel(true, 0).unwrap_or(0)
    }
    /// 标准 sysctl 权限和 min/max 同一 owner 校验；变更影响后续发送/创建，既有 qbytes 独立。
    pub(crate) fn set_message_limit(
        &self,
        limit: MessageLimit,
        value: u32,
        access: &crate::namespace::NamespaceAuthority,
    ) -> Result<(), IpcError> {
        if !self.may_set_limits(access) {
            return Err(IpcError::Permission);
        }
        if value > i32::MAX as u32 || limit == MessageLimit::Queues && value > IPC_ID_SLOTS {
            return Err(IpcError::Invalid);
        }
        self.messages.lock().limits[limit.index()] = value;
        Ok(())
    }
    /// key lookup 和 slot/sequence allocation 在同一实际 registry 提交；全部 Arc/AVL storage 锁外准备。
    pub(crate) fn message_get(
        &self,
        key: i32,
        flags: u32,
        access: &IpcAccess,
        now: u64,
    ) -> Result<i32, IpcError> {
        let find = |registry: &MessageRegistry| -> Result<Option<i32>, IpcError> {
            let Some(queue) = registry.ids.get_key(key) else {
                return Ok(None);
            };
            if flags & (IPC_CREAT | IPC_EXCL) == (IPC_CREAT | IPC_EXCL) {
                return Err(IpcError::Exists);
            }
            queue.require(access, flags as u16 & 0o777)?;
            Ok(Some(queue.id))
        };
        {
            let registry = self.messages.lock();
            if let Some(id) = find(&registry)? {
                return Ok(id);
            }
            if key != 0 && flags & IPC_CREAT == 0 {
                return Err(IpcError::NotFound);
            }
        }
        let prepared = PreparedIpcObject::new(key)?;
        let mut registry = self.messages.lock();
        if let Some(id) = find(&registry)? {
            return Ok(id);
        }
        let identity = registry.ids.reserve(registry.limits[2] as usize)?;
        let queue = MessageQueue::new(
            identity.id,
            self.user.clone(),
            IpcPermission::new(key, flags as u16, access),
            registry.limits[1] as usize,
            now,
        );
        Ok(registry.ids.publish(prepared, identity, queue))
    }

    pub(crate) fn message_queue(&self, id: i32) -> Result<Arc<MessageQueue>, IpcError> {
        self.messages.lock().ids.get(id).cloned()
    }

    pub(crate) fn message_snapshot(
        &self,
        id: i32,
        index: bool,
        any: bool,
        access: &IpcAccess,
    ) -> Result<MessageQueueSnapshot, IpcError> {
        let queue = {
            let registry = self.messages.lock();
            if index {
                registry.ids.get_index(id)?.clone()
            } else {
                registry.ids.get(id)?.clone()
            }
        };
        queue.snapshot(access, any)
    }
    #[allow(
        clippy::too_many_arguments,
        reason = "同一次原子 publication 的独立领域输入，参数语义与所有权已逐项说明"
    )]
    pub(crate) fn message_update(
        &self,
        id: i32,
        uid: u32,
        gid: u32,
        mode: u16,
        limit: usize,
        access: &IpcAccess,
        now: u64,
    ) -> Result<(), IpcError> {
        if limit > self.message_limit(MessageLimit::QueueBytes) as usize
            && !access
                .authority
                .capable(&crate::namespace::initial_user(), Capability::SYS_RESOURCE)
        {
            return Err(IpcError::Permission);
        }
        self.message_queue(id)?
            .update(access, uid, gid, mode, limit, now)
    }
    /// IPC_INFO/MSG_INFO 从实际 queues 聚合；不再维护一份 namespace message byte/header counter。
    pub(crate) fn message_info(
        &self,
        usage: bool,
        access: &IpcAccess,
    ) -> Result<([i32; 7], u16, i32), IpcError> {
        let registry = self.messages.lock();
        let mut bytes = 0usize;
        let mut messages = 0usize;
        let mut maximum = 0;
        for (&slot, queue) in registry.ids.iter() {
            maximum = maximum.max(i32::from(slot));
            if usage {
                let snapshot = queue.snapshot(access, true)?;
                bytes = bytes.saturating_add(snapshot.bytes);
                messages = messages.saturating_add(snapshot.messages);
            }
        }
        let pool = if usage {
            registry.ids.len() as i32
        } else {
            32000 * 16384 / 1024
        };
        let map = if usage {
            messages.min(i32::MAX as usize) as i32
        } else {
            16384
        };
        let total = if usage {
            bytes.min(i32::MAX as usize) as i32
        } else {
            16384
        };
        Ok((
            [
                pool,
                map,
                registry.limits[0] as i32,
                registry.limits[1] as i32,
                registry.limits[2] as i32,
                16,
                total,
            ],
            65535,
            maximum,
        ))
    }
    pub(crate) fn message_remove(&self, id: i32, access: &IpcAccess) -> Result<(), IpcError> {
        let mut registry = self.messages.lock();
        let queue = registry.ids.get(id)?;
        let messages = queue.remove(access)?;
        let retired = registry.ids.remove(id)?;
        drop(registry);
        messages.finish();
        drop(retired);
        Ok(())
    }
    pub(crate) fn semaphore_limits(&self) -> [i32; 4] {
        self.semaphores.lock().limits
    }
    pub(crate) fn sysctl_value(&self, limit: IpcLimit) -> IpcLimitValue {
        match limit {
            IpcLimit::Message(limit) => IpcLimitValue::Message(self.message_limit(limit)),
            IpcLimit::Semaphores => IpcLimitValue::Semaphores(self.semaphore_limits()),
            IpcLimit::SharedMemory(limit) => {
                IpcLimitValue::SharedMemory(self.shared_memory_limit(limit))
            }
        }
    }
    /// Linux proc_dointvec 的 accepted prefix：前三项可为任意 int，semmni 越界只拒绝第四项。
    pub(crate) fn set_semaphore_limits(
        &self,
        values: &[Option<i32>; 4],
        caller: &crate::namespace::NamespaceAuthority,
    ) -> Result<(), IpcError> {
        if !self.may_set_limits(caller) {
            return Err(IpcError::Permission);
        }
        let mut registry = self.semaphores.lock();
        for (index, value) in values.iter().enumerate() {
            if let Some(value) = value {
                if index == 3 && !(0..=IPC_ID_SLOTS as i32).contains(value) {
                    return Err(IpcError::Range);
                }
                registry.limits[index] = *value;
            }
        }
        Ok(())
    }
    pub(crate) fn semaphore_get(
        &self,
        key: i32,
        count: i32,
        flags: u32,
        access: &IpcAccess,
        now: u64,
    ) -> Result<i32, IpcError> {
        let find = |registry: &SemaphoreRegistry| -> Result<Option<i32>, IpcError> {
            if count < 0 || count > registry.limits[0] {
                return Err(IpcError::Invalid);
            }
            let Some(set) = registry.ids.get_key(key) else {
                return Ok(None);
            };
            if flags & (IPC_CREAT | IPC_EXCL) == (IPC_CREAT | IPC_EXCL) {
                return Err(IpcError::Exists);
            }
            set.require(access, flags as u16 & 0o777)?;
            if count as usize > set.count() {
                return Err(IpcError::Invalid);
            }
            Ok(Some(set.id))
        };
        {
            let registry = self.semaphores.lock();
            if let Some(id) = find(&registry)? {
                return Ok(id);
            }
            if key != 0 && flags & IPC_CREAT == 0 {
                return Err(IpcError::NotFound);
            }
            if count == 0 {
                return Err(IpcError::Invalid);
            }
            if registry.used as i64 + i64::from(count) > i64::from(registry.limits[1]) {
                return Err(IpcError::NoSpace);
            }
        }
        let prepared = PreparedIpcObject::new(key)?;
        let values = PreparedSemaphores::new(count as usize)?;
        let mut registry = self.semaphores.lock();
        if let Some(id) = find(&registry)? {
            return Ok(id);
        }
        if registry.used as i64 + i64::from(count) > i64::from(registry.limits[1]) {
            return Err(IpcError::NoSpace);
        }
        let identity = registry.ids.reserve(registry.limits[3] as usize)?;
        let set = SemaphoreSet::new(
            identity.id,
            self.id,
            self.user.clone(),
            IpcPermission::new(key, flags as u16, access),
            values,
            now,
        );
        let id = registry.ids.publish(prepared, identity, set);
        registry.used += count as usize;
        Ok(id)
    }
    pub(crate) fn semaphore_set(&self, id: i32) -> Result<Arc<SemaphoreSet>, IpcError> {
        self.semaphores.lock().ids.get(id).cloned()
    }
    pub(crate) fn semaphore_snapshot(
        &self,
        id: i32,
        index: bool,
        any: bool,
        access: &IpcAccess,
    ) -> Result<SemaphoreSnapshot, IpcError> {
        let set = {
            let registry = self.semaphores.lock();
            if index {
                registry.ids.get_index(id)?.clone()
            } else {
                registry.ids.get(id)?.clone()
            }
        };
        set.snapshot(access, any)
    }
    pub(crate) fn semaphore_update(
        &self,
        id: i32,
        uid: u32,
        gid: u32,
        mode: u16,
        access: &IpcAccess,
        now: u64,
    ) -> Result<(), IpcError> {
        self.semaphore_set(id)?.update(uid, gid, mode, access, now)
    }
    pub(crate) fn semaphore_remove(&self, id: i32, access: &IpcAccess) -> Result<(), IpcError> {
        let mut registry = self.semaphores.lock();
        let set = registry.ids.get(id)?;
        let count = set.count();
        let semaphores = set.remove(access)?;
        let retired = registry.ids.remove(id)?;
        registry.used = registry
            .used
            .checked_sub(count)
            .expect("semaphore admission underflow");
        drop(registry);
        semaphores.finish();
        drop(retired);
        Ok(())
    }
    pub(crate) fn semaphore_info(&self, usage: bool) -> ([i32; 10], i32) {
        let registry = self.semaphores.lock();
        let [maximum_set, total, operations, sets] = registry.limits;
        let maximum = registry
            .ids
            .iter()
            .map(|(&slot, _)| i32::from(slot))
            .last()
            .unwrap_or(0);
        (
            [
                1024000000,
                sets,
                total,
                1024000000,
                maximum_set,
                operations,
                500,
                if usage { registry.ids.len() as i32 } else { 20 },
                32767,
                if usage {
                    registry.used.min(i32::MAX as usize) as i32
                } else {
                    32767
                },
            ],
            maximum,
        )
    }
}

impl Drop for IpcNamespace {
    fn drop(&mut self) {
        // 最后 Arc 已排除新的 namespace lookup；没有 registry guard，cleanup/wake 不反向持锁。
        for (_, queue) in self.messages.get_mut().ids.iter() {
            queue.destroy();
        }
        for (_, set) in self.semaphores.get_mut().ids.iter() {
            set.destroy();
        }
        for (_, segment) in self.shared_memory.get_mut().ids.iter() {
            drop(segment.destroy());
        }
    }
}
