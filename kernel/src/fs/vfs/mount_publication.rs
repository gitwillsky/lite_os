//! 实际森林的单次准备/发表；new、clone、bind、propagation stacking与move共用事务。
use super::{
    Attachment, FileSystemError, MountScope, MountView, mount_index, mount_propagation,
    mount_retirement,
};
use alloc::{
    boxed::Box,
    sync::{Arc, Weak},
    vec::Vec,
};

/// 已存在view的一次placement迁移；实际attachment payload和新index节点均在摘旧边前准备。
pub(super) struct Placement {
    view: Arc<MountView>,
    parent: Arc<MountView>,
    old_parent: Arc<MountView>,
    edge: Option<Box<mount_retirement::Retirement>>,
    node: Option<mount_index::PreparedAttachment>,
    owner: Option<Arc<MountView>>,
}
impl Placement {
    pub(super) fn prepare(
        view: Arc<MountView>,
        attachment: Attachment,
    ) -> Result<Self, FileSystemError> {
        let old_parent = view
            .attachment()
            .and_then(|edge| edge.parent.upgrade())
            .ok_or(FileSystemError::InvalidOperation)?;
        let parent = attachment
            .parent
            .upgrade()
            .ok_or(FileSystemError::InvalidOperation)?;
        if !Arc::ptr_eq(&parent.scope, &view.scope) {
            return Err(FileSystemError::InvalidOperation);
        }
        let key = mount_index::AttachmentKey::Child {
            parent: Arc::as_ptr(&parent) as usize,
            point: Arc::as_ptr(&attachment.point) as usize,
        };
        let node = mount_index::prepare_at(&view, key)?;
        let edge = mount_retirement::Retirement::for_attachment(attachment)?;
        Ok(Self {
            view,
            parent,
            old_parent,
            edge: Some(edge),
            node: Some(node),
            owner: None,
        })
    }
}
struct ParentCapacity {
    view: Arc<MountView>,
    additions: usize,
    removals: usize,
    grown: Vec<Arc<MountView>>,
}
/// 临时publication准备，借用实际incoming owner集合；不成为持久拓扑或关系副本。
pub(super) struct Publication<'a> {
    incoming: &'a [Arc<MountView>],
    nodes: &'a mut [Option<mount_index::PreparedAttachment>],
    placements: Vec<Placement>,
    parents: Vec<ParentCapacity>,
    shared: Vec<mount_propagation::PreparedShared>,
    retiring: Option<mount_retirement::PreparedDetach>,
    scopes: Vec<(Arc<MountScope>, usize)>,
}
impl<'a> Publication<'a> {
    pub(super) fn prepare(
        incoming: &'a [Arc<MountView>],
        nodes: &'a mut [Option<mount_index::PreparedAttachment>],
        placements: Vec<Placement>,
        shared: Vec<mount_propagation::PreparedShared>,
        retiring: Option<mount_retirement::PreparedDetach>,
    ) -> Result<Self, FileSystemError> {
        assert_eq!(incoming.len(), nodes.len());
        let mut publication = Self {
            incoming,
            nodes,
            placements,
            parents: Vec::new(),
            shared,
            retiring,
            scopes: Vec::new(),
        };
        let capacity = incoming
            .len()
            .checked_add(
                publication
                    .placements
                    .len()
                    .checked_mul(2)
                    .ok_or(FileSystemError::OutOfMemory)?,
            )
            .and_then(|capacity| {
                capacity.checked_add(
                    publication
                        .retiring
                        .as_ref()
                        .map_or(0, |set| set.views.len()),
                )
            })
            .ok_or(FileSystemError::OutOfMemory)?;
        publication
            .parents
            .try_reserve_exact(capacity)
            .map_err(|_| FileSystemError::OutOfMemory)?;
        let mut keys = Vec::new();
        keys.try_reserve_exact(
            incoming
                .len()
                .checked_add(publication.placements.len())
                .ok_or(FileSystemError::OutOfMemory)?,
        )
        .map_err(|_| FileSystemError::OutOfMemory)?;
        for (view, node) in incoming.iter().zip(publication.nodes.iter()) {
            let node = node.as_ref().expect("incoming index prepared");
            assert_eq!(*node.key(), view.attachment_key());
            assert!(core::ptr::eq(node.value().as_ptr(), Arc::as_ptr(view)));
            keys.push((*node.key(), Arc::as_ptr(view) as usize));
        }
        let mut moving = Vec::new();
        moving
            .try_reserve_exact(publication.placements.len())
            .map_err(|_| FileSystemError::OutOfMemory)?;
        for placement in &publication.placements {
            let node = placement.node.as_ref().expect("placement index prepared");
            keys.push((*node.key(), Arc::as_ptr(&placement.view) as usize));
            moving.push(Arc::as_ptr(&placement.view) as usize);
        }
        keys.sort_unstable_by_key(|(key, _)| *key);
        if keys.windows(2).any(|pair| pair[0].0 == pair[1].0) {
            return Err(FileSystemError::Busy);
        }
        moving.sort_unstable();
        for (key, _) in &keys {
            if let Some(owner) = mount_index::owner_at(*key)
                && moving.binary_search(&(owner.as_ptr() as usize)).is_err()
                && !publication.retiring.as_ref().is_some_and(|set| {
                    set.views.iter().any(|view| {
                        core::ptr::eq(owner.as_ptr(), Arc::as_ptr(view)) && set.disconnects(view)
                    })
                })
            {
                return Err(FileSystemError::Busy);
            }
        }
        keys.sort_unstable_by_key(|(_, owner)| *owner);
        if keys.windows(2).any(|pair| pair[0].1 == pair[1].1) {
            return Err(FileSystemError::InvalidOperation);
        }
        publication.validate_graph()?;
        if let Some(set) = &publication.retiring {
            for view in &set.views {
                if !view.state.lock().namespace_member
                    || keys
                        .binary_search_by_key(&(Arc::as_ptr(view) as usize), |(_, owner)| *owner)
                        .is_ok()
                {
                    return Err(FileSystemError::InvalidOperation);
                }
                if set.disconnects(view)
                    && let Some(parent) = view.attachment().and_then(|edge| edge.parent.upgrade())
                {
                    Self::parent_change(&mut publication.parents, parent, false)?;
                }
            }
        }
        for view in incoming {
            assert!(
                !view.state.lock().namespace_member,
                "new mount already admitted"
            );
            if let Some(edge) = view.attachment() {
                let parent = edge
                    .parent
                    .upgrade()
                    .ok_or(FileSystemError::InvalidOperation)?;
                if !Arc::ptr_eq(&parent.scope, &view.scope) {
                    return Err(FileSystemError::InvalidOperation);
                }
                Self::parent_change(&mut publication.parents, parent, true)?;
            }
        }
        for placement in &publication.placements {
            if !placement.view.state.lock().namespace_member {
                return Err(FileSystemError::InvalidOperation);
            }
            Self::parent_change(&mut publication.parents, placement.parent.clone(), true)?;
            Self::parent_change(
                &mut publication.parents,
                placement.old_parent.clone(),
                false,
            )?;
        }
        for parent in &mut publication.parents {
            let (length, capacity) = {
                let state = parent.view.state.lock();
                (state.children.len(), state.children.capacity())
            };
            let required = length
                .checked_sub(parent.removals)
                .expect("prepared removal owns actual child")
                .checked_add(parent.additions)
                .ok_or(FileSystemError::OutOfMemory)?;
            if required > capacity {
                parent
                    .grown
                    .try_reserve_exact(
                        required.max(length.checked_mul(2).ok_or(FileSystemError::OutOfMemory)?),
                    )
                    .map_err(|_| FileSystemError::OutOfMemory)?;
            }
        }
        mount_propagation::prepare_memberships(incoming, &publication.shared)?;
        publication
            .scopes
            .try_reserve_exact(capacity)
            .map_err(|_| FileSystemError::OutOfMemory)?;
        for view in incoming {
            publication.scopes.push((view.scope.clone(), 1));
        }
        for view in publication
            .placements
            .iter()
            .map(|placement| &placement.view)
            .chain(publication.retiring.iter().flat_map(|set| &set.views))
        {
            publication.scopes.push((view.scope.clone(), 0));
        }
        publication
            .scopes
            .sort_unstable_by_key(|(scope, _)| Arc::as_ptr(scope) as usize);
        publication
            .scopes
            .dedup_by(|(left, count), (right, total)| {
                if Arc::ptr_eq(left, right) {
                    *total = total
                        .checked_add(*count)
                        .expect("prepared incoming scope count");
                    true
                } else {
                    false
                }
            });
        for (scope, added) in &publication.scopes {
            scope.prepare_additions(*added)?;
        }
        Ok(publication)
    }
    fn validate_graph(&self) -> Result<(), FileSystemError> {
        // 临时edge索引只借真实prepared placement/incoming关系；不复制持久forest。
        // Gray/Black只memo本次祖先walk，deep/batch图不靠递归栈或每节点永久visited位。
        let mut edges: Vec<(usize, Option<Weak<MountView>>)> = Vec::new();
        edges
            .try_reserve_exact(
                self.incoming
                    .len()
                    .checked_add(self.placements.len())
                    .ok_or(FileSystemError::OutOfMemory)?,
            )
            .map_err(|_| FileSystemError::OutOfMemory)?;
        for view in self.incoming {
            edges.push((
                Arc::as_ptr(view) as usize,
                view.attachment().map(|edge| edge.parent),
            ));
        }
        for placement in &self.placements {
            edges.push((
                Arc::as_ptr(&placement.view) as usize,
                Some(Arc::downgrade(&placement.parent)),
            ));
        }
        edges.sort_unstable_by_key(|(owner, _)| *owner);
        enum Visit {
            Gray,
            Black,
        }
        let mut visited = crate::fallible_tree::FallibleMap::new();
        let mut path = Vec::new();
        for start in self
            .incoming
            .iter()
            .chain(self.placements.iter().map(|placement| &placement.view))
        {
            let mut current = Some(start.clone());
            while let Some(view) = current {
                if self
                    .retiring
                    .as_ref()
                    .is_some_and(|set| set.contains(&view))
                {
                    return Err(FileSystemError::InvalidOperation);
                }
                let identity = Arc::as_ptr(&view) as usize;
                match visited.get(&identity) {
                    Some(Visit::Black) => break,
                    Some(Visit::Gray) => return Err(FileSystemError::Loop),
                    None => {}
                }
                path.try_reserve(1)
                    .map_err(|_| FileSystemError::OutOfMemory)?;
                visited
                    .try_insert(identity, Visit::Gray)
                    .map_err(|_| FileSystemError::OutOfMemory)?;
                path.push(identity);
                let prepared = edges
                    .binary_search_by_key(&identity, |(owner, _)| *owner)
                    .ok();
                let parent = if let Some(index) = prepared {
                    edges[index].1.clone()
                } else {
                    view.attachment().map(|edge| edge.parent)
                };
                current = match parent {
                    Some(parent) => {
                        let parent = parent.upgrade().ok_or(FileSystemError::InvalidOperation)?;
                        if !parent.state.lock().namespace_member
                            && edges
                                .binary_search_by_key(
                                    &(Arc::as_ptr(&parent) as usize),
                                    |(owner, _)| *owner,
                                )
                                .is_err()
                        {
                            return Err(FileSystemError::InvalidOperation);
                        }
                        Some(parent)
                    }
                    None => None,
                };
            }
            for owner in path.drain(..) {
                *visited.get_mut(&owner).expect("graph walk memo") = Visit::Black;
            }
        }
        Ok(())
    }
    fn parent_change(
        parents: &mut Vec<ParentCapacity>,
        view: Arc<MountView>,
        addition: bool,
    ) -> Result<(), FileSystemError> {
        let index = if let Some(index) = parents
            .iter()
            .position(|parent| Arc::ptr_eq(&parent.view, &view))
        {
            index
        } else {
            assert!(parents.capacity() > parents.len());
            parents.push(ParentCapacity {
                view,
                additions: 0,
                removals: 0,
                grown: Vec::new(),
            });
            parents.len() - 1
        };
        let count = if addition {
            &mut parents[index].additions
        } else {
            &mut parents[index].removals
        };
        *count = count.checked_add(1).ok_or(FileSystemError::OutOfMemory)?;
        Ok(())
    }
    /// capacity/index/关系都已准备；无分配发表，旧attachment只提交预置退休，不执行filesystem cleanup。
    pub(super) fn publish(mut self) {
        for parent in &mut self.parents {
            if parent.grown.capacity() != 0 {
                let mut state = parent.view.state.lock();
                parent.grown.append(&mut state.children);
                core::mem::swap(&mut parent.grown, &mut state.children);
            }
        }
        // 先摘迁移的旧index，才能在原位置发表被它覆盖的新mount。
        for placement in &mut self.placements {
            mount_index::detach(&placement.view);
            placement.owner = Some({
                let mut state = placement.old_parent.state.lock();
                let index = state
                    .children
                    .iter()
                    .position(|child| Arc::ptr_eq(child, &placement.view))
                    .expect("placement owns actual child edge");
                state.children.remove(index)
            });
            let retired = placement
                .view
                .state
                .lock()
                .attachment
                .replace(placement.edge.take().expect("placement edge prepared"));
            if let Some(retired) = retired {
                mount_retirement::enqueue(retired);
            }
        }
        if let Some(set) = self.retiring.take() {
            set.publish();
        }
        // 必须先批量移走全部旧owning edge，再批量插入；net capacity不保证逐项搬移的峰值。
        for placement in &mut self.placements {
            placement
                .parent
                .state
                .lock()
                .children
                .push(placement.owner.take().expect("actual moving edge prepared"));
        }
        mount_propagation::publish_shared(core::mem::take(&mut self.shared));
        mount_propagation::publish_memberships(self.incoming);
        mount_index::publish(self.incoming, self.nodes);
        for view in self.incoming {
            if let Some(edge) = view.attachment() {
                let parent = edge
                    .parent
                    .upgrade()
                    .expect("publication retains actual owning parent");
                parent.state.lock().children.push(view.clone());
            }
        }
        for placement in &mut self.placements {
            mount_index::publish(
                core::slice::from_ref(&placement.view),
                core::slice::from_mut(&mut placement.node),
            );
            placement.view.state.lock().expiry_mark = false;
        }
        // 实际scope的去重pins只属于本cold事务；避免按每个view再二次扫描全部mounts。
        for (scope, _) in self.scopes {
            scope.changed();
        }
    }
}
