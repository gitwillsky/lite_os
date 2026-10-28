//! 实际View/PeerGroup/slave owner的clone、属性变更与退休；不复制生产传播算法。
use super::*;
struct MountLimitRestore(u32);
impl Drop for MountLimitRestore {
    fn drop(&mut self) {
        mount_limit::set_mount_limit(self.0).unwrap();
    }
}
fn authority(user: Arc<crate::namespace::UserNamespace>) -> crate::namespace::NamespaceAuthority {
    crate::namespace::NamespaceAuthority {
        namespace: user,
        effective_uid: 0,
        effective_gid: 0,
        effective_capabilities: u64::MAX,
    }
}

// internal mount装配是nsfs adapter边界；所有bind/copy/loop/传播/retirement加载实际VFS owner。
fn namespace_file_path(reference: crate::namespace::NamespaceReference) -> Arc<OpenedFile> {
    let filesystem = super::super::nsfs::mounted_filesystem().unwrap();
    let root = Dentry::named(filesystem.filesystem.root_inode().unwrap(), None, b"").unwrap();
    let entry =
        Dentry::anonymous(Node::namespace_file(reference), b"fixture-nsfs".to_vec()).unwrap();
    let view = MountView::new(
        MountScope::new(0, crate::namespace::initial_user()).unwrap(),
        Arc::new(b"nsfs".to_vec()),
        filesystem,
        root,
        None,
        MountFlags::from_bits(0),
        MountRestrictions::default(),
    )
    .unwrap();
    OpenedFile::at(view, entry).unwrap()
}

#[test]
fn mount_namespace_file_bind_rejects_cycles_and_namespace_clone_prunes_the_bound_view() {
    let _case = CASE.lock();
    let f = Fixture::new();
    let namespace = VFS.namespace.lock().as_ref().unwrap().clone();
    let point = VFS.open_file(b"/jail/file").unwrap();
    let self_file =
        namespace_file_path(crate::namespace::NamespaceReference::new(namespace.clone()));
    let event = namespace.event();
    assert!(matches!(
        VFS.bind_mount(&namespace, &self_file, &point, false),
        Err(FileSystemError::InvalidOperation)
    ));
    assert_eq!(namespace.event(), event);
    let (later, _) = namespace
        .copy_for(crate::namespace::initial_user(), 0, f.origin())
        .unwrap();
    let later_file = namespace_file_path(crate::namespace::NamespaceReference::new(later.clone()));
    VFS.bind_mount(&namespace, &later_file, &point, false)
        .unwrap();
    let bound = VFS.open_file(b"/jail/file").unwrap();
    assert!(Arc::ptr_eq(&bound.dentry, &later_file.dentry));
    assert!(Arc::ptr_eq(
        &bound.mount.as_ref().unwrap().filesystem,
        &later_file.mount.as_ref().unwrap().filesystem
    ));
    assert_eq!(bound.mount_flags().bits(), 0);
    assert_eq!(namespace.snapshot().unwrap().len(), 2);
    let (copy, _) = namespace
        .copy_for(crate::namespace::initial_user(), 0, f.origin())
        .unwrap();
    assert_eq!(copy.snapshot().unwrap().len(), 1);
    assert!(mount_index::child(&copy.root_view(), &point.dentry).is_none());
    let rebound_point = OpenedFile::at(later.root_view(), point.dentry.clone()).unwrap();
    assert!(matches!(
        VFS.bind_mount(&later, &bound, &rebound_point, false),
        Err(FileSystemError::InvalidOperation)
    ));
}

