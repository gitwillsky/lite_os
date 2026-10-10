#![feature(allocator_ext)]
#![allow(dead_code)]
//! 真实 PID identity/registration transaction；测试 observer 与 graph ownership 的不同生命周期。
extern crate alloc;
#[path = "../src/allocation_failure.rs"]
mod allocation_failure;
#[path = "../../../kernel/src/fallible_tree/mod.rs"]
mod fallible_tree;
#[path = "../src/namespace_fixture.rs"]
mod namespace;
mod task {
    use crate::namespace::{PidLinkSlot, PreparedPid, RetiredPidRegistrations};
    use crate::{allocation_failure, namespace};
    use namespace::{PidAllocationError, PidKey, PidNamespace};

    #[test]
    fn native_namespace_lookup_pins_actual_identity_without_owning_number_registration() {
        namespace::boot();
        let leaf = child();
        let mut init = PreparedPid::try_new(
            71900,
            leaf.clone(),
            &[],
            &crate::namespace::root_authority(),
        )
        .unwrap();
        init.publish().unwrap();
        let init_owner = init.take_registration();
        let mut prepared = PreparedPid::try_new(
            71901,
            leaf.clone(),
            &[],
            &crate::namespace::root_authority(),
        )
        .unwrap();
        let identity = prepared.identity();
        let number = identity.number_in(&leaf);
        assert!(leaf.lookup_identity(number).is_none());
        prepared.publish().unwrap();
        let process = prepared.take_registration();
        let group = process.clone_in(PidLinkSlot::try_new().unwrap());
        allocation_failure::fail_next();
        let found = leaf.lookup_identity(number).unwrap();
        let allocated = allocation_failure::was_consumed();
        assert!(!allocated);
        assert!(alloc::sync::Arc::ptr_eq(&found, &identity));
        let ancestor = namespace::initial_pid();
        let ancestor_number = identity.number_in(&ancestor);
        assert!(alloc::sync::Arc::ptr_eq(
            &ancestor.lookup_identity(ancestor_number).unwrap(),
            &identity
        ));
        drop(process);
        assert!(
            alloc::sync::Arc::ptr_eq(&leaf.lookup_identity(number).unwrap(), &identity),
            "actual PGID/SID link must retain the same number association"
        );
        drop(group);
        assert!(leaf.lookup_identity(number).is_none());
        assert!(ancestor.lookup_identity(ancestor_number).is_none());
        let mut reused = PreparedPid::try_new(
            71902,
            leaf.clone(),
            &[number as i32],
            &crate::namespace::root_authority(),
        )
        .unwrap();
        reused.publish().unwrap();
        let new_owner = reused.take_registration();
        let new_identity = leaf.lookup_identity(number).unwrap();
        assert!(!alloc::sync::Arc::ptr_eq(&new_identity, &found));
        assert_eq!(found.key(), 71901);
        assert_eq!(new_identity.key(), 71902);
        drop(new_owner);
        drop(init_owner);
    }

