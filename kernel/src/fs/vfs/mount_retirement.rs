//! Mount 森林的唯一退休 owner：构造时预留节点，最终 Drop 不等待、不分配、不改拓扑。
use super::{
    FileSystemError, VirtualFileSystem, mount_index,
    mount_view::{Attachment, MountPayload, MountView},
};
use crate::sync::{TaskEvent, TaskMutexWaitPreparation};
use alloc::{boxed::Box, sync::Arc, vec::Vec};
use spin::Mutex;

pub(super) enum Work {
    Namespace(Option<Arc<MountView>>),
    View(MountPayload),
    Attachment(Attachment),
}
/// Box 同时是实际 resource payload 与预分配 queue node；不另建 dummy mount 或资源副本。
pub(super) struct Retirement {
    next: Option<Box<Retirement>>,
    work: Option<Work>,
    // View Arc allocation identity，仅供 exact Weak retirement，不解引用已销毁的 Arc。
    pub(super) identity: usize,
}
impl Retirement {
    pub(super) fn for_view(payload: MountPayload) -> Result<Box<Self>, FileSystemError> {
        Box::try_new(Self {
            next: None,
            work: Some(Work::View(payload)),
            identity: 0,
        })
        .map_err(|_| FileSystemError::OutOfMemory)
    }
    /// 实际 placement edge 本身预留 queue link；detach/move 直接消费该 owner，不临时分配清理节点。
    pub(super) fn for_attachment(edge: Attachment) -> Result<Box<Self>, FileSystemError> {
        Box::try_new(Self {
            next: None,
            work: Some(Work::Attachment(edge)),
            identity: 0,
        })
        .map_err(|_| FileSystemError::OutOfMemory)
    }
    pub(super) fn attachment(&self) -> &Attachment {
        match self.work.as_ref().expect("live placement edge") {
            Work::Attachment(edge) => edge,
            _ => unreachable!("retirement work is not an attachment"),
        }
    }
    pub(super) fn for_namespace() -> Result<Box<Self>, FileSystemError> {
        Box::try_new(Self {
            next: None,
            work: Some(Work::Namespace(None)),
            identity: 0,
        })
        .map_err(|_| FileSystemError::OutOfMemory)
    }
    pub(super) fn view(&self) -> &MountPayload {
        match self.work.as_ref().expect("live retirement work") {
            Work::View(payload) => payload,
            Work::Namespace(_) | Work::Attachment(_) => {
                unreachable!("retirement token is not a view")
            }
        }
    }
    pub(super) fn install_namespace(&mut self, root: Arc<MountView>) {
        let Some(Work::Namespace(target)) = &mut self.work else {
            panic!("namespace retirement token kind");
        };
        assert!(target.is_none(), "namespace retired twice");
        *target = Some(root);
    }
}
impl Drop for Retirement {
    fn drop(&mut self) {
        // 正常 batch 先摘 next；未发表失败/异常路径同样按常数栈消费 tail，不能递归 Box Drop。
        let mut tail = self.next.take();
        while let Some(mut item) = tail.take() {
            tail = item.next.take();
            drop(item);
        }
        if let Some(work) = self.work.take() {
            match work {
                Work::View(payload) => payload.release(),
                Work::Namespace(root) => drop(root),
                Work::Attachment(edge) => drop(edge),
            }
        }
    }
}
// OWNER: intrusive resource retirement queue；short spin 只转移已有 Box，所有 consequential Drop 锁外。
static PENDING: Mutex<Option<Box<Retirement>>> = Mutex::new(None);
static READY: TaskEvent = TaskEvent::new();
static STARTED: spin::Once<()> = spin::Once::new();
pub(super) fn has_pending() -> bool {
    PENDING.lock().is_some()
}
pub(super) fn enqueue(mut item: Box<Retirement>) {
    {
        let mut pending = PENDING.lock();
        item.next = pending.take();
        *pending = Some(item);
    }
    READY.signal();
}
/// syscall准备好的真实卸载集合；不成为持久forest副本。unlock记录Linux传播根的解锁职责。
pub(super) struct PreparedDetach {
    pub(super) views: Vec<Arc<MountView>>,
    pub(super) unlock: Vec<Arc<MountView>>,
    pub(super) synchronous: bool,
}
impl PreparedDetach {
    pub(super) fn new(
        mut views: Vec<Arc<MountView>>,
        mut unlock: Vec<Arc<MountView>>,
        synchronous: bool,
    ) -> Self {
        views.sort_unstable_by_key(|view| Arc::as_ptr(view) as usize);
        unlock.sort_unstable_by_key(|view| Arc::as_ptr(view) as usize);
        unlock.dedup_by(|left, right| Arc::ptr_eq(left, right));
        Self {
            views,
            unlock,
            synchronous,
        }
    }
    pub(super) fn contains(&self, view: &MountView) -> bool {
        self.views
            .binary_search_by_key(&(view as *const MountView as usize), |member| {
                Arc::as_ptr(member) as usize
            })
            .is_ok()
    }
    pub(super) fn disconnects(&self, view: &MountView) -> bool {
        self.synchronous
            || !view.state.lock().restrictions.inseparable
            || self
                .unlock
                .binary_search_by_key(&(view as *const MountView as usize), |member| {
                    Arc::as_ptr(member) as usize
                })
                .is_ok()
            || !view.attachment().is_some_and(|edge| {
                self.views
                    .binary_search_by_key(&(edge.parent.as_ptr() as usize), |member| {
                        Arc::as_ptr(member) as usize
                    })
                    .is_ok()
            })
    }
    /// 全部index/placement/容量准备成功后调用；不递归删整棵树，surviving overmount已移出集合。
    pub(super) fn publish(self) {
        for view in &self.unlock {
            view.state.lock().restrictions.inseparable = false;
        }
        for view in &self.views {
            view.retire_namespace();
        }
        for view in &self.views {
            if !self.disconnects(view) {
                continue;
            }
            mount_index::detach(view);
            if let Some(parent) = view.attachment().and_then(|edge| edge.parent.upgrade()) {
                let owner = {
                    let mut state = parent.state.lock();
                    let index = state
                        .children
                        .iter()
                        .position(|child| Arc::ptr_eq(child, view))
                        .expect("prepared detach owns actual edge");
                    state.children.remove(index)
                };
                drop(owner);
            }
            let attachment = view.state.lock().attachment.take();
            if let Some(attachment) = attachment {
                enqueue(attachment);
            }
        }
    }
}
/// 字段析构按链逐个执行，深森林的 final resource release 保持常数调用栈。
pub(super) struct RetiredBatch(Option<Box<Retirement>>);
impl Drop for RetiredBatch {
    fn drop(&mut self) {
        while let Some(mut item) = self.0.take() {
            self.0 = item.next.take();
            drop(item);
        }
    }
}
/// Caller 已持唯一 VFS gate；移动 actual nodes 构建 worklist，无新分配或递归 traversal。
/// MNT_DETACH 断开普通边；仅 parent 也退休时保留 locked/inseparable 子边。
pub(super) fn detach_forest(root: &Arc<MountView>) {
    let mut pending = Some(root.clone());
    while let Some(view) = pending.take() {
        view.retire_namespace();
        {
            let mut state = view.state.lock();
            pending = state.retirement_next.take();
            for child in &state.children {
                let mut child_state = child.state.lock();
                assert!(
                    child_state.retirement_next.is_none(),
                    "mount retirement worklist reused"
                );
                child_state.retirement_next = pending.take();
                pending = Some(child.clone());
            }
        }
        let attachment = view.attachment();
        let parent = attachment.as_ref().and_then(|edge| edge.parent.upgrade());
        let preserve = !Arc::ptr_eq(&view, root)
            && view.state.lock().restrictions.inseparable
            && parent
                .as_ref()
                .is_some_and(|parent| !parent.state.lock().namespace_member);
        if !preserve {
            mount_index::detach(&view);
            if let Some(parent) = parent {
                let retired = {
                    let mut state = parent.state.lock();
                    state
                        .children
                        .iter()
                        .position(|child| Arc::ptr_eq(child, &view))
                        .map(|index| state.children.remove(index))
                };
                drop(retired); // view/worklist 仍持实际 child，不能触发 final resource Drop。
            }
            let retired = view.state.lock().attachment.take();
            if let Some(edge) = retired {
                enqueue(edge);
            }
        }
    }
}
/// Admission 清理先前已提交的退休项；resource chain 留在 guard 后面的字段，解锁后才析构。
pub(super) fn drain() -> RetiredBatch {
    let mut head = PENDING.lock().take();
    let mut cursor = head.as_mut();
    while let Some(item) = cursor {
        match item.work.as_ref().expect("queued retirement work") {
            Work::Namespace(Some(root)) => detach_forest(root),
            Work::Namespace(None) | Work::Attachment(_) => {} // 尚未发表 constructor 的真实失败 token。
            Work::View(payload) => {
                if let Some(key) = payload.state.lock().registration.take() {
                    mount_index::unregister(key, item.identity as *const MountView);
                }
                let state = payload.state.lock();
                for child in &state.children {
                    detach_forest(child);
                }
            }
        }
        cursor = item.next.as_mut();
    }
    RetiredBatch(head)
}
impl VirtualFileSystem {
    /// Boot 在 root filesystem 前创建唯一退休线程；waiter/closure 先准备，后续 cleanup 无 OOM。
    pub(in crate::fs) fn start_mount_retirement(
        &'static self,
        threads: super::super::KernelThreadSupport,
    ) -> Result<(), FileSystemError> {
        assert!(
            STARTED.get().is_none(),
            "mount retirement worker started twice"
        );
        let mut preparation =
            TaskMutexWaitPreparation::prepare().map_err(|_| FileSystemError::OutOfMemory)?;
        let body = Box::try_new(move || {
            loop {
                READY.wait_idle();
                loop {
                    let guard = self.namespace_mutation.lock_prepared(&mut preparation);
                    let retired = drain();
                    drop(guard);
                    drop(retired);
                    if !has_pending() {
                        break;
                    }
                }
            }
        })
        .map_err(|_| FileSystemError::OutOfMemory)?;
        (threads.spawn)("mount-retire", body).map_err(|_| FileSystemError::OutOfMemory)?;
        STARTED.call_once(|| ());
        Ok(())
    }
}