#[test]
fn user_namespace_file_survives_namespace_clone_but_mount_namespace_file_cannot_propagate() {
    let _case = CASE.lock();
    let f = Fixture::new();
    let namespace = VFS.namespace.lock().as_ref().unwrap().clone();
    let point = VFS.open_file(b"/jail/file").unwrap();
    let user_file = namespace_file_path(crate::namespace::NamespaceReference::new(
        crate::namespace::initial_user(),
    ));
    VFS.bind_mount(&namespace, &user_file, &point, false)
        .unwrap();
    let (copy, _) = namespace
        .copy_for(crate::namespace::initial_user(), 0, f.origin())
        .unwrap();
    assert_eq!(copy.snapshot().unwrap().len(), 2);
    let caller = authority(crate::namespace::initial_user());
    VFS.change_mount_propagation(&caller, &f.global, MountPropagation::Shared, false)
        .unwrap();
    let (peer, _) = namespace
        .copy_for(caller.namespace.clone(), 0, f.origin())
        .unwrap();
    let (later, _) = namespace.copy_for(caller.namespace, 0, f.origin()).unwrap();
    let mnt_file = namespace_file_path(crate::namespace::NamespaceReference::new(later));
    let event = namespace.event();
    let peer_event = peer.event();
    let before = namespace.snapshot().unwrap().len();
    // /jail/file 已被 private user namespace bind 覆盖；选实际 shared parent 的未覆盖 mountpoint。
    let shared_point = VFS.open_file(b"/outside").unwrap();
    assert!(matches!(
        VFS.bind_mount(&namespace, &mnt_file, &shared_point, false),
        Err(FileSystemError::InvalidOperation)
    ));
    assert_eq!(namespace.event(), event);
    assert_eq!(peer.event(), peer_event);
    assert_eq!(namespace.snapshot().unwrap().len(), before);
}

#[test]
fn lowered_mount_limit_keeps_existing_moves_and_failed_bind_has_no_partial_effect() {
    let _case = CASE.lock();
    let _restore = MountLimitRestore(mount_limit::mount_limit());
    let f = Fixture::new();
    let namespace = VFS.namespace.lock().as_ref().unwrap().clone();
    VFS.bind_mount(&namespace, &f.jail, &f.inner, false)
        .unwrap();
    let source = VFS.open_file(b"/jail/inner").unwrap();
    mount_limit::set_mount_limit(1).unwrap();
    VFS.move_mount(&namespace, &source, &f.jail).unwrap();
    let event = namespace.event();
    assert!(matches!(
        VFS.bind_mount(&namespace, &f.jail, &f.inner, false),
        Err(FileSystemError::NoSpace)
    ));
    assert_eq!(namespace.event(), event);
    assert_eq!(namespace.snapshot().unwrap().len(), 2);
    {
        let _guard = VFS.namespace().unwrap();
        namespace.remove(&source, UnmountMode::Detach).unwrap();
    }
    assert_eq!(namespace.snapshot().unwrap().len(), 1);
    mount_limit::set_mount_limit(2).unwrap();
    VFS.bind_mount(&namespace, &f.jail, &f.inner, false)
        .unwrap();
}

#[test]
fn normal_propagated_unmount_returns_completion_for_each_quiescent_actual_view() {
    let _case = CASE.lock();
    let f = Fixture::new();
    let namespace = VFS.namespace.lock().as_ref().unwrap().clone();
    let caller = authority(crate::namespace::initial_user());
    VFS.change_mount_propagation(&caller, &f.global, MountPropagation::Shared, false)
        .unwrap();
    let (peer, _) = namespace.copy_for(caller.namespace, 0, f.origin()).unwrap();
    VFS.bind_mount(&namespace, &f.jail, &f.inner, false)
        .unwrap();
    let source = VFS.open_file(b"/jail/inner").unwrap();
    let peer_event = mount_index::child(&peer.root_view(), &f.inner.dentry)
        .unwrap()
        .complete
        .clone();
    let source_event = source.mount.as_ref().unwrap().complete.clone();
    let completions = {
        let _guard = VFS.namespace().unwrap();
        namespace.remove(&source, UnmountMode::Normal).unwrap()
    };
    assert_eq!(completions.len(), 2);
    assert!(
        completions
            .iter()
            .any(|event| Arc::ptr_eq(event, &peer_event))
    );
    assert!(
        completions
            .iter()
            .any(|event| Arc::ptr_eq(event, &source_event))
    );
    assert!(mount_index::child(&peer.root_view(), &f.inner.dentry).is_none());
}

