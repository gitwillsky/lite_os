//! System V segment 的权限、实际 VMA nattch 与延迟销毁 owner；观察者 Arc 不计挂接。
use super::shared_memory_creator::{CreatorMembership, RetiredCreator};
use super::{IpcAccess, IpcError, IpcNamespace, IpcPermission};
use crate::{
    memory::SharedMemoryBacking,
    namespace::{PidIdentity, UserNamespace},
};
use alloc::sync::Arc;
use spin::Mutex;
use syscall_abi::{
    capability::Capability,
    ipc::{SHM_DEST, SHM_LOCKED},
};

struct SegmentState {
    permission: IpcPermission,
    backing: Option<Arc<SharedMemoryBacking>>,
    attaches: usize,
    creator: Arc<PidIdentity>,
    last_operator: Option<Arc<PidIdentity>>,
    attach_time: u64,
    detach_time: u64,
    change_time: u64,
    lock_charge: Option<crate::namespace::RlimitCharge>,
}
/// 标准 shmid 的实际对象；ipc_perm 与 nattch 只在此维护，ID/size 创建后不变。
pub(crate) struct SharedMemorySegment {
    pub(crate) id: i32,
    pub(crate) size: usize,
    user: Arc<UserNamespace>,
    state: Mutex<SegmentState>,
    creator_membership: CreatorMembership,
}
/// ID/payload/clist 已摘除之后的实际资源；只能移交到所有 guard 外析构。
pub(super) struct SharedMemoryRetirement {
    _backing: Option<Arc<SharedMemoryBacking>>,
    _creator: RetiredCreator,
    _lock_charge: Option<crate::namespace::RlimitCharge>,
}
impl core::fmt::Debug for SharedMemorySegment {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SharedMemorySegment")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}
pub(crate) struct SharedMemorySnapshot {
    pub(crate) id: i32,
    pub(crate) size: usize,
    pub(crate) permission: IpcPermission,
    pub(crate) attaches: usize,
    pub(crate) creator: Arc<PidIdentity>,
    pub(crate) last_operator: Option<Arc<PidIdentity>>,
    pub(crate) attach_time: u64,
    pub(crate) detach_time: u64,
    pub(crate) change_time: u64,
}
impl super::ids::IpcObject for SharedMemorySegment {
    fn id(&self) -> i32 {
        self.id
    }
    fn key(&self) -> i32 {
        self.state.lock().permission.key
    }
}
impl SharedMemorySegment {
    #[allow(
        clippy::too_many_arguments,
        reason = "同一次原子 publication 的独立领域输入，参数语义与所有权已逐项说明"
    )]
    pub(super) fn new(
        id: i32,
        size: usize,
        user: Arc<UserNamespace>,
        permission: IpcPermission,
        backing: Arc<SharedMemoryBacking>,
        creator: Arc<PidIdentity>,
        now: u64,
        creator_membership: CreatorMembership,
    ) -> Self {
        Self {
            id,
            size,
            user,
            state: Mutex::new(SegmentState {
                permission,
                backing: Some(backing),
                attaches: 0,
                creator,
                last_operator: None,
                attach_time: 0,
                detach_time: 0,
                change_time: now,
                lock_charge: None,
            }),
            creator_membership,
        }
    }
    pub(super) fn require(&self, access: &IpcAccess, requested: u16) -> Result<(), IpcError> {
        let state = self.state.lock();
        Self::live(&state)?;
        state.permission.require(access, &self.user, requested)
    }
    fn live(state: &SegmentState) -> Result<(), IpcError> {
        state.backing.as_ref().map(|_| ()).ok_or(IpcError::Removed)
    }
    pub(crate) fn snapshot(
        &self,
        access: &IpcAccess,
        any: bool,
    ) -> Result<SharedMemorySnapshot, IpcError> {
        let state = self.state.lock();
        Self::live(&state)?;
        if !any {
            state.permission.require(access, &self.user, 4)?;
        }
        Ok(self.describe_state(&state))
    }
    /// 同一 segment guard 读取 metadata 与真实 backing 页状态；不增加 VMA membership。
    pub(super) fn description(&self) -> (SharedMemorySnapshot, usize, usize) {
        let state = self.state.lock();
        let backing = state
            .backing
            .as_ref()
            .expect("IPC description requires live registry member");
        let (resident, swapped) = backing.shared_memory_statistics();
        (self.describe_state(&state), resident, swapped)
    }
    fn describe_state(&self, state: &SegmentState) -> SharedMemorySnapshot {
        let mut permission = state.permission;
        if state
            .backing
            .as_ref()
            .is_some_and(|backing| backing.locked())
        {
            permission.mode |= SHM_LOCKED;
        }
        SharedMemorySnapshot {
            id: self.id,
            size: self.size,
            permission,
            attaches: state.attaches,
            creator: state.creator.clone(),
            last_operator: state.last_operator.clone(),
            attach_time: state.attach_time,
            detach_time: state.detach_time,
            change_time: state.change_time,
        }
    }
    pub(crate) fn update(
        &self,
        access: &IpcAccess,
        uid: u32,
        gid: u32,
        mode: u16,
        now: u64,
    ) -> Result<(), IpcError> {
        let mut state = self.state.lock();
        Self::live(&state)?;
        state.permission.require_control(access, &self.user)?;
        state.permission.uid = uid;
        state.permission.gid = gid;
        state.permission.mode = (state.permission.mode & !0o777) | (mode & 0o777);
        state.change_time = now;
        Ok(())
    }
    pub(super) fn require_control(&self, access: &IpcAccess) -> Result<(), IpcError> {
        let state = self.state.lock();
        Self::live(&state)?;
        state.permission.require_control(access, &self.user)
    }
    /// 外层 registry 同锁撤销 key 后标记 DEST；已有 VMA 允许继续由原 ID 挂接。
    pub(super) fn mark_destroy(&self) -> bool {
        let mut state = self.state.lock();
        state.permission.key = 0;
        state.permission.mode |= SHM_DEST;
        state.attaches == 0
    }
    pub(super) fn reserve_attach(
        &self,
        access: &IpcAccess,
        requested: u16,
    ) -> Result<(), IpcError> {
        let mut state = self.state.lock();
        Self::live(&state)?;
        state.permission.require(access, &self.user, requested)?;
        state.attaches = state.attaches.checked_add(1).expect("shm nattch exhausted");
        Ok(())
    }
    pub(crate) fn backing(&self) -> Arc<SharedMemoryBacking> {
        self.state
            .lock()
            .backing
            .as_ref()
            .expect("reserved shm backing")
            .clone()
    }
    pub(super) fn statistics(&self) -> (usize, usize) {
        self.state
            .lock()
            .backing
            .as_ref()
            .map_or((0, 0), |backing| backing.shared_memory_statistics())
    }
    pub(super) fn require_lock(
        &self,
        access: &IpcAccess,
        locking: bool,
        limit: u64,
    ) -> Result<Arc<SharedMemoryBacking>, IpcError> {
        let state = self.state.lock();
        self.require_lock_state(&state, access, locking, limit)?;
        Ok(state.backing.as_ref().expect("live shm backing").clone())
    }
    fn require_lock_state(
        &self,
        state: &SegmentState,
        access: &IpcAccess,
        locking: bool,
        limit: u64,
    ) -> Result<(), IpcError> {
        Self::live(state)?;
        if !access.authority.capable(&self.user, Capability::IPC_LOCK)
            && (access.authority.effective_uid != state.permission.uid
                && access.authority.effective_uid != state.permission.creator_uid
                || locking && limit == 0)
        {
            return Err(IpcError::Permission);
        }
        Ok(())
    }
    /// 外层已持实际 policy gate；移交的 charge 才能改变唯一 unevictable fact。
    pub(super) fn publish_lock(
        &self,
        backing: &Arc<SharedMemoryBacking>,
        locking: bool,
        charge: &mut Option<crate::namespace::RlimitCharge>,
        access: &IpcAccess,
        limit: u64,
    ) -> Result<Option<crate::namespace::RlimitCharge>, IpcError> {
        let mut state = self.state.lock();
        self.require_lock_state(&state, access, locking, limit)?;
        assert!(
            Arc::ptr_eq(state.backing.as_ref().expect("live shm backing"), backing),
            "shm backing changed"
        );
        if locking {
            if state.lock_charge.is_none() {
                state.lock_charge = Some(charge.take().expect("new SHM_LOCK charge"));
            }
            Ok(None)
        } else {
            Ok(state.lock_charge.take())
        }
    }
    pub(super) fn open(&self, pid: Arc<PidIdentity>, now: u64) -> Option<Arc<PidIdentity>> {
        let mut state = self.state.lock();
        assert!(
            state.attaches != 0 && state.backing.is_some(),
            "shm VMA requires live attachment owner"
        );
        state.attaches = state.attaches.checked_add(1).expect("shm nattch exhausted");
        state.attach_time = now;
        state.last_operator.replace(pid)
    }
    pub(super) fn close(
        &self,
        actor: Option<(Arc<PidIdentity>, u64)>,
        forced: bool,
    ) -> (bool, Option<Arc<PidIdentity>>) {
        let mut state = self.state.lock();
        let retired = if let Some((pid, now)) = actor {
            state.detach_time = now;
            state.last_operator.replace(pid)
        } else {
            None
        };
        state.attaches = state.attaches.checked_sub(1).expect("shm nattch underflow");
        (
            state.attaches == 0 && (forced || state.permission.mode & SHM_DEST != 0),
            retired,
        )
    }
    /// 必须移交到 registry/MM guard 外析构；缓存的 segment Arc 不延长实际页 lifetime。
    pub(super) fn may_destroy_orphan(&self, forced: bool) -> bool {
        forced && self.creator_membership.orphaned() && self.state.lock().attaches == 0
    }
    pub(super) fn destroy(&self) -> SharedMemoryRetirement {
        let (backing, lock_charge) = {
            let mut state = self.state.lock();
            assert_eq!(state.attaches, 0, "destroying attached shm");
            (state.backing.take(), state.lock_charge.take())
        };
        SharedMemoryRetirement {
            _backing: backing,
            _creator: self.creator_membership.detach(),
            _lock_charge: lock_charge,
        }
    }
}
/// shmat 的临时 nattch reservation；跨 user-copy/MM preparation 防止 RMID 提前销毁。
/// 其退出只归还 reservation，不伪造 VMA 的 dtime/PID 更新。
pub(crate) struct SharedMemoryAttachReservation {
    pub(crate) namespace: Arc<IpcNamespace>,
    pub(crate) segment: Arc<SharedMemorySegment>,
}
impl Drop for SharedMemoryAttachReservation {
    fn drop(&mut self) {
        self.namespace.shared_memory_close(&self.segment, None);
    }
}
