//! 实际 mount forest、资源退休与 proc codec；不复制生产路径算法。
use super::*;

#[test]
fn bind_alias_is_different_path_and_remount_uses_actual_view() {
    let _case = CASE.lock();
    let f = Fixture::new();
    let namespace = VFS.namespace.lock().as_ref().unwrap().clone();
    VFS.bind_mount(&namespace, &f.jail, &f.inner, false)
        .unwrap();
    let bound = VFS.open_file(b"/jail/inner").unwrap();
    assert_eq!(
        bound.inode_identity().unwrap(),
        f.jail.inode_identity().unwrap()
    );
    assert!(!bound.same_path(&f.jail));
    assert_eq!(bound.path(&f.global, false).unwrap(), b"/jail/inner");
    let view = bound.mount.as_ref().unwrap();
    view.reconfigure(
        mount_view::Reconfiguration::View(MountFlags::from_bits(1)),
        b"",
        &AccessIdentity,
    )
    .unwrap();
    assert!(matches!(
        bound.begin_write(),
        Err(FileSystemError::ReadOnly)
    ));
    assert!(f.jail.begin_write().is_ok());
    assert!(
        !f.jail
            .mount
            .as_ref()
            .unwrap()
            .filesystem
            .state
            .lock()
            .read_only
    );
}
#[test]
fn namespace_copy_relocates_frozen_paths_but_retains_external_paths() {
    let _case = CASE.lock();
    let f = Fixture::new();
    let namespace = VFS.namespace.lock().as_ref().unwrap().clone();
    let (copy, relocated) = namespace
        .copy_for(crate::namespace::initial_user(), 0, f.origin())
        .unwrap();
    let (root, cwd) = relocated.into_parts();
    assert!(!root.same_path(&f.jail));
    assert!(!cwd.same_path(&f.inner));
    assert!(Arc::ptr_eq(&root.dentry, &f.jail.dentry));
    assert_eq!(cwd.path(&root, false).unwrap(), b"/inner");
    let (_, retained) = copy
        .copy_for(crate::namespace::initial_user(), 0, f.origin())
        .unwrap();
    let (root, cwd) = retained.into_parts();
    assert!(root.same_path(&f.jail) && cwd.same_path(&f.inner));
}
#[test]
fn namespace_copy_after_move_uses_attachment_order_and_rejects_cycles() {
    let _case = CASE.lock();
    let f = Fixture::new();
    let namespace = VFS.namespace.lock().as_ref().unwrap().clone();
    // A 先注册，B 后注册；迁移 A 到 B 后 Vec 顺序不再是 parent 顺序。
    VFS.bind_mount(&namespace, &f.jail, &f.inner, false)
        .unwrap();
    let a = VFS.open_file(b"/jail/inner").unwrap();
    VFS.bind_mount(&namespace, &f.jail, &f.jail, false).unwrap();
    let b = VFS.open_file(b"/jail").unwrap();
    let target = VFS
        .open_file_at(
            LookupOrigin::new(f.global.clone(), b.clone()),
            b"inner",
            &AccessIdentity,
        )
        .unwrap();
    VFS.move_mount(&namespace, &a, &target).unwrap();
    assert!(matches!(
        VFS.move_mount(&namespace, &b, &a),
        Err(FileSystemError::Loop)
    ));
    let (copy, origin) = namespace
        .copy_for(
            crate::namespace::initial_user(),
            0,
            LookupOrigin::new(f.global.clone(), a),
        )
        .unwrap();
    let (root, cwd) = origin.into_parts();
    assert_eq!(cwd.path(&root, false).unwrap(), b"/jail/inner");
    assert_eq!(
        copy.snapshot().unwrap().len(),
        namespace.snapshot().unwrap().len()
    );
}
#[test]
fn mount_table_formats_use_actual_mount_parent_root_and_escaped_fields() {
    let _case = CASE.lock();
    let f = Fixture::new();
    let namespace = VFS.namespace.lock().as_ref().unwrap().clone();
    VFS.bind_mount(&namespace, &f.jail, &f.inner, false)
        .unwrap();
    let bound = VFS.open_file(b"/jail/inner").unwrap();
    let view = bound.mount.as_ref().unwrap();
    let mut output = Vec::new();
    let stats = FileSystemStatistics {
        type_name: "fixture",
        flags: 0,
    };
    mount_table::write_mount_record(
        &mut output,
        view,
        &f.global,
        b"/jail/inner",
        b"/jail",
        &stats,
        mount_table::MountTableKind::Info,
    )
    .unwrap();
    assert_eq!(
        String::from_utf8(output).unwrap(),
        format!(
            "{} {} {}:{} /jail /jail/inner rw,relatime - fixture  rw\n",
            view.id,
            view.attachment().unwrap().parent.upgrade().unwrap().id,
            DeviceNumber::decode(view.root.inode().metadata().unwrap().filesystem).major,
            DeviceNumber::decode(view.root.inode().metadata().unwrap().filesystem).minor
        )
    );
    let mut output = Vec::new();
    mount_table::write_mount_record(
        &mut output,
        view,
        &f.global,
        b"/a b\t\n\\",
        b"/jail",
        &stats,
        mount_table::MountTableKind::Stats,
    )
    .unwrap();
    assert_eq!(
        output,
        b"device  mounted on /a\\040b\\011\\012\\134 with fstype fixture\n"
    );
}
#[test]
fn recursive_bind_commits_actual_subtree_with_new_view_ids() {
    let _case = CASE.lock();
    let f = Fixture::new();
    let namespace = VFS.namespace.lock().as_ref().unwrap().clone();
    VFS.bind_mount(&namespace, &f.jail, &f.inner, false)
        .unwrap();
    let child = VFS.open_file(b"/jail/inner").unwrap();
    let target = OpenedFile::child(
        Node::new(InodeType::Directory, b""),
        f.global.clone(),
        b"copy",
    )
    .unwrap();
    VFS.bind_mount(&namespace, &f.jail, &target, true).unwrap();
    let copied = VFS.enter_mount(target).unwrap();
    let copy_child = VFS
        .open_file_at(
            LookupOrigin::new(f.global.clone(), copied),
            b"inner",
            &AccessIdentity,
        )
        .unwrap();
    assert!(!copy_child.same_path(&child));
    assert!(Arc::ptr_eq(&copy_child.dentry, &child.dentry));
    assert_ne!(
        copy_child.mount.as_ref().unwrap().id,
        child.mount.as_ref().unwrap().id
    );
    assert_eq!(copy_child.path(&f.global, false).unwrap(), b"/copy/inner");
}
#[test]
fn mount_visibility_is_real_root_reachability() {
    let _case = CASE.lock();
    let f = Fixture::new();
    assert!(!f.global.project_path(&f.jail, false).unwrap().1);
    assert!(f.inner.project_path(&f.jail, false).unwrap().1);
    assert_eq!(f.inner.project_path(&f.jail, false).unwrap().0, b"/inner");
}