#[test]
fn private_bind_into_shared_parent_prepares_peer_and_lower_user_slave_copies() {
    let _case = CASE.lock();
    let f = Fixture::new();
    let namespace = VFS.namespace.lock().as_ref().unwrap().clone();
    VFS.bind_mount(&namespace, &f.jail, &f.inner, false)
        .unwrap();
    let source = VFS.open_file(b"/jail/inner").unwrap();
    let caller = authority(crate::namespace::initial_user());
    VFS.change_mount_propagation(&caller, &f.global, MountPropagation::Shared, false)
        .unwrap();
    let (peer, _) = namespace
        .copy_for(caller.namespace.clone(), 0, f.origin())
        .unwrap();
    let child = crate::namespace::UserNamespace::child(
        &caller,
        true,
        crate::namespace::NamespaceRlimits {
            locked_memory: u64::MAX,
            pending_signals: u64::MAX,
        },
    )
    .unwrap();
    let (slave, _) = namespace.copy_for(child, 0, f.origin()).unwrap();
    let before_peer = peer.event();
    let before_slave = slave.event();
    VFS.bind_mount(&namespace, &source, &f.jail, false).unwrap();
    let primary = mount_index::child(&namespace.root_view(), &f.jail.dentry).unwrap();
    let second = mount_index::child(&peer.root_view(), &f.jail.dentry).unwrap();
    let lower = mount_index::child(&slave.root_view(), &f.jail.dentry).unwrap();
    assert!(mount_propagation::group(source.mount.as_ref().unwrap()).is_none());
    let group = mount_propagation::group(&primary).unwrap();
    assert!(Arc::ptr_eq(
        &group,
        &mount_propagation::group(&second).unwrap()
    ));
    assert!(mount_propagation::group(&lower).is_none());
    assert!(Arc::ptr_eq(
        &mount_propagation::master(&lower).unwrap(),
        &primary
    ));
    assert!(lower.state.lock().restrictions.atime_locked);
    assert!(!lower.state.lock().restrictions.inseparable);
    assert!(peer.event() > before_peer && slave.event() > before_slave);
    assert!(Arc::ptr_eq(&primary.filesystem, &second.filesystem));
    assert!(Arc::ptr_eq(&primary.root, &lower.root));
}

#[test]
fn unmount_keeps_receiver_with_independent_submount_and_detaches_unblocked_peer() {
    let _case = CASE.lock();
    let f = Fixture::new();
    let namespace = VFS.namespace.lock().as_ref().unwrap().clone();
    let caller = authority(crate::namespace::initial_user());
    VFS.change_mount_propagation(&caller, &f.global, MountPropagation::Shared, false)
        .unwrap();
    let (blocked, _) = namespace
        .copy_for(caller.namespace.clone(), 0, f.origin())
        .unwrap();
    let (free, _) = namespace.copy_for(caller.namespace, 0, f.origin()).unwrap();
    VFS.bind_mount(&namespace, &f.jail, &f.inner, false)
        .unwrap();
    let primary = VFS.open_file(b"/jail/inner").unwrap();
    let blocked_view = mount_index::child(&blocked.root_view(), &f.inner.dentry).unwrap();
    let blocked_root = OpenedFile::at(blocked_view.clone(), blocked_view.root.clone()).unwrap();
    // 接收端主动切断传播后创建独立child，不能由上游umount移走。
    VFS.change_mount_propagation(
        &authority(crate::namespace::initial_user()),
        &blocked_root,
        MountPropagation::Private,
        false,
    )
    .unwrap();
    let target = VFS
        .open_file_at(
            LookupOrigin::new(blocked.root().unwrap(), blocked_root.clone()),
            b"inner",
            &AccessIdentity,
        )
        .unwrap();
    VFS.bind_mount(&blocked, &blocked_root, &target, false)
        .unwrap();
    {
        let _guard = VFS.namespace().unwrap();
        namespace.remove(&primary, UnmountMode::Detach).unwrap();
    }
    assert!(mount_index::child(&namespace.root_view(), &f.inner.dentry).is_none());
    assert!(mount_index::child(&free.root_view(), &f.inner.dentry).is_none());
    assert!(Arc::ptr_eq(
        &mount_index::child(&blocked.root_view(), &f.inner.dentry).unwrap(),
        &blocked_view
    ));
    assert!(blocked_view.state.lock().namespace_member);
    assert!(
        !primary
            .mount
            .as_ref()
            .unwrap()
            .state
            .lock()
            .namespace_member
    );
    assert_eq!(
        primary.inode_identity().unwrap(),
        f.jail.inode_identity().unwrap()
    );
}

