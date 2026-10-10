//! Linux propagate_umount：最大non-shifting/non-revealing子集及surviving overmount重接。
use super::{
    Attachment, FileSystemError, MountView, mount_index, mount_propagation, mount_publication,
    mount_retirement,
};
use crate::fallible_tree::FallibleMap;
use alloc::{sync::Arc, vec::Vec};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Status {
    Candidate,
    Committed,
    Excluded,
}
struct Candidate {
    view: Arc<MountView>,
    status: Status,
    seen: bool,
    trimmed: bool,
}
struct Selection {
    entries: Vec<Candidate>,
    index: FallibleMap<usize, usize>,
    original: usize,
}
impl Selection {
    fn add(&mut self, view: Arc<MountView>, status: Status) -> Result<usize, FileSystemError> {
        let key = Arc::as_ptr(&view) as usize;
        if let Some(index) = self.index.get(&key) {
            return Ok(*index);
        }
        self.entries
            .try_reserve(1)
            .map_err(|_| FileSystemError::OutOfMemory)?;
        let index = self.entries.len();
        self.index
            .try_insert(key, index)
            .map_err(|_| FileSystemError::OutOfMemory)?;
        self.entries.push(Candidate {
            view,
            status,
            seen: false,
            trimmed: false,
        });
        Ok(index)
    }
    fn index(&self, view: &MountView) -> Option<usize> {
        self.index
            .get(&(view as *const MountView as usize))
            .copied()
    }
    fn status(&self, view: &MountView) -> Status {
        self.index(view)
            .map_or(Status::Excluded, |index| self.entries[index].status)
    }
    fn parent(&self, index: usize) -> Option<Arc<MountView>> {
        self.entries[index]
            .view
            .attachment()
            .and_then(|edge| edge.parent.upgrade())
    }
    fn trim_ancestors(&mut self, mut child: usize) {
        while let Some(parent) = self.parent(child) {
            let Some(parent_index) = self.index(&parent) else {
                break;
            };
            if self.entries[parent_index].status != Status::Candidate || self.entries[child].trimmed
            {
                break;
            }
            self.entries[child].trimmed = true;
            if !self.entries[child]
                .view
                .attachment()
                .is_some_and(|edge| Arc::ptr_eq(&edge.point, &parent.root))
            {
                self.entries[parent_index].status = Status::Excluded;
            }
            child = parent_index;
        }
    }
    fn trim(&mut self, index: usize, unlocked: &[Arc<MountView>]) {
        if self.entries[index].status != Status::Candidate {
            return;
        }
        let view = self.entries[index].view.clone();
        let count = view.state.lock().children.len();
        let mut found = false;
        let mut forbidden = false;
        let mut remaining = false;
        for offset in 0..count {
            let child = view.state.lock().children[offset].clone();
            match self.status(&child) {
                Status::Committed => continue,
                Status::Candidate => remaining = true,
                Status::Excluded => {
                    remaining = true;
                    found = true;
                    if !child
                        .attachment()
                        .is_some_and(|edge| Arc::ptr_eq(&edge.point, &view.root))
                    {
                        forbidden = true;
                        break;
                    }
                }
            }
        }
        if found {
            self.trim_ancestors(index);
        }
        if forbidden {
            self.entries[index].status = Status::Excluded;
        } else if !remaining && !locked(&view, unlocked) {
            self.entries[index].status = Status::Committed;
        }
    }
    fn handle_locked(&mut self, start: usize, unlocked: &[Arc<MountView>], chain: &mut Vec<usize>) {
        chain.clear();
        let mut current = Some(self.entries[start].view.clone());
        let mut committed = 0;
        while let Some(view) = current {
            let Some(index) = self.index(&view) else {
                break;
            };
            match self.entries[index].status {
                Status::Committed => {
                    committed = chain.len();
                    break;
                }
                Status::Excluded => break,
                Status::Candidate => {}
            }
            // backing已按全部candidate数预留，phase内部没有分配或递归栈。
            chain.push(index);
            self.entries[index].status = Status::Excluded;
            if !locked(&view, unlocked) {
                committed = chain.len();
            }
            current = self.parent(index);
        }
        for index in &chain[..committed] {
            self.entries[*index].status = Status::Committed;
        }
    }
}
fn locked(view: &MountView, unlocked: &[Arc<MountView>]) -> bool {
    view.state.lock().restrictions.inseparable
        && !unlocked
            .iter()
            .any(|member| core::ptr::eq(Arc::as_ptr(member), view))
}

