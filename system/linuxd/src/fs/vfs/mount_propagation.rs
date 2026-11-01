//! mount peer/slave 关系的实际 owner；与 attachment 森林正交，所有变更持 VFS mutation gate。
use super::{FileSystemError, MountView};
use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};
use core::sync::atomic::{AtomicU64, Ordering};
use spin::Mutex;

// OWNER: Linux mountinfo 的 peer group identity，不复用以避免旧 snapshot 的 identity ABA。
static NEXT_GROUP_ID: AtomicU64 = AtomicU64::new(1);

/// 实际 shared peer membership；弱成员不保活 namespace、view 或 filesystem。
pub(super) struct PeerGroup {
    pub(super) id: u64,
    members: Mutex<Vec<Weak<MountView>>>,
}
impl PeerGroup {
    fn prepare() -> Result<Arc<Self>, FileSystemError> {
        let mut members = Vec::new();
        members
            .try_reserve_exact(1)
            .map_err(|_| FileSystemError::OutOfMemory)?;
        Arc::try_new(Self {
            id: NEXT_GROUP_ID
                .try_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
                .map_err(|_| FileSystemError::OutOfMemory)?,
            members: Mutex::new(members),
        })
        .map_err(|_| FileSystemError::OutOfMemory)
    }
    fn reserve(&self, additional: usize) -> Result<(), FileSystemError> {
        let (length, capacity) = {
            let members = self.members.lock();
            (members.len(), members.capacity())
        };
        let required = length
            .checked_add(additional)
            .ok_or(FileSystemError::OutOfMemory)?;
        if required <= capacity {
            return Ok(());
        }
        let mut grown = Vec::new();
        grown
            .try_reserve_exact(required)
            .map_err(|_| FileSystemError::OutOfMemory)?;
        {
            let mut members = self.members.lock();
            grown.append(&mut members);
            core::mem::swap(&mut grown, &mut members);
        }
        drop(grown);
        Ok(())
    }
    /// gate 下逐个 pin 实际成员，upgrade 与最终 Arc Drop 不在 membership 短锁中。
    pub(super) fn peers(&self) -> Result<Vec<Arc<MountView>>, FileSystemError> {
        let length = self.members.lock().len();
        let mut peers = Vec::new();
        peers
            .try_reserve_exact(length)
            .map_err(|_| FileSystemError::OutOfMemory)?;
        for index in 0..length {
            let member = self.members.lock()[index].clone();
            peers.push(
                member
                    .upgrade()
                    .expect("live propagation member owns its forest edge"),
            );
        }
        Ok(peers)
    }
}

/// 每个实际 mount 的传播关系。master/slave 链全为 Weak，没有强 master 链或资源环。
/// 未发表 view 只拥有 group/master 关系意图，links 空；capacity 准备后才注册实际 membership。
#[derive(Default)]
pub(super) struct Propagation {
    group: Option<Arc<PeerGroup>>,
    master: Option<Weak<MountView>>,
    previous: Option<Weak<MountView>>,
    next: Option<Weak<MountView>>,
    first_slave: Option<Weak<MountView>>,
    unbindable: bool,
    // TRANSACTION: 未发表clone插在真实source的peer/slave之后；commit消费，非持久关系副本。
    copy_origin: Option<Weak<MountView>>,
}
/// Linux MS_* 传播属性；递归范围由实际 attachment subtree 决定。
#[derive(Clone, Copy)]
pub(crate) enum MountPropagation {
    Shared,
    Private,
    Slave,
    Unbindable,
}

/// 单次事务的未来Shared关系。实际view尚未改变；secondary clone只借这份已预备的group身份。
pub(super) struct PreparedShared {
    view: Arc<MountView>,
    group: Arc<PeerGroup>,
}
pub(super) fn prepare_shared(
    views: &[Arc<MountView>],
) -> Result<Vec<PreparedShared>, FileSystemError> {
    let mut prepared = Vec::new();
    prepared
        .try_reserve_exact(views.len())
        .map_err(|_| FileSystemError::OutOfMemory)?;
    for view in views {
        if group(view).is_none() {
            prepared.push(PreparedShared {
                view: view.clone(),
                group: PeerGroup::prepare()?,
            });
        }
    }
    prepared.sort_unstable_by_key(|prepared| Arc::as_ptr(&prepared.view) as usize);
    Ok(prepared)
}
pub(super) fn prepared_group(
    view: &Arc<MountView>,
    prepared: &[PreparedShared],
) -> Option<Arc<PeerGroup>> {
    group(view).or_else(|| {
        prepared
            .binary_search_by_key(&(Arc::as_ptr(view) as usize), |prepared| {
                Arc::as_ptr(&prepared.view) as usize
            })
            .ok()
            .map(|index| prepared[index].group.clone())
    })
}
pub(super) fn publish_shared(prepared: Vec<PreparedShared>) {
    for PreparedShared { view, group } in prepared {
        let live = view.state.lock().namespace_member;
        if live {
            let mut members = group.members.lock();
            assert!(members.len() < members.capacity());
            members.push(Arc::downgrade(&view));
        }
        let retired = {
            let mut state = view.state.lock();
            state.propagation.unbindable = false;
            state.propagation.group.replace(group)
        };
        assert!(retired.is_none());
    }
}