#[test]
fn unmount_reparents_surviving_root_overmount_to_receivers_original_mountpoint() {
    let _case = CASE.lock();
    let f = Fixture::new();
    let namespace = VFS.namespace.lock().as_ref().unwrap().clone();
    let caller = authority(crate::namespace::initial_user());
    VFS.change_mount_propagation(&caller, &f.global, MountPropagation::Shared, false)
        .unwrap();
    let (peer, _) = namespace.copy_for(caller.namespace, 0, f.origin()).unwrap();
    VFS.bind_mount(&namespace, &f.jail, &f.inner, false)
        .unwrap();
    let primary = VFS.open_file(b"/jail/inner").unwrap();
    let covered = mount_index::child(&peer.root_view(), &f.inner.dentry).unwrap();
    let peer_path = OpenedFile::at(covered.clone(), covered.root.clone()).unwrap();
    VFS.change_mount_propagation(
        &authority(crate::namespace::initial_user()),
        &peer_path,
        MountPropagation::Private,
        false,
    )
    .unwrap();
    VFS.bind_mount(&peer, &peer_path, &peer_path, false)
        .unwrap();
    let top = mount_index::child(&covered, &covered.root).unwrap();
    {
        let _guard = VFS.namespace().unwrap();
        namespace.remove(&primary, UnmountMode::Detach).unwrap();
    }
    assert!(!covered.state.lock().namespace_member);
    assert!(top.state.lock().namespace_member);
    assert!(Arc::ptr_eq(
        &mount_index::child(&peer.root_view(), &f.inner.dentry).unwrap(),
        &top
    ));
    let edge = top.attachment().unwrap();
    assert!(Arc::ptr_eq(
        &edge.parent.upgrade().unwrap(),
        &peer.root_view()
    ));
    assert!(Arc::ptr_eq(&edge.point, &f.inner.dentry));
}
#[test]
fn retained_foreign_directory_remains_readable_but_cannot_supply_ordinary_bind() {
    let _case = CASE.lock();
    let f = Fixture::new();
    let namespace = VFS.namespace.lock().as_ref().unwrap().clone();
    let (copy, origin) = namespace
        .copy_for(crate::namespace::initial_user(), 0, f.origin())
        .unwrap();
    let (root, target) = origin.into_parts();
    assert!(matches!(
        VFS.bind_mount(&copy, &f.jail, &target, false),
        Err(FileSystemError::InvalidOperation)
    ));
    assert!(
        VFS.open_file_at(
            LookupOrigin::new(root, f.jail.clone()),
            b"file",
            &AccessIdentity
        )
        .is_ok()
    );
}

#[test]
fn shared_clone_joins_after_actual_source_and_slave_chooses_actual_next_peer() {
    let _case = CASE.lock();
    let f = Fixture::new();
    let namespace = VFS.namespace.lock().as_ref().unwrap().clone();
    let user = crate::namespace::initial_user();
    VFS.change_mount_propagation(
        &authority(user.clone()),
        &f.global,
        MountPropagation::Shared,
        false,
    )
    .unwrap();
    let origin = LookupOrigin::new(f.global.clone(), f.global.clone());
    let (first, _) = namespace.copy_for(user.clone(), 0, origin.clone()).unwrap();
    let (second, _) = namespace.copy_for(user.clone(), 0, origin).unwrap();
    let group = mount_propagation::group(&namespace.root_view()).unwrap();
    let peers = group.peers().unwrap();
    assert_eq!(peers.len(), 3);
    assert!(Arc::ptr_eq(&peers[0], &namespace.root_view()));
    assert!(Arc::ptr_eq(&peers[1], &second.root_view()));
    assert!(Arc::ptr_eq(&peers[2], &first.root_view()));
    VFS.change_mount_propagation(&authority(user), &f.global, MountPropagation::Slave, false)
        .unwrap();
    assert!(mount_propagation::group(&namespace.root_view()).is_none());
    assert!(Arc::ptr_eq(
        &mount_propagation::master(&namespace.root_view()).unwrap(),
        &second.root_view()
    ));
    assert_eq!(group.peers().unwrap().len(), 2);
}

