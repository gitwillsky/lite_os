//! shm registry 的 admission、key 撤销与最后 VMA 销毁；锁序始终 registry → segment。
use super::super::SharedMemoryCreatorList;
use super::super::shared_memory_creator::{CreatorMembership, PreparedCreator};
use super::super::{SharedMemoryAttachReservation, SharedMemorySegment, SharedMemorySnapshot};
use super::*;
use crate::{
    memory::{PAGE_SIZE, SharedMemoryBacking},
    namespace::PidIdentity,
};

impl IpcNamespace {
    /// 标准 SHM_LOCK/UNLOCK：policy gate 等待和 ucounts preparation 均在短 guard 外。
    pub(crate) fn shared_memory_lock(
        &self,
        id: i32,
        locking: bool,
        access: &IpcAccess,
        real_uid: u32,
        read_limit: impl Fn() -> u64,
    ) -> Result<(), IpcError> {
        let initial_limit = read_limit();
        let (segment, backing) = {
            let registry = self.shared_memory.lock();
            let segment = registry.ids.get(id)?.clone();
            let backing = segment.require_lock(access, locking, initial_limit)?;
            (segment, backing)
        };
        let policy = backing
            .freeze_swap_policy()
            .map_err(|_| IpcError::NoMemory)?;
        // policy 等待允许 prlimit 改变实际 owner；收费读取等待结束后的当前 soft limit。
        let limit = read_limit();
        drop(segment.require_lock(access, locking, limit)?);
        let mut prepared_charge = if locking && !backing.locked() {
            let exempt = access
                .authority
                .capable(&crate::namespace::initial_user(), Capability::IPC_LOCK);
            let pages_limit = if limit == u64::MAX {
                u64::MAX
            } else {
                limit / PAGE_SIZE as u64
            };
            Some(
                crate::namespace::RlimitCharge::acquire(
                    access.authority.namespace.clone(),
                    real_uid,
                    crate::namespace::RlimitResource::LockedMemory,
                    Self::shared_memory_pages(&segment) as u64,
                    pages_limit,
                    exempt,
                )
                .map_err(|_| IpcError::NoMemory)?,
            )
        } else {
            None
        };
        let result = {
            let registry = self.shared_memory.lock();
            (|| {
                let actual = registry.ids.get(id).map_err(|_| IpcError::Removed)?;
                if !Arc::ptr_eq(actual, &segment) {
                    return Err(IpcError::Removed);
                }
                let retired =
                    segment.publish_lock(&backing, locking, &mut prepared_charge, access, limit)?;
                policy.set_locked(locking);
                Ok(retired)
            })()
        };
        // 成功/commit 失败的实际 charge 在全部 guard 外退休。preparation 的部分收费失败
        // 可在 policy gate 下回滚，但不持 registry/segment，且凭据 pin 全部祖先 namespace；
        // quotas 不会获取此 gate，因而没有反向锁序或 namespace 最后析构。
        drop(policy);
        drop(prepared_charge);
        drop(result?);
        Ok(())
    }
    /// 创建/取得实际 segment；Arc、页 owner 和索引 storage 在 registry guard 外准备。
    #[allow(
        clippy::too_many_arguments,
        reason = "同一次原子 publication 的独立领域输入，参数语义与所有权已逐项说明"
    )]
    pub(crate) fn shared_memory_get(
        self: &Arc<Self>,
        key: i32,
        size: usize,
        flags: u32,
        access: &IpcAccess,
        pid: Arc<PidIdentity>,
        now: u64,
        creator: &Arc<SharedMemoryCreatorList>,
    ) -> Result<i32, IpcError> {
        let find = |registry: &SharedMemoryRegistry| -> Result<Option<i32>, IpcError> {
            let Some(segment) = registry.ids.get_key(key) else {
                return Ok(None);
            };
            if flags & (IPC_CREAT | IPC_EXCL) == (IPC_CREAT | IPC_EXCL) {
                return Err(IpcError::Exists);
            }
            segment.require(access, flags as u16 & 0o777)?;
            if size > segment.size {
                return Err(IpcError::Invalid);
            }
            Ok(Some(segment.id))
        };
        let pages = size.checked_add(PAGE_SIZE - 1).ok_or(IpcError::Invalid)? / PAGE_SIZE;
        let admit = |registry: &SharedMemoryRegistry| -> Result<(), IpcError> {
            // 当前没有配置 hugetlb hstate/pool；与 Linux hstate_sizelog(None) 同样拒绝。
            if flags & syscall_abi::ipc::SHM_HUGETLB != 0 {
                return Err(IpcError::Invalid);
            }
            if size == 0 || size > registry.maximum_size {
                return Err(IpcError::Invalid);
            }
            if registry
                .used_pages
                .checked_add(pages)
                .is_none_or(|total| total > registry.maximum_pages)
            {
                return Err(IpcError::NoSpace);
            }
            Ok(())
        };
        {
            let registry = self.shared_memory.lock();
            if let Some(id) = find(&registry)? {
                return Ok(id);
            }
            if key != 0 && flags & IPC_CREAT == 0 {
                return Err(IpcError::NotFound);
            }
            admit(&registry)?;
        }
        let prepared = PreparedIpcObject::new(key)?;
        let prepared_creator = PreparedCreator::new()?;
        let backing = SharedMemoryBacking::allocate(pages).map_err(|_| IpcError::NoMemory)?;
        // 竞争/限额失败时不消费 preparation，节点和 backing 的析构晚于 registry guard。
        let mut pending = Some((prepared, prepared_creator, backing, pid));
        let result = {
            let mut registry = self.shared_memory.lock();
            (|| {
                if let Some(id) = find(&registry)? {
                    return Ok(id);
                }
                admit(&registry)?;
                let identity = registry.ids.reserve(registry.maximum_segments)?;
                let (prepared, prepared_creator, backing, pid) =
                    pending.take().expect("unpublished shm preparation");
                let segment = SharedMemorySegment::new(
                    identity.id,
                    size,
                    self.user.clone(),
                    IpcPermission::new(key, flags as u16, access),
                    backing,
                    pid,
                    now,
                    CreatorMembership::new(creator, self.id, identity.id),
                );
                let id = registry.ids.publish(prepared, identity, segment);
                prepared_creator.publish(
                    creator,
                    self,
                    registry.ids.get(id).expect("newly published shm"),
                );
                registry.used_pages += pages;
                Ok(id)
            })()
        };
        drop(pending);
        result
    }
    pub(crate) fn shared_memory_snapshot(
        &self,
        id: i32,
        index: bool,
        any: bool,
        access: &IpcAccess,
    ) -> Result<SharedMemorySnapshot, IpcError> {
        let segment = {
            let registry = self.shared_memory.lock();
            if index {
                registry.ids.get_index(id)?.clone()
            } else {
                registry.ids.get(id)?.clone()
            }
        };
        segment.snapshot(access, any)
    }
    pub(crate) fn shared_memory_update(
        &self,
        id: i32,
        uid: u32,
        gid: u32,
        mode: u16,
        access: &IpcAccess,
        now: u64,
    ) -> Result<(), IpcError> {
        let segment = self.shared_memory.lock().ids.get(id)?.clone();
        segment.update(access, uid, gid, mode, now)
    }
    /// RMID 的实际 ID 只在 nattch 为零时消失；key 立即可被新 segment 重新使用。
    pub(crate) fn shared_memory_remove(&self, id: i32, access: &IpcAccess) -> Result<(), IpcError> {
        let (segment, key, retired, backing) = {
            let mut registry = self.shared_memory.lock();
            let segment = registry.ids.get(id)?.clone();
            segment.require_control(access)?;
            let key = registry.ids.withdraw_key(id)?;
            let destroy = segment.mark_destroy();
            let (retired, backing) = if destroy {
                registry.used_pages -= Self::shared_memory_pages(&segment);
                (Some(registry.ids.remove(id)?), Some(segment.destroy()))
            } else {
                (None, None)
            };
            (segment, key, retired, backing)
        };
        drop((segment, key, retired, backing));
        Ok(())
    }
    pub(crate) fn shared_memory_prepare_attach(
        self: &Arc<Self>,
        id: i32,
        requested: u16,
        access: &IpcAccess,
    ) -> Result<SharedMemoryAttachReservation, IpcError> {
        let registry = self.shared_memory.lock();
        let segment = registry.ids.get(id)?.clone();
        segment.reserve_attach(access, requested)?;
        Ok(SharedMemoryAttachReservation {
            namespace: self.clone(),
            segment,
        })
    }
    /// 每次新增 VMA 都有既存 reservation/VMA 保活；无分配、不能复活旧 ID observer。
    pub(crate) fn shared_memory_open(
        &self,
        segment: &Arc<SharedMemorySegment>,
        pid: Arc<PidIdentity>,
        now: u64,
    ) {
        let retired_pid = {
            let registry = self.shared_memory.lock();
            assert!(
                registry
                    .ids
                    .get(segment.id)
                    .is_ok_and(|actual| Arc::ptr_eq(actual, segment)),
                "opening retired shm identity"
            );
            segment.open(pid, now)
        };
        drop(retired_pid);
    }
    /// 最后实际 VMA/reservation close 退休 ID、实际页和 AVL storage，全部在 guard 外析构。
    pub(crate) fn shared_memory_close(
        &self,
        segment: &Arc<SharedMemorySegment>,
        actor: Option<(Arc<PidIdentity>, u64)>,
    ) {
        let (retired_pid, retired, backing) = {
            let mut registry = self.shared_memory.lock();
            assert!(
                registry
                    .ids
                    .get(segment.id)
                    .is_ok_and(|actual| Arc::ptr_eq(actual, segment)),
                "closing retired shm identity"
            );
            let (destroy, retired_pid) = segment.close(actor, registry.forced_remove);
            let (retired, backing) = if destroy {
                registry.used_pages -= Self::shared_memory_pages(segment);
                (
                    Some(
                        registry
                            .ids
                            .remove(segment.id)
                            .expect("actual shm identity"),
                    ),
                    Some(segment.destroy()),
                )
            } else {
                (None, None)
            };
            (retired_pid, retired, backing)
        };
        drop((retired_pid, retired, backing));
    }
    fn shared_memory_pages(segment: &SharedMemorySegment) -> usize {
        segment.size.div_ceil(PAGE_SIZE)
    }
    /// IPC_INFO 按实际 namespace 限额编码，不读取全局 fallback。
    pub(crate) fn shared_memory_limits(&self) -> ([u64; 5], i32) {
        let registry = self.shared_memory.lock();
        let highest = registry
            .ids
            .iter()
            .map(|(&slot, _)| i32::from(slot))
            .last()
            .unwrap_or(0);
        (
            [
                registry.maximum_size as u64,
                1,
                registry.maximum_segments as u64,
                registry.maximum_segments as u64,
                registry.maximum_pages as u64,
            ],
            highest,
        )
    }
    /// SHM_INFO 的 namespace admission 与实际 backing 内容；swap 历史字段标准为零。
    pub(crate) fn shared_memory_info(&self) -> (i32, [u64; 3], i32) {
        let registry = self.shared_memory.lock();
        let (mut resident, mut swapped, mut highest) = (0, 0, 0);
        for (&slot, segment) in registry.ids.iter() {
            highest = i32::from(slot);
            let (segment_resident, segment_swapped) = segment.statistics();
            resident += segment_resident;
            swapped += segment_swapped;
        }
        (
            registry.ids.len() as i32,
            [registry.used_pages as u64, resident as u64, swapped as u64],
            highest,
        )
    }
    pub(crate) fn shared_memory_limit(&self, limit: SharedMemoryLimit) -> u64 {
        let registry = self.shared_memory.lock();
        match limit {
            SharedMemoryLimit::MaximumSize => registry.maximum_size as u64,
            SharedMemoryLimit::TotalPages => registry.maximum_pages as u64,
            SharedMemoryLimit::Segments => registry.maximum_segments as u64,
            SharedMemoryLimit::ForcedRemove => u64::from(registry.forced_remove),
        }
    }
    pub(crate) fn set_shared_memory_limit(
        &self,
        limit: SharedMemoryLimit,
        value: u64,
        access: &crate::namespace::NamespaceAuthority,
    ) -> Result<(), IpcError> {
        if !self.may_set_limits(access) {
            return Err(IpcError::Permission);
        }
        let value = usize::try_from(value).map_err(|_| IpcError::Invalid)?;
        let mut registry = self.shared_memory.lock();
        match limit {
            SharedMemoryLimit::MaximumSize => registry.maximum_size = value,
            SharedMemoryLimit::TotalPages => registry.maximum_pages = value,
            SharedMemoryLimit::Segments if value <= IPC_ID_SLOTS as usize => {
                registry.maximum_segments = value
            }
            SharedMemoryLimit::Segments => return Err(IpcError::Range),
            SharedMemoryLimit::ForcedRemove if value <= 1 => registry.forced_remove = value != 0,
            SharedMemoryLimit::ForcedRemove => return Err(IpcError::Invalid),
        }
        drop(registry);
        if limit == SharedMemoryLimit::ForcedRemove && value != 0 {
            self.shared_memory_destroy_orphans();
        }
        Ok(())
    }
    /// 创建线程的真实 clist 已退休；没有挂接且 forced=1 才立即撤销 ID/key。
    pub(in crate::ipc) fn shared_memory_reap_orphan(&self, segment: &Arc<SharedMemorySegment>) {
        let retirement = {
            let mut registry = self.shared_memory.lock();
            if !registry
                .ids
                .get(segment.id)
                .is_ok_and(|actual| Arc::ptr_eq(actual, segment))
                || !segment.may_destroy_orphan(registry.forced_remove)
            {
                return;
            }
            registry.used_pages -= Self::shared_memory_pages(segment);
            (
                registry.ids.remove(segment.id).expect("actual orphan shm"),
                segment.destroy(),
            )
        };
        drop(retirement);
    }
    fn shared_memory_destroy_orphans(&self) {
        // 单调 slot cursor 扫描真实 registry；不分配全表 snapshot，不从首节点重复搜索。
        let mut cursor = 0u32;
        while cursor < IPC_ID_SLOTS {
            let retirement = {
                let mut registry = self.shared_memory.lock();
                if !registry.forced_remove {
                    return;
                }
                let Some((slot, segment)) = registry
                    .ids
                    .iter_from(cursor as u16)
                    .find(|(_, segment)| segment.may_destroy_orphan(true))
                    .map(|(&slot, segment)| (slot, segment.clone()))
                else {
                    return;
                };
                cursor = u32::from(slot) + 1;
                registry.used_pages -= Self::shared_memory_pages(&segment);
                (
                    segment.clone(),
                    registry.ids.remove(segment.id).expect("actual orphan shm"),
                    segment.destroy(),
                )
            };
            drop(retirement);
        }
    }
}