pub(super) fn group(view: &MountView) -> Option<Arc<PeerGroup>> {
    view.state.lock().propagation.group.clone()
}
pub(super) fn master(view: &MountView) -> Option<Arc<MountView>> {
    let master = view.state.lock().propagation.master.clone();
    master.map(|owner| {
        owner
            .upgrade()
            .expect("propagation master retains its forest edge")
    })
}
pub(super) fn unbindable(view: &MountView) -> bool {
    view.state.lock().propagation.unbindable
}

/// 固定 Linux get_dominating_id：从实际master向上寻找，在被观察namespace且FSroot内
/// 可达的真实peer。不同bind-root的peer不能用一个任意group代表替代。
pub(super) fn dominating_id(
    view: &MountView,
    root: &Arc<super::OpenedFile>,
) -> Result<u64, FileSystemError> {
    let mut current = master(view);
    while let Some(candidate) = current {
        let peers = if let Some(group) = group(&candidate) {
            group.peers()?
        } else {
            let mut peers = Vec::new();
            peers
                .try_reserve_exact(1)
                .map_err(|_| FileSystemError::OutOfMemory)?;
            peers.push(candidate.clone());
            peers
        };
        for peer in peers {
            if peer.scope.id == view.scope.id {
                let path = super::OpenedFile::at(peer.clone(), peer.root.clone())?;
                if path.project_path(root, false)?.1 {
                    return Ok(group(&peer).map_or(0, |group| group.id));
                }
            }
        }
        current = master(&candidate);
    }
    Ok(0)
}

/// clone_mnt的唯一关系准备。source_group可借尚未发表的PreparedShared身份；
/// slave对应CL_SLAVE（lower-user copy/传播slave端），make_shared对应CL_MAKE_SHARED。
/// 非shared slave只继承真实master，不能把独立singleton错误并成peers；OOM只影响未发表target。
pub(super) fn prepare_copy(
    source: &Arc<MountView>,
    target: &Arc<MountView>,
    source_group: Option<Arc<PeerGroup>>,
    slave: bool,
    make_shared: bool,
) -> Result<(), FileSystemError> {
    let source_master = master(source);
    let unbindable = unbindable(source);
    let make_slave = slave && source_group.is_some();
    let target_group = if make_shared && (make_slave || source_group.is_none()) {
        Some(PeerGroup::prepare()?)
    } else if make_slave {
        None
    } else {
        source_group
    };
    let mut state = target.state.lock();
    assert!(!state.namespace_member);
    assert!(state.propagation.group.is_none() && state.propagation.master.is_none());
    if make_slave {
        state.propagation.master = Some(Arc::downgrade(source));
    } else {
        if target_group.is_some() || source_master.is_some() {
            state.propagation.copy_origin = Some(Arc::downgrade(source));
        }
        state.propagation.master = source_master.as_ref().map(Arc::downgrade);
    }
    state.propagation.group = target_group;
    state.propagation.unbindable = unbindable && !make_shared;
    drop(state);
    drop(source_master);
    Ok(())
}

/// 全部 incoming peers 的 backing 在发表前预留。容量本身无传播语义，OOM不修改关系。
pub(super) fn prepare_memberships(
    views: &[Arc<MountView>],
    shared: &[PreparedShared],
) -> Result<(), FileSystemError> {
    let mut groups: Vec<(Arc<PeerGroup>, usize)> = Vec::new();
    groups
        .try_reserve_exact(
            views
                .len()
                .checked_add(shared.len())
                .ok_or(FileSystemError::OutOfMemory)?,
        )
        .map_err(|_| FileSystemError::OutOfMemory)?;
    for view in views {
        if let Some(group) = group(view) {
            if let Some((_, count)) = groups
                .iter_mut()
                .find(|(existing, _)| Arc::ptr_eq(existing, &group))
            {
                *count = count.checked_add(1).ok_or(FileSystemError::OutOfMemory)?;
            } else {
                groups.push((group, 1));
            }
        }
    }
    for prepared in shared {
        if let Some((_, count)) = groups
            .iter_mut()
            .find(|(existing, _)| Arc::ptr_eq(existing, &prepared.group))
        {
            *count = count.checked_add(1).ok_or(FileSystemError::OutOfMemory)?;
        } else {
            groups.push((prepared.group.clone(), 1));
        }
    }
    for (group, count) in groups {
        group.reserve(count)?;
    }
    Ok(())
}