#[test]
fn lower_user_shared_copy_becomes_slave_and_slave_clones_remain_independent() {
    let _case = CASE.lock();
    let f = Fixture::new();
    let namespace = VFS.namespace.lock().as_ref().unwrap().clone();
    let initial = crate::namespace::initial_user();
    VFS.change_mount_propagation(
        &authority(initial.clone()),
        &f.global,
        MountPropagation::Shared,
        false,
    )
    .unwrap();
    let child = crate::namespace::UserNamespace::child(
        &authority(initial),
        true,
        crate::namespace::NamespaceRlimits {
            locked_memory: u64::MAX,
            pending_signals: u64::MAX,
        },
    )
    .unwrap();
    let (first, first_origin) = namespace
        .copy_for(
            child.clone(),
            0,
            LookupOrigin::new(f.global.clone(), f.global.clone()),
        )
        .unwrap();
    let (second, _) = first.copy_for(child.clone(), 0, first_origin).unwrap();
    assert!(mount_propagation::group(&first.root_view()).is_none());
    assert!(mount_propagation::group(&second.root_view()).is_none());
    for view in [first.root_view(), second.root_view()] {
        assert!(Arc::ptr_eq(
            &mount_propagation::master(&view).unwrap(),
            &namespace.root_view()
        ));
    }
    let first_root = first.root().unwrap();
    VFS.change_mount_propagation(
        &authority(child),
        &first_root,
        MountPropagation::Shared,
        false,
    )
    .unwrap();
    assert!(mount_propagation::group(&first.root_view()).is_some());
    assert!(mount_propagation::group(&second.root_view()).is_none());
    let receivers = mount_propagation::receivers(&namespace.root_view()).unwrap();
    assert_eq!(receivers.len(), 3);
    assert!(
        receivers
            .iter()
            .any(|view| Arc::ptr_eq(view, &first.root_view()))
    );
    assert!(
        receivers
            .iter()
            .any(|view| Arc::ptr_eq(view, &second.root_view()))
    );
}

#[test]
fn unbindable_root_rejects_bind_and_slave_preserves_unbindable_until_shared() {
    let _case = CASE.lock();
    let f = Fixture::new();
    let namespace = VFS.namespace.lock().as_ref().unwrap().clone();
    let caller = authority(crate::namespace::initial_user());
    VFS.change_mount_propagation(&caller, &f.global, MountPropagation::Unbindable, false)
        .unwrap();
    assert!(matches!(
        VFS.bind_mount(&namespace, &f.jail, &f.inner, false),
        Err(FileSystemError::InvalidOperation)
    ));
    VFS.change_mount_propagation(&caller, &f.global, MountPropagation::Slave, false)
        .unwrap();
    assert!(mount_propagation::unbindable(&namespace.root_view()));
    assert!(mount_propagation::master(&namespace.root_view()).is_none());
    VFS.change_mount_propagation(&caller, &f.global, MountPropagation::Shared, false)
        .unwrap();
    assert!(!mount_propagation::unbindable(&namespace.root_view()));
}

#[test]
fn peer_retirement_transfers_existing_slaves_before_releasing_actual_view() {
    let _case = CASE.lock();
    let f = Fixture::new();
    let namespace = VFS.namespace.lock().as_ref().unwrap().clone();
    let user = crate::namespace::initial_user();
    let caller = authority(user.clone());
    VFS.change_mount_propagation(&caller, &f.global, MountPropagation::Shared, false)
        .unwrap();
    let origin = LookupOrigin::new(f.global.clone(), f.global.clone());
    let (peer, _) = namespace.copy_for(user, 0, origin.clone()).unwrap();
    let child = crate::namespace::UserNamespace::child(
        &caller,
        true,
        crate::namespace::NamespaceRlimits {
            locked_memory: u64::MAX,
            pending_signals: u64::MAX,
        },
    )
    .unwrap();
    let (slave, _) = peer.copy_for(child, 0, origin).unwrap();
    assert!(Arc::ptr_eq(
        &mount_propagation::master(&slave.root_view()).unwrap(),
        &peer.root_view()
    ));
    drop(peer);
    let transaction = VFS.namespace().unwrap();
    assert!(Arc::ptr_eq(
        &mount_propagation::master(&slave.root_view()).unwrap(),
        &namespace.root_view()
    ));
    drop(transaction);
}

#[test]
fn mountinfo_reports_view_and_superblock_readonly_independently() {
    let _case = CASE.lock();
    let f = Fixture::new();
    let namespace = VFS.namespace.lock().as_ref().unwrap().clone();
    let caller = authority(crate::namespace::initial_user());
    VFS.change_mount_propagation(&caller, &f.global, MountPropagation::Shared, false)
        .unwrap();
    let view = namespace.root_view();
    view.filesystem.state.lock().read_only = true;
    let mut record = Vec::new();
    mount_table::write_mount_record(
        &mut record,
        &view,
        &f.global,
        b"/",
        b"/",
        mount_table::MountTableKind::Info,
    )
    .unwrap();
    let record = String::from_utf8(record).unwrap();
    let group = mount_propagation::group(&view).unwrap();
    assert!(record.contains(&format!(" / / rw,relatime shared:{} - fixture", group.id)));
    assert!(record.ends_with(" ro\n"));
    assert!(!view.state.lock().flags.read_only());
}
