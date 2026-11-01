//! creating Thread 的真实 shm clist；Weak observer 不代表仍活着的创建者。
use super::{IpcError, IpcNamespace, SharedMemorySegment};
use crate::fallible_tree::{FallibleMap, NodeSlot, VacantEntry};
use crate::namespace::NamespaceOwner;
use alloc::sync::{Arc, Weak};
use spin::Mutex;

type CreatorKey = (u64, i32);
struct CreatorRegistration {
    namespace: Weak<IpcNamespace>,
    segment: Weak<SharedMemorySegment>,
}
/// 每个 Thread 独立的创建登记；fork/CLONE_THREAD 不继承，exec 保留，实际退出显式退休。
pub(crate) struct SharedMemoryCreatorList {
    // OWNER: Some 是实际 creating Thread，None 是已退休；额外 Arc 不延长这个阶段。
    state: Mutex<Option<FallibleMap<CreatorKey, CreatorRegistration>>>,
}
impl core::fmt::Debug for SharedMemoryCreatorList {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SharedMemoryCreatorList")
            .finish_non_exhaustive()
    }
}
impl SharedMemoryCreatorList {
    pub(crate) fn new() -> Result<Arc<Self>, IpcError> {
        Arc::try_new(Self {
            state: Mutex::new(Some(FallibleMap::new())),
        })
        .map_err(|_| IpcError::NoMemory)
    }
    pub(crate) fn retire(&self) {
        let Some(registrations) = self.state.lock().take() else {
            return;
        };
        // 创建者先一次退出 membership，再在 list guard 外按实际 namespace 执行 forced cleanup。
        for (_, registration) in &registrations {
            if let Some(namespace) = registration.namespace.upgrade()
                && let Some(segment) = registration.segment.upgrade()
            {
                namespace.shared_memory_reap_orphan(&segment);
            }
        }
    }
}
impl Drop for SharedMemoryCreatorList {
    fn drop(&mut self) {
        // 实际 Thread 必须先 retire；fallback 不在未知 caller guard 下反向进入 namespace。
        assert!(
            self.state
                .get_mut()
                .as_ref()
                .is_none_or(FallibleMap::is_empty),
            "shm creating Thread omitted retirement"
        );
    }
}
/// 创建前在 namespace/list guard 外预留实际 clist 节点。
pub(super) struct PreparedCreator(NodeSlot<CreatorKey, CreatorRegistration>);
impl PreparedCreator {
    pub(super) fn new() -> Result<Self, IpcError> {
        FallibleMap::try_reserve_node()
            .map(Self)
            .map_err(|_| IpcError::NoMemory)
    }
    pub(super) fn publish(
        self,
        creator: &Arc<SharedMemoryCreatorList>,
        namespace: &Arc<IpcNamespace>,
        segment: &Arc<SharedMemorySegment>,
    ) {
        creator
            .state
            .lock()
            .as_mut()
            .expect("creating Thread still live")
            .commit_vacant(self.0.fill(
                (namespace.id(), segment.id),
                CreatorRegistration {
                    namespace: Arc::downgrade(namespace),
                    segment: Arc::downgrade(segment),
                },
            ));
    }
}
/// segment 对实际 clist 的只读身份与销毁时撤销 capability；不是 creator-alive mirror。
pub(super) struct CreatorMembership {
    owner: Weak<SharedMemoryCreatorList>,
    key: CreatorKey,
}
pub(super) struct RetiredCreator(
    #[allow(dead_code)] Option<VacantEntry<CreatorKey, CreatorRegistration>>,
);
impl CreatorMembership {
    pub(super) fn new(owner: &Arc<SharedMemoryCreatorList>, namespace: u64, id: i32) -> Self {
        Self {
            owner: Arc::downgrade(owner),
            key: (namespace, id),
        }
    }
    pub(super) fn orphaned(&self) -> bool {
        self.owner.upgrade().is_none_or(|owner| {
            owner
                .state
                .lock()
                .as_ref()
                .is_none_or(|entries| !entries.contains_key(&self.key))
        })
    }
    pub(super) fn detach(&self) -> RetiredCreator {
        let entry = self.owner.upgrade().and_then(|owner| {
            owner
                .state
                .lock()
                .as_mut()
                .and_then(|entries| entries.take_entry(&self.key))
        });
        RetiredCreator(entry)
    }
}