/// 只在全部 forest/index/group preflight 成功后调用；发表没有分配或可恢复失败。
pub(super) fn publish_memberships(views: &[Arc<MountView>]) {
    for view in views {
        let group = group(view);
        let master = master(view);
        let origin = view.state.lock().propagation.copy_origin.take();
        let origin = origin.map(|origin| {
            origin
                .upgrade()
                .expect("prepared clone retains actual source")
        });
        if let Some(group) = group {
            let anchor = origin.as_ref().filter(|origin| {
                self::group(origin)
                    .as_ref()
                    .is_some_and(|source_group| Arc::ptr_eq(source_group, &group))
            });
            let mut members = group.members.lock();
            assert!(members.capacity() > members.len());
            assert!(
                !members
                    .iter()
                    .any(|member| core::ptr::eq(member.as_ptr(), Arc::as_ptr(view)))
            );
            if let Some(anchor) = anchor {
                let index = members
                    .iter()
                    .position(|member| core::ptr::eq(member.as_ptr(), Arc::as_ptr(anchor)))
                    .expect("prepared peer source already registered");
                members.insert(index + 1, Arc::downgrade(view));
            } else {
                members.push(Arc::downgrade(view));
            }
        }
        // staged master还未拥有 slave edge；先取走意图，再以同一个实际 owner 注册。
        let intent = view.state.lock().propagation.master.take();
        drop(intent);
        if let Some(master) = master {
            if let Some(origin) = &origin {
                link_slave_after(view, &master, origin);
            } else {
                link_slave(view, &master);
            }
        }
    }
}

fn unlink_slave(view: &Arc<MountView>) {
    let (master, previous, next) = {
        let mut state = view.state.lock();
        let relation = &mut state.propagation;
        (
            relation.master.take(),
            relation.previous.take(),
            relation.next.take(),
        )
    };
    let Some(master) = master else {
        assert!(previous.is_none() && next.is_none());
        return;
    };
    let master = master
        .upgrade()
        .expect("registered slave retains actual master");
    if let Some(previous) = &previous {
        let previous = previous
            .upgrade()
            .expect("previous slave retains actual member");
        let retired = core::mem::replace(&mut previous.state.lock().propagation.next, next.clone());
        drop(retired);
    } else {
        let retired = core::mem::replace(
            &mut master.state.lock().propagation.first_slave,
            next.clone(),
        );
        drop(retired);
    }
    if let Some(next) = &next {
        let next = next.upgrade().expect("next slave retains actual member");
        let retired = core::mem::replace(
            &mut next.state.lock().propagation.previous,
            previous.clone(),
        );
        drop(retired);
    }
}
fn link_slave(view: &Arc<MountView>, master: &Arc<MountView>) {
    assert!(
        !Arc::ptr_eq(view, master),
        "mount cannot propagate to itself"
    );
    let first = master.state.lock().propagation.first_slave.clone();
    if let Some(first) = &first {
        let first = first.upgrade().expect("slave head owns actual forest edge");
        let retired = first
            .state
            .lock()
            .propagation
            .previous
            .replace(Arc::downgrade(view));
        assert!(retired.is_none());
    }
    {
        let mut state = view.state.lock();
        assert!(state.propagation.master.is_none());
        assert!(state.propagation.previous.is_none() && state.propagation.next.is_none());
        state.propagation.master = Some(Arc::downgrade(master));
        state.propagation.next = first;
    }
    let retired = master
        .state
        .lock()
        .propagation
        .first_slave
        .replace(Arc::downgrade(view));
    drop(retired);
}
fn link_slave_after(view: &Arc<MountView>, master: &Arc<MountView>, origin: &Arc<MountView>) {
    assert!(!Arc::ptr_eq(view, master) && !Arc::ptr_eq(view, origin));
    assert!(
        self::master(origin)
            .as_ref()
            .is_some_and(|source_master| Arc::ptr_eq(source_master, master))
    );
    let next = origin.state.lock().propagation.next.clone();
    if let Some(next) = &next {
        let next = next
            .upgrade()
            .expect("clone slave successor owns actual forest edge");
        let retired = next
            .state
            .lock()
            .propagation
            .previous
            .replace(Arc::downgrade(view));
        drop(retired);
    }
    {
        let mut state = view.state.lock();
        assert!(state.propagation.master.is_none());
        assert!(state.propagation.previous.is_none() && state.propagation.next.is_none());
        state.propagation.master = Some(Arc::downgrade(master));
        state.propagation.previous = Some(Arc::downgrade(origin));
        state.propagation.next = next;
    }
    let retired = origin
        .state
        .lock()
        .propagation
        .next
        .replace(Arc::downgrade(view));
    drop(retired);
}
fn transfer_slaves(view: &Arc<MountView>, destination: Option<&Arc<MountView>>) {
    loop {
        let first = view.state.lock().propagation.first_slave.clone();
        let Some(first) = first else {
            return;
        };
        let slave = first.upgrade().expect("slave transfer owns actual member");
        unlink_slave(&slave);
        if let Some(destination) = destination {
            link_slave(&slave, destination);
        }
    }
}
fn leave_group(view: &Arc<MountView>) -> (bool, Option<Arc<MountView>>) {
    let group = view.state.lock().propagation.group.take();
    let Some(group) = group else {
        return (false, None);
    };
    let (retired, peer) = {
        let mut members = group.members.lock();
        let index = members
            .iter()
            .position(|member| core::ptr::eq(member.as_ptr(), Arc::as_ptr(view)))
            .expect("registered mount peer");
        let retired = members.remove(index);
        // 固定 Linux change_mnt_propagation选择下一个实际peer，而非任意namespace代表。
        let peer = if members.is_empty() {
            None
        } else {
            Some(members[index % members.len()].clone())
        };
        (retired, peer)
    };
    drop(retired);
    (
        true,
        peer.map(|peer| {
            peer.upgrade()
                .expect("remaining peer retains actual forest edge")
        }),
    )
}