#[test]
fn lazy_detach_disconnects_unlocked_children_and_preserves_locked_edges() {
    let _case = CASE.lock();
    for locked in [false, true] {
        let f = Fixture::new();
        let namespace = VFS.namespace.lock().as_ref().unwrap().clone();
        VFS.bind_mount(&namespace, &f.jail, &f.inner, false)
            .unwrap();
        let old_root = VFS.open_file(b"/jail/inner").unwrap();
        let child_point = VFS
            .open_file_at(
                LookupOrigin::new(f.global.clone(), old_root.clone()),
                b"inner",
                &AccessIdentity,
            )
            .unwrap();
        VFS.bind_mount(&namespace, &f.jail, &child_point, false)
            .unwrap();
        let old_child = VFS.enter_mount(child_point.clone()).unwrap();
        old_child
            .mount
            .as_ref()
            .unwrap()
            .state
            .lock()
            .restrictions
            .inseparable = locked;
        {
            let _transaction = VFS.namespace().unwrap();
            namespace.remove(&old_root, UnmountMode::Detach).unwrap();
            assert!(!namespace.contains(old_root.mount.as_ref().unwrap()));
            assert!(!namespace.contains(old_child.mount.as_ref().unwrap()));
            let visible = VFS.enter_mount(child_point.clone()).unwrap();
            assert_eq!(visible.same_path(&old_child), locked);
            if locked {
                assert!(
                    old_child
                        .parent(&f.global)
                        .unwrap()
                        .unwrap()
                        .same_path(&old_root)
                );
            } else {
                assert!(old_child.parent(&f.global).unwrap().is_none());
            }
        }
        drop(child_point);
        drop(old_root);
        // 只持 child FD 不保活已消亡 parent；final cleanup 仍使用 same index/path algorithm。
        while mount_retirement::has_pending() {
            let transaction = VFS.namespace().unwrap();
            drop(transaction);
        }
        assert!(old_child.parent(&f.global).unwrap().is_none());
    }
}
#[test]
fn namespace_final_drop_is_prepared_and_quota_release_does_not_wait_for_old_paths() {
    let _case = CASE.lock();
    let f = Fixture::new();
    let namespace = VFS.namespace.lock().as_ref().unwrap().clone();
    let user = crate::namespace::initial_user();
    let authority = crate::namespace::NamespaceAuthority {
        namespace: user.clone(),
        effective_uid: 0,
        effective_gid: 0,
        effective_capabilities: syscall_abi::capability::Capability::SYS_RESOURCE.mask(),
    };
    let resource = crate::namespace::UserResource::Mount;
    let previous = user.resource_limit(resource);
    user.set_resource_limit(resource, 1, &authority).unwrap();
    let (copy, origin) = namespace.copy_for(user.clone(), 0, f.origin()).unwrap();
    let view = origin.root().mount.as_ref().unwrap().clone();
    crate::allocation_failure::fail_next();
    drop(copy);
    assert!(
        !crate::allocation_failure::was_consumed(),
        "final namespace Drop must only transfer its existing token"
    );
    // Poisoned allocation hook 已由 was_consumed 收回，下一次 copy 使用同一配额槽，旧路径仍持 view。
    assert!(
        view.state.lock().namespace_member,
        "Drop must not mutate topology outside its gate"
    );
    {
        let transaction = VFS.namespace().unwrap();
        assert!(!view.state.lock().namespace_member);
        drop(transaction);
    }
    let (next, _) = namespace.copy_for(user.clone(), 0, f.origin()).unwrap();
    drop(next);
    user.set_resource_limit(resource, previous, &authority)
        .unwrap();
}
#[test]
fn normal_unmount_busy_check_is_actual_path_and_child_ownership() {
    let _case = CASE.lock();
    let f = Fixture::new();
    let namespace = VFS.namespace.lock().as_ref().unwrap().clone();
    VFS.bind_mount(&namespace, &f.jail, &f.inner, false)
        .unwrap();
    let root = VFS.open_file(b"/jail/inner").unwrap();
    let extra = root.clone();
    {
        let _transaction = VFS.namespace().unwrap();
        assert!(matches!(
            namespace.remove(&root, UnmountMode::Normal),
            Err(FileSystemError::Busy)
        ));
    }
    drop(extra);
    {
        let _transaction = VFS.namespace().unwrap();
        namespace.remove(&root, UnmountMode::Normal).unwrap();
    }
    assert!(root.parent(&f.global).unwrap().is_none());
}

