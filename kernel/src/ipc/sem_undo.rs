//! SEM_UNDO 实际共享 membership、adjustments 与 array/list 交叉注册。
use super::{IpcError, SemaphoreSet};
use crate::{
    fallible_tree::{FallibleMap, VacantEntry},
    namespace::PidIdentity,
};
use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};
use spin::Mutex;

type UndoKey = (u64, i32);
pub(super) struct UndoAdjustments {
    pub(super) values: Vec<i32>,
    pub(super) retired_pids: Vec<Arc<PidIdentity>>,
}
/// 唯一 semadj storage；list 与实际 array 注册同一 Arc，反向 owner edges 都是 Weak。
pub(crate) struct SemaphoreUndoEntry {
    set: Weak<SemaphoreSet>,
    list: Weak<SemaphoreUndoList>,
    key: UndoKey,
    pub(super) adjustments: Mutex<UndoAdjustments>,
}
struct UndoListState {
    // OWNER: 实际参与 Thread 的 membership 数；operation 的 Arc observer 不参与退出撤销。
    members: usize,
    entries: FallibleMap<UndoKey, Arc<SemaphoreUndoEntry>>,
}
pub(crate) struct SemaphoreUndoList {
    state: Mutex<UndoListState>,
}
/// Thread 独占的 membership，CLONE_SYSVSEM 可共享下层 list；普通 fork 不继承 semadj。
pub(crate) struct SemaphoreUndoHandle {
    list: Arc<SemaphoreUndoList>,
    active: bool,
}
impl core::fmt::Debug for SemaphoreUndoHandle {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SemaphoreUndoHandle")
            .field("active", &self.active)
            .finish_non_exhaustive()
    }
}
impl SemaphoreUndoHandle {
    pub(crate) fn new() -> Result<Self, IpcError> {
        Ok(Self {
            list: Arc::try_new(SemaphoreUndoList {
                state: Mutex::new(UndoListState {
                    members: 1,
                    entries: FallibleMap::new(),
                }),
            })
            .map_err(|_| IpcError::NoMemory)?,
            active: true,
        })
    }
    pub(crate) fn fork(&self, share: bool) -> Result<Self, IpcError> {
        assert!(self.active, "clone retired SEM_UNDO membership");
        if !share {
            return Self::new();
        }
        let mut state = self.list.state.lock();
        state.members = state.members.checked_add(1).ok_or(IpcError::NoSpace)?;
        Ok(Self {
            list: self.list.clone(),
            active: true,
        })
    }
    pub(crate) fn list(&self) -> Arc<SemaphoreUndoList> {
        assert!(self.active, "observe retired SEM_UNDO membership");
        self.list.clone()
    }
    /// 只有最后实际 participant 才取走 entries；退出 pid/time 是该次 detach 的输入，不存第二快照。
    pub(crate) fn retire(&mut self, pid: &Arc<PidIdentity>, now: u64) {
        assert!(self.active, "SEM_UNDO membership retired twice");
        self.active = false;
        if let Some(entries) = self.list.detach() {
            for entry in entries.values() {
                if let Some(set) = entry.set.upgrade() {
                    set.apply_undo(entry, pid, now);
                }
            }
            drop(entries);
        }
    }
}
impl Drop for SemaphoreUndoHandle {
    fn drop(&mut self) {
        if self.active {
            // 未发布 clone/constructor 的回滚可以归还空 membership；真实退出必须带 pid 显式 retire。
            let entries = self.list.detach();
            assert!(
                entries.as_ref().is_none_or(FallibleMap::is_empty),
                "live SEM_UNDO dropped without exit transaction"
            );
            drop(entries);
        }
    }
}
impl SemaphoreUndoList {
    fn detach(&self) -> Option<FallibleMap<UndoKey, Arc<SemaphoreUndoEntry>>> {
        let mut state = self.state.lock();
        state.members = state
            .members
            .checked_sub(1)
            .expect("SEM_UNDO membership underflow");
        (state.members == 0).then(|| core::mem::take(&mut state.entries))
    }
    /// 双索引 preparation 在锁外；publication 唯一锁序 array→list，双方引用同一 actual entry。
    pub(crate) fn entry(
        self: &Arc<Self>,
        set: &Arc<SemaphoreSet>,
    ) -> Result<Arc<SemaphoreUndoEntry>, IpcError> {
        let key = (set.namespace_id, set.id);
        if let Some(entry) = self.state.lock().entries.get(&key).cloned() {
            return Ok(entry);
        }
        let count = set.count();
        let mut values = Vec::new();
        values
            .try_reserve_exact(count)
            .map_err(|_| IpcError::NoMemory)?;
        values.resize(count, 0);
        let mut retired_pids = Vec::new();
        retired_pids
            .try_reserve_exact(count)
            .map_err(|_| IpcError::NoMemory)?;
        let entry = Arc::try_new(SemaphoreUndoEntry {
            set: Arc::downgrade(set),
            list: Arc::downgrade(self),
            key,
            adjustments: Mutex::new(UndoAdjustments {
                values,
                retired_pids,
            }),
        })
        .map_err(|_| IpcError::NoMemory)?;
        let list_node = FallibleMap::try_reserve_node().map_err(|_| IpcError::NoMemory)?;
        let set_node = FallibleMap::try_reserve_node().map_err(|_| IpcError::NoMemory)?;
        let mut array = set.state.lock();
        if array.removed {
            return Err(IpcError::Removed);
        }
        let mut list = self.state.lock();
        if list.members == 0 {
            return Err(IpcError::Removed);
        }
        if let Some(entry) = list.entries.get(&key) {
            return Ok(entry.clone());
        }
        array.undo.commit_vacant(set_node.fill(
            Arc::as_ptr(&entry) as usize,
            UndoRegistration {
                entry: entry.clone(),
                retired_list: None,
                retired_list_node: None,
            },
        ));
        list.entries
            .commit_vacant(list_node.fill(key, entry.clone()));
        Ok(entry)
    }
}
/// 实际 array 的注册及其 RMID 锁外 retirement storage；Live 时两项 retired 字段始终为空。
pub(super) struct UndoRegistration {
    pub(super) entry: Arc<SemaphoreUndoEntry>,
    retired_list: Option<Arc<SemaphoreUndoList>>,
    retired_list_node: Option<VacantEntry<UndoKey, Arc<SemaphoreUndoEntry>>>,
}
/// array guard 内原子摘下每个 list 注册；所有 Arc/tree node Drop 推迟到外层 registry guard 外。
pub(super) fn retire_registrations(
    registrations: &mut FallibleMap<usize, UndoRegistration>,
) -> FallibleMap<usize, UndoRegistration> {
    let mut retired = FallibleMap::new();
    while let Some((&key, _)) = registrations.first_key_value() {
        let mut node = registrations
            .take_entry(&key)
            .expect("actual undo registration");
        let entry = &node.value().entry;
        if let Some(list) = entry.list.upgrade() {
            let list_node = list.state.lock().entries.take_entry(&entry.key);
            node.value_mut().retired_list_node = list_node;
            node.value_mut().retired_list = Some(list);
        }
        retired.commit_vacant(node);
    }
    retired
}
