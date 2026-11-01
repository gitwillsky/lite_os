//! PID namespace 的实际编号、reaper 与 allocation 生命周期；Task identity/graph 归 task。
use super::{
    NamespaceAuthority, NamespaceError, PidIdentity, UserNamespace, UserResource,
    UserResourceCharge,
};
use crate::fallible_tree::FallibleMap;
use alloc::sync::{Arc, Weak};
use spin::Mutex;

const RESERVED_PIDS: u32 = 300;
pub(crate) const PID_MAX_LIMIT: u32 = 4 * 1024 * 1024;

/// 不对用户暴露的稳定 task identity key；只由 task PID identity owner 构造。
/// 编号被回收后，新 identity 仍使用不同 key，旧 proc/fd/reference 不会误选新任务。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct PidKey(usize);
impl PidKey {
    pub(crate) const INIT: Self = Self(1);
    pub(crate) const fn from_identity(identity: usize) -> Self {
        Self(identity)
    }
    pub(crate) const fn identity(self) -> usize {
        self.0
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PidAllocationError {
    OutOfMemory,
    Exhausted,
    Invalid,
    Dead,
    Permission,
    Exists,
}

/// Creating 尚无 PID1；Live 允许创建；Dead 保留 reaper identity 供退出 drain，永久拒绝创建。
enum Lifecycle {
    Creating,
    Live(PidKey),
    Dead(PidKey),
}
/// 最后 association retirement 的 domain 通知 seam；namespace 不查 Task 或 scheduler。
/// 实际 shutdown event 由 graph 拥有，weak edge 不让 namespace 与 event waiter 形成 Arc cycle。
pub(crate) trait PidExitWake: Send + Sync {
    fn signal(&self);
}
/// 一个已发表的 native PID association；key 是同一 immutable identity 的索引投影。
/// Weak 不拥有 graph registration，也不延迟 local number 归还；无第二份 identity registry。
struct PidAssociation {
    key: PidKey,
    identity: Weak<PidIdentity>,
}
struct State {
    // OWNER: 唯一 local PID→稳定 identity association；None 是不可见的未发布 reservation。
    numbers: FallibleMap<u32, Option<PidAssociation>>,
    cursor: u32,
    limit: u32,
    minimum_limit: u32,
    lifecycle: Lifecycle,
    exit_wake: Option<Weak<dyn PidExitWake>>,
}
/// 实际 namespace owner；parent/user/identity 不变，可变 allocation tuple 由一个短锁拥有。
/// 锁内不分配、不查 graph、不 pin TCB。root PID1 由 boot 永久保留，global init 不允许退出。
pub(crate) struct PidNamespace {
    id: u64,
    level: u8,
    parent: Option<Arc<Self>>,
    user: Arc<UserNamespace>,
    state: Mutex<State>,
    _charge: Option<UserResourceCharge>,
}
impl core::fmt::Debug for PidNamespace {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PidNamespace")
            .field("id", &self.id)
            .field("level", &self.level)
            .finish_non_exhaustive()
    }
}
impl PidNamespace {
    pub(super) fn initial(
        user: Arc<UserNamespace>,
        possible_cpus: usize,
    ) -> Result<Arc<Self>, NamespaceError> {
        let mut numbers = FallibleMap::new();
        numbers
            .try_insert(1, None)
            .map_err(|_| NamespaceError::OutOfMemory)?;
        Arc::try_new(Self {
            id: 4,
            level: 0,
            parent: None,
            user,
            state: Mutex::new(State {
                numbers,
                cursor: 2,
                limit: (possible_cpus
                    .saturating_mul(1024)
                    .max(32_768)
                    .min(PID_MAX_LIMIT as usize)) as u32,
                minimum_limit: (possible_cpus
                    .saturating_mul(8)
                    .max(301)
                    .min(PID_MAX_LIMIT as usize)) as u32,
                lifecycle: Lifecycle::Live(PidKey::INIT),
                exit_wake: None,
            }),
            _charge: None,
        })
        .map_err(|_| NamespaceError::OutOfMemory)
    }
    /// owning user 必须在 parent 的 user namespace 之内；active==for_children 由 task owner 验证。
    pub(crate) fn child(
        self: &Arc<Self>,
        user: Arc<UserNamespace>,
        uid: u32,
    ) -> Result<Arc<Self>, NamespaceError> {
        if !user.is_within(&self.user) {
            return Err(NamespaceError::Invalid);
        }
        if self.level >= 32 {
            return Err(NamespaceError::NestingLimit);
        }
        let charge = UserResourceCharge::acquire(&user, uid, UserResource::Pid)?;
        let minimum_limit = self.state.lock().minimum_limit;
        Arc::try_new(Self {
            id: super::next_namespace_id(),
            level: self.level + 1,
            parent: Some(self.clone()),
            user,
            state: Mutex::new(State {
                numbers: FallibleMap::new(),
                cursor: 1,
                limit: PID_MAX_LIMIT,
                minimum_limit,
                lifecycle: Lifecycle::Creating,
                exit_wake: None,
            }),
            _charge: Some(charge),
        })
        .map_err(|_| NamespaceError::OutOfMemory)
    }
    pub(crate) fn id(&self) -> u64 {
        self.id
    }
    pub(crate) fn level(&self) -> u8 {
        self.level
    }
    pub(crate) fn parent(&self) -> Option<Arc<Self>> {
        self.parent.clone()
    }
    pub(crate) fn user_namespace(&self) -> Arc<UserNamespace> {
        self.user.clone()
    }
    pub(crate) fn is_descendant_of(&self, ancestor: &Self) -> bool {
        let mut current = self;
        loop {
            if core::ptr::eq(current, ancestor) {
                return true;
            }
            let Some(parent) = &current.parent else {
                return false;
            };
            current = parent;
        }
    }
    pub(crate) fn reaper(&self) -> Option<PidKey> {
        match self.state.lock().lifecycle {
            Lifecycle::Creating => None,
            Lifecycle::Live(key) | Lifecycle::Dead(key) => Some(key),
        }
    }
    /// /proc/loadavg 的 ns_last_pid；直接读取 allocation owner 的 cursor，不复制 counter。
    pub(crate) fn last_allocated(&self) -> u32 {
        self.state.lock().cursor.saturating_sub(1)
    }
    pub(crate) fn limit(&self) -> u32 {
        self.state.lock().limit
    }
    /// pid_max 为 namespace root 0644 sysctl；SYS_ADMIN 或映射后的实际 root euid 可写。
    /// caller 为此次操作的凭据快照；权限通过后才按 boot 的 possible CPU 下限检查范围。
    pub(crate) fn set_limit(
        &self,
        limit: u32,
        caller: &NamespaceAuthority,
    ) -> Result<(), NamespaceError> {
        if !self.may_set_limit(caller) {
            return Err(NamespaceError::Permission);
        }
        let mut state = self.state.lock();
        if !(state.minimum_limit..=PID_MAX_LIMIT).contains(&limit) {
            return Err(NamespaceError::Invalid);
        }
        state.limit = limit;
        Ok(())
    }
    /// pid namespace sysctl 的 root/SYS_ADMIN 权限；open 与每次 write 复查同一 owner。
    pub(crate) fn may_set_limit(&self, caller: &NamespaceAuthority) -> bool {
        caller.capable(&self.user, syscall_abi::capability::Capability::SYS_ADMIN)
            || self.user.to_kernel(true, 0) == Some(caller.effective_uid)
    }
    pub(crate) fn lookup(&self, number: u32) -> Option<PidKey> {
        self.state
            .lock()
            .numbers
            .get(&number)
            .and_then(|value| value.as_ref().map(|association| association.key))
    }
    /// 在实际 numbered association 内 pin 同一 PID 对象；包含仅被 PGID/SID links 持有的编号。
    /// 返回的是 observer，lookup 后的 exit/reuse 不会把它替换为新同号对象。
    /// 所有 namespace guards 在转交/退休 Arc 前释放，不查 Task graph 或扫描进程。
    pub(crate) fn lookup_identity(&self, number: u32) -> Option<Arc<PidIdentity>> {
        {
            let state = self.state.lock();
            state
                .numbers
                .get(&number)
                .and_then(|value| value.as_ref())
                .and_then(|association| association.identity.upgrade())
        }
    }
    /// boot 完成唯一 initial PID1 构造后，向已预留的实际 node 无分配发表 association。
    pub(super) fn publish_initial(&self, identity: &Arc<PidIdentity>) {
        assert!(self.parent.is_none() && identity.key() == PidKey::INIT.identity());
        assert_eq!(identity.number_in(self), 1);
        let mut state = self.state.lock();
        let entry = state.numbers.get_mut(&1).expect("boot reserved PID1 node");
        assert!(entry.is_none(), "initial PID identity published twice");
        *entry = Some(PidAssociation {
            key: PidKey::INIT,
            identity: Arc::downgrade(identity),
        });
    }
    /// init 退出只遍历此 owner 的实际 numbered members，不扫描不相关 Process。
    pub(crate) fn next_member(&self, after: u32) -> Option<(u32, PidKey)> {
        let state = self.state.lock();
        state
            .numbers
            .iter_after(&after)
            .find_map(|(&number, key)| key.as_ref().map(|association| (number, association.key)))
    }
    /// 每个 ancestor 的 node 在其锁外准备；部分 allocation 失败由 reservation Drop 归还。
    pub(crate) fn reserve(
        self: &Arc<Self>,
        key: PidKey,
        requested: Option<u32>,
    ) -> Result<PidReservation, PidAllocationError> {
        let mut entry =
            FallibleMap::try_prepare(0, None).map_err(|_| PidAllocationError::OutOfMemory)?;
        let mut state = self.state.lock();
        if matches!(state.lifecycle, Lifecycle::Creating) && state.numbers.is_empty() {
            state.cursor = 1;
        }
        let minimum = if state.cursor > RESERVED_PIDS {
            RESERVED_PIDS
        } else {
            1
        };
        let start = if state.cursor >= state.limit {
            minimum
        } else {
            state.cursor.max(minimum)
        };
        let number = if let Some(number) = requested {
            if number == 0 || number >= state.limit {
                return Err(PidAllocationError::Invalid);
            }
            if state.numbers.contains_key(&number) {
                return Err(PidAllocationError::Exists);
            }
            number
        } else {
            first_gap(&state.numbers, start, state.limit)
                .or_else(|| first_gap(&state.numbers, minimum, start))
                .ok_or(PidAllocationError::Exhausted)?
        };
        // 对照 alloc_pid：PID1 必须先创建；dead 检查在编号/其他错误之后，返回 ENOMEM。
        if matches!(state.lifecycle, Lifecycle::Creating) && number != 1 {
            return Err(PidAllocationError::Invalid);
        }
        if matches!(state.lifecycle, Lifecycle::Dead(_)) {
            return Err(PidAllocationError::Dead);
        }
        // 精确编号不移动 cyclic allocator cursor；否则 checkpoint restore 会改变后续普通 fork。
        if requested.is_none() {
            state.cursor = number + 1;
        }
        entry.set_key(number);
        state.numbers.commit_vacant(entry);
        drop(state);
        Ok(PidReservation {
            namespace: Some(self.clone()),
            number,
            key,
        })
    }
    pub(crate) fn allows_creation(&self) -> bool {
        !matches!(self.state.lock().lifecycle, Lifecycle::Dead(_))
    }
    /// graph 的最后一个 namespace init Thread 退出时调用；后续 fork 永久拒绝。
    pub(crate) fn disable(&self, reaper: PidKey, wake: Option<Weak<dyn PidExitWake>>) {
        assert!(self.parent.is_some(), "Attempted to kill global init");
        let mut state = self.state.lock();
        assert!(matches!(state.lifecycle, Lifecycle::Live(key) if key == reaper));
        state.lifecycle = Lifecycle::Dead(reaper);
        state.exit_wake = wake;
    }
    /// 当前 init key 的一至两个实际编号之外再无成员/预留，shutdown 可结束。
    pub(crate) fn reaper_only(&self, init_numbers: usize) -> bool {
        self.state.lock().numbers.len() == init_numbers
    }
    fn signal_exit(&self) {
        let wake = self.state.lock().exit_wake.clone();
        if let Some(wake) = wake.and_then(|wake| wake.upgrade()) {
            wake.signal();
        }
    }
    pub(crate) fn release(&self, number: u32, key: PidKey) {
        assert!(
            !(self.parent.is_none() && number == 1),
            "boot owns permanent initial PID1"
        );
        let retired = {
            let mut state = self.state.lock();
            assert_eq!(
                state
                    .numbers
                    .get(&number)
                    .map(|entry| entry.as_ref().map(|association| association.key)),
                Some(Some(key)),
                "PID association released twice or by a different identity"
            );
            state
                .numbers
                .take_entry(&number)
                .expect("registered PID disappeared")
        };
        drop(retired);
        self.signal_exit();
    }
}
fn first_gap(
    numbers: &FallibleMap<u32, Option<PidAssociation>>,
    start: u32,
    end: u32,
) -> Option<u32> {
    if start >= end {
        return None;
    }
    let mut candidate = start;
    for (&number, _) in numbers.iter_from(&start) {
        if number > candidate {
            break;
        }
        candidate += 1;
        if candidate >= end {
            return None;
        }
    }
    Some(candidate)
}
/// 不可见的 number reservation；Task 完整构造 identity 后才 publish，失败不遗留占用。
pub(crate) struct PidReservation {
    namespace: Option<Arc<PidNamespace>>,
    number: u32,
    key: PidKey,
}
impl PidReservation {
    pub(crate) fn number(&self) -> u32 {
        self.number
    }
    /// 只发表已有 node 的 scalar；返回永久 identity ladder 使用的 namespace/number。
    pub(crate) fn publish(
        mut self,
        identity: &Arc<PidIdentity>,
    ) -> Result<(Arc<PidNamespace>, u32), PidAllocationError> {
        assert_eq!(
            identity.key(),
            self.key.identity(),
            "association must use its prepared actual identity"
        );
        // reservation 不 pin task；init 可以在准备期间退出，commit 必须复查实际生命周期。
        // graph 调用者持同一 publication guard，让各 ancestor association 与 Task graph 同次发布。
        {
            let namespace = self
                .namespace
                .as_ref()
                .expect("PID reservation consumed twice");
            assert_eq!(
                identity.number_in(namespace),
                self.number,
                "association must belong to its actual PID ladder"
            );
            let mut state = namespace.state.lock();
            if matches!(state.lifecycle, Lifecycle::Dead(_)) {
                return Err(PidAllocationError::Dead);
            }
            let value = state
                .numbers
                .get_mut(&self.number)
                .expect("PID reservation disappeared");
            assert!(value.is_none());
            *value = Some(PidAssociation {
                key: self.key,
                identity: Arc::downgrade(identity),
            });
            if matches!(state.lifecycle, Lifecycle::Creating) {
                assert_eq!(self.number, 1, "first published PID must be namespace init");
                state.lifecycle = Lifecycle::Live(self.key);
            }
        }
        Ok((
            self.namespace.take().expect("live PID reservation"),
            self.number,
        ))
    }
}
impl Drop for PidReservation {
    fn drop(&mut self) {
        if let Some(namespace) = self.namespace.take() {
            let retired = {
                let mut state = namespace.state.lock();
                assert!(
                    state.numbers.get(&self.number).is_some_and(Option::is_none),
                    "unpublished PID reservation lost"
                );
                state
                    .numbers
                    .take_entry(&self.number)
                    .expect("PID reservation disappeared")
            };
            drop(retired);
            namespace.signal_exit();
        }
    }
}
