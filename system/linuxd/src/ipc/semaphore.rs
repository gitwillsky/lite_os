//! System V semaphore array 的唯一 value/PID/operation/wait/undo owner。
use super::sem_undo::{SemaphoreUndoEntry, UndoRegistration};
use super::{IpcAccess, IpcError, IpcPermission};
use crate::{
    execution::sync::{TaskEvent, WaitResult},
    fallible_tree::{FallibleMap, VacantEntry},
    namespace::{PidIdentity, UserNamespace},
};
use alloc::{sync::Arc, vec::Vec};
use spin::Mutex;
use syscall_abi::ipc::{GETNCNT, GETPID, GETVAL, GETZCNT, IPC_NOWAIT, SEM_UNDO, SEM_VALUE_MAX};

/// Linux sembuf 的语义输入；布局/用户指针只属于 syscall codec。
#[derive(Clone, Copy, Debug)]
pub(crate) struct SemaphoreChange {
    pub(crate) index: u16,
    pub(crate) delta: i16,
    pub(crate) flags: u16,
}
struct Semaphore {
    value: i32,
    pid: Option<Arc<PidIdentity>>,
}
/// 创建前预留完整 value array，不在 namespace/array 锁下分配。
pub(super) struct PreparedSemaphores(Vec<Semaphore>);
impl PreparedSemaphores {
    pub(super) fn new(count: usize) -> Result<Self, IpcError> {
        let mut values = Vec::new();
        values
            .try_reserve_exact(count)
            .map_err(|_| IpcError::NoMemory)?;
        values.resize_with(count, || Semaphore {
            value: 0,
            pid: None,
        });
        Ok(Self(values))
    }
}
pub(super) struct SemaphoreState {
    permission: IpcPermission,
    pub(super) removed: bool,
    values: Vec<Semaphore>,
    operation_time: u64,
    change_time: u64,
    waiters: FallibleMap<u64, Arc<PendingOperation>>,
    next_waiter: u64,
    // OWNER: actual cross-registration；entry 的 adjustments 不在这里复制。
    pub(super) undo: FallibleMap<usize, UndoRegistration>,
}
pub(crate) struct SemaphoreSet {
    pub(crate) id: i32,
    pub(super) namespace_id: u64,
    // Linux sem_nsems 是不可变对象 metadata；RMID 退休 payload 后仍可完成已开始的 codec preparation。
    count: usize,
    user: Arc<UserNamespace>,
    pub(super) state: Mutex<SemaphoreState>,
}
pub(crate) struct SemaphoreSnapshot {
    pub(crate) id: i32,
    pub(crate) permission: IpcPermission,
    pub(crate) operation_time: u64,
    pub(crate) change_time: u64,
    pub(crate) count: usize,
}
struct OperationState {
    changes: Vec<SemaphoreChange>,
    undo: Option<Arc<SemaphoreUndoEntry>>,
    pid: Arc<PidIdentity>,
    // 最多每项操作退一个旧 PID pin，所有 storage 在操作准备时预留；锁外统一 clear。
    retired_pids: Vec<Arc<PidIdentity>>,
    blocking: Option<usize>,
    complete: Option<Result<(), IpcError>>,
}
struct PendingOperation {
    event: TaskEvent,
    state: Mutex<OperationState>,
}
impl PendingOperation {
    fn retire_pids(&self) {
        let retired = core::mem::take(&mut self.state.lock().retired_pids);
        drop(retired);
    }
}