#[test]
fn mount_expiry_requires_two_idle_attempts_and_real_path_release_disarms_it() {
    let _case = CASE.lock();
    let f = Fixture::new();
    let namespace = VFS.namespace.lock().as_ref().unwrap().clone();
    VFS.bind_mount(&namespace, &f.jail, &f.inner, false)
        .unwrap();
    let mut root = VFS.open_file(b"/jail/inner").unwrap();
    Arc::get_mut(&mut root).unwrap().preserve_mount_expiry();
    {
        let _transaction = VFS.namespace().unwrap();
        assert!(matches!(
            namespace.remove(&root, UnmountMode::Expire),
            Err(FileSystemError::Retry)
        ));
    }
    drop(root);
    // 普通访问的 path_put 清标；再次 expire 必须重新设标，不能卸载曾被使用的 mount。
    drop(VFS.open_file(b"/jail/inner/file").unwrap());
    let mut root = VFS.open_file(b"/jail/inner").unwrap();
    Arc::get_mut(&mut root).unwrap().preserve_mount_expiry();
    {
        let _transaction = VFS.namespace().unwrap();
        assert!(matches!(
            namespace.remove(&root, UnmountMode::Expire),
            Err(FileSystemError::Retry)
        ));
    }
    drop(root);
    let mut root = VFS.open_file(b"/jail/inner").unwrap();
    Arc::get_mut(&mut root).unwrap().preserve_mount_expiry();
    {
        let _transaction = VFS.namespace().unwrap();
        namespace.remove(&root, UnmountMode::Expire).unwrap();
    }
    assert!(!root.mount.as_ref().unwrap().state.lock().namespace_member);
    assert!(VFS.open_file(b"/jail/inner").unwrap().same_path(&f.inner));
}

#[test]
fn independent_mount_view_source_name_survives_namespace_copy_and_bind() {
    let _case = CASE.lock();
    let f = Fixture::new();
    let namespace = VFS.namespace.lock().as_ref().unwrap().clone();
    let origin = f.jail.mount.as_ref().unwrap();
    let view = MountView::new(
        namespace.scope.clone(),
        Arc::new(b"alias\tname".to_vec()),
        origin.filesystem.clone(),
        f.jail.dentry.clone(),
        Some(Attachment::new(origin, f.inner.dentry.clone())),
        MountFlags::default(),
        MountRestrictions::default(),
    )
    .unwrap();
    {
        let _transaction = VFS.namespace().unwrap();
        let mut nodes = [Some(mount_index::prepare(&view).unwrap())];
        mount_publication::Publication::prepare(
            core::slice::from_ref(&view),
            &mut nodes,
            Vec::new(),
            Vec::new(),
            None,
        )
        .unwrap()
        .publish();
    }
    let bound = VFS.open_file(b"/jail/inner").unwrap();
    let (copy, _) = namespace
        .copy_for(crate::namespace::initial_user(), 0, f.origin())
        .unwrap();
    let _transaction = VFS.namespace().unwrap();
    let copied = copy.snapshot().unwrap();
    assert!(
        copied
            .iter()
            .any(|view| view.source.as_slice() == b"alias\tname")
    );
    assert_ne!(
        bound.mount.as_ref().unwrap().source.as_slice(),
        origin.source.as_slice()
    );
}