/// 改变实际 views 的传播属性；所有Shared group先准备完，递归请求不会半途改变子树。
pub(super) fn change(
    views: &[Arc<MountView>],
    kind: MountPropagation,
) -> Result<(), FileSystemError> {
    if matches!(kind, MountPropagation::Shared) {
        let prepared = prepare_shared(views)?;
        prepare_memberships(&[], &prepared)?;
        publish_shared(prepared);
        return Ok(());
    }
    for view in views {
        let old_master = master(view);
        let (shared, peer) = leave_group(view);
        let destination = peer.as_ref().or(old_master.as_ref());
        if shared {
            transfer_slaves(view, destination);
        }
        unlink_slave(view);
        if matches!(kind, MountPropagation::Slave) {
            if let Some(destination) = destination {
                link_slave(view, destination);
            }
            // Linux MS_SLAVE不清旧T_UNBINDABLE；没有master时依旧是singleton，无peer。
        } else {
            view.state.lock().propagation.unbindable = matches!(kind, MountPropagation::Unbindable);
        }
    }
    Ok(())
}

/// 实际 forest edge 释放之前注销；所有slave转接到仍live的peer/master，全程无分配、无强链Drop。
pub(super) fn retire(view: &Arc<MountView>) {
    let old_master = master(view);
    let (_, peer) = leave_group(view);
    transfer_slaves(view, peer.as_ref().or(old_master.as_ref()));
    unlink_slave(view);
    view.state.lock().propagation.unbindable = false;
}

/// 传播接收者按实际 peer/master/slave 链枚举，保留每View root与namespace identity。
pub(super) fn receivers(origin: &Arc<MountView>) -> Result<Vec<Arc<MountView>>, FileSystemError> {
    let mut output = if let Some(origin_group) = group(origin) {
        origin_group.peers()?
    } else {
        let mut views = Vec::new();
        views
            .try_reserve_exact(1)
            .map_err(|_| FileSystemError::OutOfMemory)?;
        views.push(origin.clone());
        views
    };
    let mut index = 0;
    while index < output.len() {
        let mut slave = output[index].state.lock().propagation.first_slave.clone();
        while let Some(member) = slave {
            let member = member
                .upgrade()
                .expect("propagation receiver retains actual forest edge");
            slave = member.state.lock().propagation.next.clone();
            if output.iter().any(|old| Arc::ptr_eq(old, &member)) {
                continue;
            }
            let Some(group) = group(&member) else {
                output
                    .try_reserve(1)
                    .map_err(|_| FileSystemError::OutOfMemory)?;
                output.push(member);
                continue;
            };
            let peers = group.peers()?;
            output
                .try_reserve(peers.len())
                .map_err(|_| FileSystemError::OutOfMemory)?;
            for peer in peers {
                if !output.iter().any(|old| Arc::ptr_eq(old, &peer)) {
                    output.push(peer);
                }
            }
        }
        index += 1;
    }
    Ok(output)
}