/// 一次 semop 的真实 operation 和可取消 admission，不能在 wake 后重读用户 sembuf。
pub(crate) struct SemaphoreOperation {
    set: Arc<SemaphoreSet>,
    pending: Arc<PendingOperation>,
    node: Option<VacantEntry<u64, Arc<PendingOperation>>>,
    admission: Option<u64>,
}
impl SemaphoreOperation {
    pub(crate) fn new(
        set: &Arc<SemaphoreSet>,
        changes: Vec<SemaphoreChange>,
        undo: Option<Arc<SemaphoreUndoEntry>>,
        pid: Arc<PidIdentity>,
    ) -> Result<Self, IpcError> {
        if changes.is_empty() {
            return Err(IpcError::Invalid);
        }
        let mut retired_pids = Vec::new();
        retired_pids
            .try_reserve_exact(changes.len())
            .map_err(|_| IpcError::NoMemory)?;
        let pending = Arc::try_new(PendingOperation {
            event: TaskEvent::new(),
            state: Mutex::new(OperationState {
                changes,
                undo,
                pid,
                retired_pids,
                blocking: None,
                complete: None,
            }),
        })
        .map_err(|_| IpcError::NoMemory)?;
        let node = FallibleMap::try_prepare(0, pending.clone()).map_err(|_| IpcError::NoMemory)?;
        Ok(Self {
            set: set.clone(),
            pending,
            node: Some(node),
            admission: None,
        })
    }
    #[cfg(test)]
    pub(crate) fn wait(&self) -> WaitResult {
        self.wait_until(None)
    }
    pub(crate) fn wait_until(&self, deadline_ns: Option<u64>) -> WaitResult {
        match deadline_ns {
            Some(deadline) => self.pending.event.wait_interruptible_until(deadline),
            None => self.pending.event.wait_interruptible(),
        }
    }
    /// 锁内先取消真实 admission 再取已完成结果；完成胜过信号和随后的 RMID。
    pub(crate) fn finish_wait(&mut self) -> Option<Result<(), IpcError>> {
        let mut state = self.set.state.lock();
        let retired = self
            .admission
            .take()
            .and_then(|key| state.waiters.take_entry(&key));
        let complete = self.pending.state.lock().complete.take();
        drop(state);
        drop(retired);
        self.pending.retire_pids();
        complete
    }
}
impl Drop for SemaphoreOperation {
    fn drop(&mut self) {
        let _ = self.finish_wait();
    }
}
enum AtomicAttempt {
    Complete,
    Blocked(usize),
    Failed(IpcError),
}