    #[test]
    fn removed_avl_storage_can_be_reinitialized_without_allocation_or_repeated_drop() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct Value(alloc::sync::Arc<AtomicUsize>);
        impl Drop for Value {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }
        let drops = alloc::sync::Arc::new(AtomicUsize::new(0));
        let mut tree = crate::fallible_tree::FallibleMap::new();
        tree.try_insert(1usize, Value(drops.clone())).unwrap();
        tree.try_insert(2usize, Value(drops.clone())).unwrap();
        let (slot, key, value) = tree.take_entry(&1).unwrap().into_slot();
        assert_eq!(key, 1);
        assert_eq!(drops.load(Ordering::Relaxed), 0);
        drop(value);
        assert_eq!(drops.load(Ordering::Relaxed), 1);
        allocation_failure::fail_next();
        tree.commit_vacant(slot.fill(3, Value(drops.clone())));
        assert!(!allocation_failure::was_consumed());
        tree.test_assert_invariants();
        assert_eq!(tree.iter().map(|(&key, _)| key).collect::<Vec<_>>(), [2, 3]);
        drop(tree);
        assert_eq!(drops.load(Ordering::Relaxed), 3);
    }

    fn child() -> alloc::sync::Arc<PidNamespace> {
        namespace::boot();
        namespace::initial_pid()
            .child(namespace::initial_user(), 0)
            .unwrap()
    }

    #[test]
    fn boot_idle_observers_share_canonical_pid_zero_without_user_number_occupancy() {
        namespace::boot();
        let first = namespace::idle_pid_identity();
        let second = namespace::idle_pid_identity();
        assert!(alloc::sync::Arc::ptr_eq(&first, &second));
        assert_eq!(first.key(), 0);
        assert_eq!(first.number_in(&namespace::initial_pid()), 0);
        assert!(alloc::sync::Arc::ptr_eq(
            &first.namespace(),
            &namespace::initial_pid()
        ));
        assert_eq!(namespace::initial_pid().lookup(0), None);
        assert!(!first.is_namespace_init());
    }

    #[test]
    fn last_graph_registration_releases_numbers_while_historical_observers_stay_alive() {
        let namespace = child();
        let mut prepared = PreparedPid::try_new(
            100,
            namespace.clone(),
            &[],
            &crate::namespace::root_authority(),
        )
        .unwrap();
        let observer = prepared.identity();
        assert_eq!(observer.number_in(&namespace), 1);
        assert_eq!(namespace.lookup(1), None);
        prepared.publish().unwrap();
        let registration = prepared.take_registration();
        let root_number = observer.number_in(&namespace::initial_pid());
        assert_eq!(namespace.lookup(1), Some(PidKey::from_identity(100)));
        let second = registration.clone_in(PidLinkSlot::try_new().unwrap());
        drop(prepared);
        drop(registration);
        assert_eq!(namespace.lookup(1), Some(PidKey::from_identity(100)));
        let mut retired = RetiredPidRegistrations::default();
        retired.retire(second);
        assert_eq!(namespace.lookup(1), Some(PidKey::from_identity(100)));
        drop(retired);
        assert_eq!(namespace.lookup(1), None);
        assert_eq!(namespace::initial_pid().lookup(root_number), None);
        assert_eq!(observer.key(), 100);
        assert_eq!(observer.number_in(&namespace), 1);
    }

    #[test]
    fn real_ladder_projects_only_ancestors_and_failed_preparation_leaves_no_number() {
        let parent = child();
        let leaf = parent.child(namespace::initial_user(), 0).unwrap();
        let unrelated = child();
        for failure in 0..7 {
            allocation_failure::fail_after(failure);
            let result = PreparedPid::try_new(
                200 + failure,
                leaf.clone(),
                &[],
                &crate::namespace::root_authority(),
            );
            let consumed = allocation_failure::was_consumed();
            assert_eq!(result.is_err(), consumed);
            drop(result);
            assert_eq!(leaf.next_member(0), None);
            assert_eq!(parent.next_member(0), None);
        }
        let mut prepared =
            PreparedPid::try_new(300, leaf.clone(), &[], &crate::namespace::root_authority())
                .unwrap();
        let observer = prepared.identity();
        assert_eq!(observer.number_in(&leaf), 1);
        assert_eq!(observer.number_in(&parent), 1);
        assert_ne!(observer.number_in(&namespace::initial_pid()), 0);
        assert_eq!(observer.number_in(&unrelated), 0);
        assert!(observer.is_namespace_init());
        assert!(alloc::sync::Arc::ptr_eq(&observer.namespace(), &leaf));
        prepared.publish().unwrap();
        assert_eq!(leaf.lookup(1), Some(PidKey::from_identity(300)));
        assert_eq!(parent.lookup(1), Some(PidKey::from_identity(300)));
        drop(prepared);
        assert_eq!(leaf.next_member(0), None);
        assert_eq!(parent.next_member(0), None);
    }

    #[test]
    fn dead_ancestor_rejects_entire_commit_without_publishing_live_leaf() {
        let parent = child();
        let mut reaper = PreparedPid::try_new(
            400,
            parent.clone(),
            &[],
            &crate::namespace::root_authority(),
        )
        .unwrap();
        reaper.publish().unwrap();
        let leaf = parent.child(namespace::initial_user(), 0).unwrap();
        let mut pending =
            PreparedPid::try_new(401, leaf.clone(), &[], &crate::namespace::root_authority())
                .unwrap();
        let observer = pending.identity();
        parent.disable(PidKey::from_identity(400), None);
        assert_eq!(pending.publish(), Err(PidAllocationError::Dead));
        assert_eq!(leaf.lookup(1), None);
        assert_eq!(parent.lookup(observer.number_in(&parent)), None);
        drop(pending);
        assert_eq!(leaf.next_member(0), None);
        drop(reaper);
        assert_eq!(parent.next_member(0), None);
    }

    #[test]
    fn requested_pid_uses_actual_index_without_moving_cyclic_cursor_or_retaining_observers() {
        let view = child();
        let root = namespace::root_authority();
        assert!(matches!(
            PreparedPid::try_new(500, view.clone(), &[2], &root),
            Err(PidAllocationError::Invalid)
        ));
        let mut init = PreparedPid::try_new(501, view.clone(), &[1], &root).unwrap();
        init.publish().unwrap();
        let reaper = init.take_registration();
        let mut selected = PreparedPid::try_new(502, view.clone(), &[7], &root).unwrap();
        assert!(matches!(
            PreparedPid::try_new(503, view.clone(), &[7], &root),
            Err(PidAllocationError::Exists)
        ));
        let observer = selected.identity();
        assert_eq!(view.lookup(7), None);
        selected.publish().unwrap();
        let registration = selected.take_registration();
        assert_eq!(view.lookup(7), Some(PidKey::from_identity(502)));
        let normal = PreparedPid::try_new(504, view.clone(), &[], &root).unwrap();
        assert_eq!(normal.identity().number_in(&view), 2);
        drop(normal);
        drop(registration);
        assert_eq!(view.lookup(7), None);
        let replacement = PreparedPid::try_new(505, view.clone(), &[7], &root).unwrap();
        assert_eq!(
            replacement.identity().number_in(&view),
            observer.number_in(&view)
        );
        assert_ne!(replacement.identity().key(), observer.key());
        drop(replacement);
        drop(reaper);
    }

    #[test]
    fn requested_pid_authorizes_every_ancestor_and_failure_returns_leaf_reservation() {
        let root_view = child();
        let root = namespace::root_authority();
        let user = namespace::UserNamespace::child(
            &root,
            true,
            crate::namespace::NamespaceRlimits {
                locked_memory: u64::MAX,
                pending_signals: u64::MAX,
            },
        )
        .unwrap();
        let view = root_view.child(user.clone(), 0).unwrap();
        let current = namespace::NamespaceAuthority {
            namespace: user,
            effective_capabilities: syscall_abi::capability::Capability::CHECKPOINT_RESTORE.mask(),
            ..root.clone()
        };
        assert!(matches!(
            PreparedPid::try_new(510, view.clone(), &[1, 1], &current),
            Err(PidAllocationError::Permission)
        ));
        assert_eq!(view.next_member(0), None);
        let permitted = PreparedPid::try_new(511, view.clone(), &[1], &current).unwrap();
        assert_eq!(permitted.identity().number_in(&view), 1);
        drop(permitted);
        assert!(matches!(
            PreparedPid::try_new(512, view.clone(), &[0], &root),
            Err(PidAllocationError::Invalid)
        ));
        assert!(matches!(
            PreparedPid::try_new(513, view.clone(), &[view.limit() as i32], &root),
            Err(PidAllocationError::Invalid)
        ));
        assert!(matches!(
            PreparedPid::try_new(514, view, &[1, 1, 1, 1], &root),
            Err(PidAllocationError::Invalid)
        ));
    }
}