/// Caller持mutation gate且已完成primary busy/expiry准入。selection/placement/OOM都在效果前。
pub(super) fn unmount(
    root: &Arc<MountView>,
    synchronous: bool,
) -> Result<Vec<Arc<crate::sync::TaskEvent>>, FileSystemError> {
    let original = root.snapshot_subtree()?;
    let mut selected = Selection {
        entries: Vec::new(),
        index: FallibleMap::new(),
        original: original.len(),
    };
    for view in original {
        selected.add(view, Status::Committed)?;
    }
    let mut unlock = Vec::new();
    let mut traversed = FallibleMap::new();
    for index in 0..selected.original {
        if selected.entries[index].seen {
            continue;
        }
        selected.entries[index].seen = true;
        let Some(edge) = selected.entries[index].view.attachment() else {
            continue;
        };
        let parent = edge
            .parent
            .upgrade()
            .ok_or(FileSystemError::InvalidOperation)?;
        let identity = mount_propagation::group(&parent)
            .map_or((false, Arc::as_ptr(&parent) as u64), |group| {
                (true, group.id)
            });
        let key = (identity, Arc::as_ptr(&edge.point) as usize);
        if traversed.get(&key).is_some() {
            continue;
        }
        traversed
            .try_insert(key, ())
            .map_err(|_| FileSystemError::OutOfMemory)?;
        for receiver in mount_propagation::receivers(&parent)? {
            if Arc::ptr_eq(&receiver, &parent) {
                continue;
            }
            let Some(child) = mount_index::child(&receiver, &edge.point) else {
                continue;
            };
            if index == 0 {
                unlock
                    .try_reserve(1)
                    .map_err(|_| FileSystemError::OutOfMemory)?;
                unlock.push(child.clone());
            }
            let candidate = selected.add(child, Status::Candidate)?;
            selected.entries[candidate].seen = true;
        }
    }
    // propagate_mount_busy只检查原root的cognates且忽略有独立child的接收端。
    // selection/unlock各额外pin一次；真实namespace owning edge一次，其余均是外部busy租约。
    if synchronous {
        for view in &unlock {
            if selected
                .index(view)
                .is_some_and(|index| index < selected.original)
            {
                continue;
            }
            let (count, over) = {
                let state = view.state.lock();
                (state.children.len(), state.children.first().cloned())
            };
            if count != 0
                && (count != 1
                    || !over.is_some_and(|child| {
                        child
                            .attachment()
                            .is_some_and(|edge| Arc::ptr_eq(&edge.point, &view.root))
                    }))
            {
                continue;
            }
            if Arc::strong_count(view) > 3 {
                return Err(FileSystemError::Busy);
            }
        }
    }
    // Linux candidate链用head插入；逆序扫描同一临时owner集合，exact索引只记录cold决策状态。
    for index in (selected.original..selected.entries.len()).rev() {
        selected.trim(index, &unlock);
    }
    let mut chain = Vec::new();
    chain
        .try_reserve_exact(selected.entries.len())
        .map_err(|_| FileSystemError::OutOfMemory)?;
    for index in (selected.original..selected.entries.len()).rev() {
        selected.handle_locked(index, &unlock, &mut chain);
    }
    // 在新增Placement/ParentCapacity pins之前判定真实外部租约；Linux允许有独立child的
    // cognate跳过busy引用检查，不能等这些合法FD关闭。只等待无external pin的实际view。
    let mut completions = Vec::new();
    if synchronous {
        completions
            .try_reserve_exact(selected.entries.len())
            .map_err(|_| FileSystemError::OutOfMemory)?;
        for candidate in &selected.entries {
            if candidate.status != Status::Committed {
                continue;
            }
            let known = 2 // actual parent owning edge + selection pin
                + usize::from(Arc::ptr_eq(&candidate.view, root)) // caller's actual path
                + unlock.iter().filter(|view| Arc::ptr_eq(view, &candidate.view)).count();
            if Arc::strong_count(&candidate.view) == known {
                completions.push(candidate.view.complete.clone());
            }
        }
    }
    let mut placements = Vec::new();
    for candidate in &selected.entries {
        if candidate.status != Status::Committed {
            continue;
        }
        let count = candidate.view.state.lock().children.len();
        for offset in 0..count {
            let survivor = candidate.view.state.lock().children[offset].clone();
            if selected.status(&survivor) == Status::Committed {
                continue;
            }
            let mut current = survivor.clone();
            let destination = loop {
                let edge = current
                    .attachment()
                    .ok_or(FileSystemError::InvalidOperation)?;
                let parent = edge
                    .parent
                    .upgrade()
                    .ok_or(FileSystemError::InvalidOperation)?;
                if selected.status(&parent) != Status::Committed {
                    break Attachment::new(&parent, edge.point);
                }
                current = parent;
            };
            placements
                .try_reserve(1)
                .map_err(|_| FileSystemError::OutOfMemory)?;
            placements.push(mount_publication::Placement::prepare(
                survivor,
                destination,
            )?);
        }
    }
    let mut views = Vec::new();
    views
        .try_reserve_exact(selected.entries.len())
        .map_err(|_| FileSystemError::OutOfMemory)?;
    for candidate in selected.entries {
        if candidate.status == Status::Committed {
            views.push(candidate.view);
        }
    }
    let retiring = mount_retirement::PreparedDetach::new(views, unlock, synchronous);
    mount_publication::Publication::prepare(&[], &mut [], placements, Vec::new(), Some(retiring))?
        .publish();
    Ok(completions)
}