/// 未提交的变化仅存在于本锁事务；失败倒序恢复，PID/time/等待副作用只在全体成功后发布。
fn atomic(state: &mut SemaphoreState, operation: &mut OperationState) -> AtomicAttempt {
    let mut adjustments = operation
        .undo
        .as_ref()
        .map(|entry| entry.adjustments.lock());
    let mut failure = None;
    for (position, change) in operation.changes.iter().enumerate() {
        let semaphore = &mut state.values[usize::from(change.index)];
        let next = semaphore.value + i32::from(change.delta);
        let blocked = change.delta == 0 && semaphore.value != 0 || next < 0;
        let error = if blocked {
            if u32::from(change.flags) & IPC_NOWAIT != 0 {
                Some(AtomicAttempt::Failed(IpcError::Again))
            } else {
                Some(AtomicAttempt::Blocked(position))
            }
        } else if next > SEM_VALUE_MAX {
            Some(AtomicAttempt::Failed(IpcError::Range))
        } else if change.flags & SEM_UNDO != 0 {
            let undo = &adjustments.as_ref().expect("SEM_UNDO preparation").values;
            let next_undo = undo[usize::from(change.index)] - i32::from(change.delta);
            (!(-SEM_VALUE_MAX - 1..=SEM_VALUE_MAX).contains(&next_undo))
                .then_some(AtomicAttempt::Failed(IpcError::Range))
        } else {
            None
        };
        if let Some(error) = error {
            failure = Some((position, error));
            break;
        }
        semaphore.value = next;
        if change.flags & SEM_UNDO != 0 {
            adjustments.as_mut().expect("SEM_UNDO preparation").values
                [usize::from(change.index)] -= i32::from(change.delta);
        }
    }
    if let Some((position, failure)) = failure {
        for change in operation.changes[..position].iter().rev() {
            state.values[usize::from(change.index)].value -= i32::from(change.delta);
            if change.flags & SEM_UNDO != 0 {
                adjustments.as_mut().expect("SEM_UNDO preparation").values
                    [usize::from(change.index)] += i32::from(change.delta);
            }
        }
        return failure;
    }
    drop(adjustments);
    for change in &operation.changes {
        if let Some(old) = state.values[usize::from(change.index)]
            .pid
            .replace(operation.pid.clone())
        {
            operation.retired_pids.push(old);
        }
    }
    AtomicAttempt::Complete
}
fn wake(waiters: FallibleMap<u64, Arc<PendingOperation>>) {
    for waiter in waiters.values() {
        waiter.retire_pids();
        waiter.event.signal();
    }
}
impl SemaphoreSet {
    pub(super) fn new(
        id: i32,
        namespace_id: u64,
        user: Arc<UserNamespace>,
        permission: IpcPermission,
        values: PreparedSemaphores,
        now: u64,
    ) -> Self {
        Self {
            id,
            namespace_id,
            count: values.0.len(),
            user,
            state: Mutex::new(SemaphoreState {
                permission,
                removed: false,
                values: values.0,
                operation_time: 0,
                change_time: now,
                waiters: FallibleMap::new(),
                next_waiter: 0,
                undo: FallibleMap::new(),
            }),
        }
    }
    pub(crate) fn count(&self) -> usize {
        self.count
    }
    pub(super) fn require(&self, access: &IpcAccess, requested: u16) -> Result<(), IpcError> {
        let state = self.state.lock();
        if state.removed {
            return Err(IpcError::Removed);
        }
        state.permission.require(access, &self.user, requested)
    }
    /// 成功的值变化持续推进可完成的既有操作，FIFO 每轮扫描，复杂操作完成后重扫依赖。
    fn progress(state: &mut SemaphoreState, now: u64) -> FallibleMap<u64, Arc<PendingOperation>> {
        let mut retired = FallibleMap::new();
        let mut cursor = 0;
        loop {
            let Some((key, waiter)) = state
                .waiters
                .successor(&cursor)
                .map(|(key, waiter)| (*key, waiter.clone()))
            else {
                break;
            };
            let mut operation = waiter.state.lock();
            let result = atomic(state, &mut operation);
            let changed = operation.changes.iter().any(|change| change.delta != 0);
            match result {
                AtomicAttempt::Blocked(position) => {
                    operation.blocking = Some(position);
                    drop(operation);
                    cursor = key;
                }
                AtomicAttempt::Complete => {
                    operation.complete = Some(Ok(()));
                    operation.blocking = None;
                    state.operation_time = now;
                    drop(operation);
                    retired.commit_vacant(
                        state.waiters.take_entry(&key).expect("completed operation"),
                    );
                    cursor = if changed { 0 } else { key };
                }
                AtomicAttempt::Failed(error) => {
                    operation.complete = Some(Err(error));
                    operation.blocking = None;
                    drop(operation);
                    retired
                        .commit_vacant(state.waiters.take_entry(&key).expect("failed operation"));
                    cursor = key;
                }
            }
        }
        retired
    }
    /// 一次 permission/atomic/admission 事务；false 已登记真实 waiter，无变化无部分成功。
    pub(crate) fn operate(
        &self,
        operation: &mut SemaphoreOperation,
        access: &IpcAccess,
        now: u64,
    ) -> Result<bool, IpcError> {
        assert!(core::ptr::eq(self, operation.set.as_ref()));
        assert!(
            operation.node.is_some(),
            "semaphore operation submitted twice"
        );
        let mut state = self.state.lock();
        if state.removed {
            return Err(IpcError::Removed);
        }
        let mut pending = operation.pending.state.lock();
        if pending
            .changes
            .iter()
            .any(|change| usize::from(change.index) >= state.values.len())
        {
            return Err(IpcError::Index);
        }
        let requested = if pending.changes.iter().any(|change| change.delta != 0) {
            2
        } else {
            4
        };
        state.permission.require(access, &self.user, requested)?;
        match atomic(&mut state, &mut pending) {
            AtomicAttempt::Failed(error) => Err(error),
            AtomicAttempt::Blocked(position) => {
                let key = state.next_waiter.checked_add(1).ok_or(IpcError::NoSpace)?;
                let mut node = operation.node.take().expect("operation admitted twice");
                node.set_key(key);
                pending.blocking = Some(position);
                state.waiters.commit_vacant(node);
                state.next_waiter = key;
                operation.admission = Some(key);
                Ok(false)
            }
            AtomicAttempt::Complete => {
                let unused_admission = operation.node.take();
                state.operation_time = now;
                pending.blocking = None;
                drop(pending);
                let completed = Self::progress(&mut state, now);
                drop(state);
                operation.pending.retire_pids();
                drop(unused_admission);
                wake(completed);
                Ok(true)
            }
        }
    }
    pub(crate) fn snapshot(
        &self,
        access: &IpcAccess,
        any: bool,
    ) -> Result<SemaphoreSnapshot, IpcError> {
        let state = self.state.lock();
        if state.removed {
            return Err(IpcError::Removed);
        }
        if !any {
            state.permission.require(access, &self.user, 4)?;
        }
        Ok(self.describe_state(&state))
    }
    /// registry 已确认实际对象存活；proc 读取不受对象 DAC 限制。
    pub(super) fn description(&self) -> SemaphoreSnapshot {
        let state = self.state.lock();
        assert!(
            !state.removed,
            "IPC description requires live registry member"
        );
        self.describe_state(&state)
    }
    fn describe_state(&self, state: &SemaphoreState) -> SemaphoreSnapshot {
        SemaphoreSnapshot {
            id: self.id,
            permission: state.permission,
            operation_time: state.operation_time,
            change_time: state.change_time,
            count: state.values.len(),
        }
    }
    pub(crate) fn scalar(
        &self,
        index: usize,
        command: i32,
        access: &IpcAccess,
        view: &crate::namespace::PidNamespace,
    ) -> Result<i32, IpcError> {
        let state = self.state.lock();
        if state.removed {
            return Err(IpcError::Removed);
        }
        state.permission.require(access, &self.user, 4)?;
        let semaphore = state.values.get(index).ok_or(IpcError::Invalid)?;
        match command {
            GETVAL => Ok(semaphore.value),
            GETPID => Ok(semaphore
                .pid
                .as_ref()
                .map_or(0, |pid| pid.number_in(view) as i32)),
            GETNCNT | GETZCNT => Ok(state
                .waiters
                .values()
                .filter(|pending| {
                    let operation = pending.state.lock();
                    operation.blocking.is_some_and(|position| {
                        let change = operation.changes[position];
                        usize::from(change.index) == index
                            && (change.delta == 0) == (command == GETZCNT)
                    })
                })
                .count()
                .min(i32::MAX as usize) as i32),
            _ => Err(IpcError::Invalid),
        }
    }
    pub(crate) fn values(&self, access: &IpcAccess) -> Result<Vec<u16>, IpcError> {
        let count = self.count();
        let mut values = Vec::new();
        values
            .try_reserve_exact(count)
            .map_err(|_| IpcError::NoMemory)?;
        let state = self.state.lock();
        if state.removed {
            return Err(IpcError::Removed);
        }
        state.permission.require(access, &self.user, 4)?;
        values.extend(state.values.iter().map(|semaphore| semaphore.value as u16));
        Ok(values)
    }
    /// SETVAL/SETALL 预留 retirement 容量后单次发布；同时清除相应的所有实际 undo adjustments。
    pub(crate) fn set_values(
        &self,
        index: Option<usize>,
        values: &[u16],
        access: &IpcAccess,
        pid: &Arc<PidIdentity>,
        now: u64,
    ) -> Result<(), IpcError> {
        if values.iter().any(|&value| i32::from(value) > SEM_VALUE_MAX) {
            return Err(IpcError::Range);
        }
        let count = self.count();
        if index.is_some_and(|index| index >= count) || values.len() != index.map_or(count, |_| 1) {
            return Err(IpcError::Invalid);
        }
        let mut retired_pids = Vec::new();
        retired_pids
            .try_reserve_exact(values.len())
            .map_err(|_| IpcError::NoMemory)?;
        let mut state = self.state.lock();
        if state.removed {
            return Err(IpcError::Removed);
        }
        state.permission.require(access, &self.user, 2)?;
        for (position, &value) in values.iter().enumerate() {
            let position = index.unwrap_or(position);
            state.values[position].value = i32::from(value);
            if let Some(old) = state.values[position].pid.replace(pid.clone()) {
                retired_pids.push(old);
            }
            for registration in state.undo.values() {
                registration.entry.adjustments.lock().values[position] = 0;
            }
        }
        state.change_time = now;
        let completed = Self::progress(&mut state, now);
        drop(state);
        drop(retired_pids);
        wake(completed);
        Ok(())
    }
    pub(super) fn update(
        &self,
        uid: u32,
        gid: u32,
        mode: u16,
        access: &IpcAccess,
        now: u64,
    ) -> Result<(), IpcError> {
        let mut state = self.state.lock();
        if state.removed {
            return Err(IpcError::Removed);
        }
        state.permission.require_control(access, &self.user)?;
        state.permission.uid = uid;
        state.permission.gid = gid;
        state.permission.mode = mode & 0o777;
        state.change_time = now;
        // Linux 已 admission 的 semop 不因 IPC_SET 重新做 DAC；这里只变 permission/ctime。
        Ok(())
    }
    pub(super) fn remove(&self, access: &IpcAccess) -> Result<RetiredSemaphores, IpcError> {
        let mut state = self.state.lock();
        state.permission.require_control(access, &self.user)?;
        Ok(Self::retire(&mut state))
    }
    fn retire(state: &mut SemaphoreState) -> RetiredSemaphores {
        state.removed = true;
        let undo = super::sem_undo::retire_registrations(&mut state.undo);
        let waiters = core::mem::take(&mut state.waiters);
        for waiter in waiters.values() {
            waiter.state.lock().complete = Some(Err(IpcError::Removed));
        }
        RetiredSemaphores {
            waiters,
            undo,
            values: core::mem::take(&mut state.values),
        }
    }
    pub(super) fn destroy(&self) {
        let retired = Self::retire(&mut self.state.lock());
        retired.finish();
    }
    /// 最后实际 SEM_UNDO participant 退出：在同一 array transaction 中夹限、更新 PID 并推进等待。
    pub(super) fn apply_undo(&self, entry: &SemaphoreUndoEntry, pid: &Arc<PidIdentity>, now: u64) {
        let mut state = self.state.lock();
        let key = core::ptr::from_ref(entry).addr();
        let registration = state.undo.take_entry(&key);
        if state.removed {
            drop(state);
            drop(registration);
            return;
        }
        assert!(
            registration.is_some(),
            "SEM_UNDO applied without live array registration"
        );
        let mut adjustments = entry.adjustments.lock();
        for position in 0..adjustments.values.len() {
            let adjustment = adjustments.values[position];
            if adjustment == 0 {
                continue;
            }
            let semaphore = &mut state.values[position];
            semaphore.value = (semaphore.value + adjustment).clamp(0, SEM_VALUE_MAX);
            if let Some(old) = semaphore.pid.replace(pid.clone()) {
                adjustments.retired_pids.push(old);
            }
        }
        drop(adjustments);
        state.operation_time = now;
        let completed = Self::progress(&mut state, now);
        drop(state);
        drop(registration);
        let retired_pids = core::mem::take(&mut entry.adjustments.lock().retired_pids);
        drop(retired_pids);
        wake(completed);
    }
}
impl super::ids::IpcObject for SemaphoreSet {
    fn id(&self) -> i32 {
        self.id
    }
    fn key(&self) -> i32 {
        self.state.lock().permission.key
    }
}
pub(super) struct RetiredSemaphores {
    waiters: FallibleMap<u64, Arc<PendingOperation>>,
    undo: FallibleMap<usize, UndoRegistration>,
    values: Vec<Semaphore>,
}
impl RetiredSemaphores {
    pub(super) fn finish(self) {
        wake(self.waiters);
        drop(self.undo);
        drop(self.values);
    }
}
