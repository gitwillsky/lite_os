#![feature(allocator_ext)]
#![allow(dead_code)]
//! Production namespace ancestry/map/identity/permission owner; no copied namespace state.
extern crate alloc;
#[path = "../../../kernel/src/fallible_tree/mod.rs"]
mod fallible_tree;
use alloc::sync::Arc;
use syscall_abi::capability::{Capability, VALID_MASK};
#[path = "../src/allocation_failure.rs"]
mod allocation_failure;
#[path = "../../../kernel/src/namespace/mod.rs"]
#[allow(
    unused_imports,
    reason = "本用例只覆盖实际 user namespace owner，其他导出由独立集成用例覆盖"
)]
mod namespace;
use namespace::{NamespaceAuthority, NamespaceError, UserNamespace};
fn authority(uid: u32, gid: u32, caps: u64) -> NamespaceAuthority {
    namespace::initialize(i32::MAX as u32, 8192).unwrap();
    NamespaceAuthority {
        namespace: namespace::initial_user(),
        effective_uid: uid,
        effective_gid: gid,
        effective_capabilities: caps,
    }
}
#[test]
fn actual_userns_capabilities_cannot_escape_to_ancestor_and_owner_authority_is_explicit() {
    let parent = authority(1000, 1001, 0);
    let child = UserNamespace::child(
        &parent,
        true,
        crate::namespace::NamespaceRlimits {
            locked_memory: u64::MAX,
            pending_signals: u64::MAX,
        },
    )
    .unwrap();
    let current = NamespaceAuthority {
        namespace: child.clone(),
        effective_capabilities: VALID_MASK,
        ..parent.clone()
    };
    assert!(parent.capable(&child, Capability::SYS_ADMIN));
    assert!(current.capable(&child, Capability::SYS_ADMIN));
    assert!(!current.capable(&parent.namespace, Capability::SYS_ADMIN));
    assert!(
        !authority(2000, 2001, VALID_MASK & !Capability::SYS_ADMIN.mask())
            .capable(&child, Capability::SYS_ADMIN)
    );
    assert_eq!(child.display_id(true, 1000), 65534);
}
#[test]
fn actual_unprivileged_mapping_and_setgroups_are_one_write_once_transaction() {
    let parent = authority(1000, 1001, 0);
    let child = UserNamespace::child(
        &parent,
        true,
        crate::namespace::NamespaceRlimits {
            locked_memory: u64::MAX,
            pending_signals: u64::MAX,
        },
    )
    .unwrap();
    let current = NamespaceAuthority {
        namespace: child.clone(),
        effective_capabilities: VALID_MASK,
        ..parent.clone()
    };
    child
        .publish_map(true, b"0 1000 1\n", &current, &current)
        .unwrap();
    assert_eq!(child.to_kernel(true, 0), Some(1000));
    assert_eq!(child.map_kernel_id(true, 1000), Some(0));
    assert_eq!(
        child.publish_map(true, b"bad", &current, &current),
        Err(NamespaceError::Permission)
    );
    assert_eq!(
        child.publish_map(false, b"0 1001 1\n", &current, &current),
        Err(NamespaceError::Permission)
    );
    assert!(child.map(false).is_none());
    child.control_setgroups(false, &current).unwrap();
    child
        .publish_map(false, b"0 1001 1\n", &current, &current)
        .unwrap();
    assert!(!child.may_setgroups());
    assert_eq!(
        child.control_setgroups(true, &current),
        Err(NamespaceError::Permission)
    );
    assert_eq!(child.to_kernel(false, 0), Some(1001));
}
#[test]
fn actual_mapping_rejects_parent_holes_and_all_overlap_overflow_sentinel_ranges() {
    let parent = authority(0, 0, VALID_MASK);
    let child = UserNamespace::child(
        &parent,
        true,
        crate::namespace::NamespaceRlimits {
            locked_memory: u64::MAX,
            pending_signals: u64::MAX,
        },
    )
    .unwrap();
    for bytes in [
        b"0 0 0\n".as_slice(),
        b"0 4294967295 1\n",
        b"4294967295 0 1\n",
        b"0 1 4294967295\n",
        b"0 1 2\n1 3 1\n",
        b"0 1 2\n3 2 1\n",
        b"0 0 1 junk\n",
    ] {
        assert_eq!(
            child.publish_map(true, bytes, &parent, &parent),
            Err(NamespaceError::Invalid)
        );
        assert!(child.map(true).is_none());
    }
    child
        .publish_map(true, b"0 0 1\n2 2 1\n", &parent, &parent)
        .unwrap();
    child
        .publish_map(false, b"0 0 1\n", &parent, &parent)
        .unwrap();
    let current = NamespaceAuthority {
        namespace: child.clone(),
        ..parent.clone()
    };
    let nested = UserNamespace::child(
        &current,
        true,
        crate::namespace::NamespaceRlimits {
            locked_memory: u64::MAX,
            pending_signals: u64::MAX,
        },
    )
    .unwrap();
    assert_eq!(
        nested.publish_map(true, b"0 0 3\n", &current, &current),
        Err(NamespaceError::Permission)
    );
    assert!(nested.map(true).is_none());
}
#[test]
fn actual_map_opener_credentials_and_setfcap_root_mapping_cannot_be_replaced_by_writer() {
    let parent = authority(1000, 1001, 0);
    let child = UserNamespace::child(
        &parent,
        true,
        crate::namespace::NamespaceRlimits {
            locked_memory: u64::MAX,
            pending_signals: u64::MAX,
        },
    )
    .unwrap();
    let root = authority(0, 0, VALID_MASK);
    assert_eq!(
        child.publish_map(true, b"0 0 1\n", &parent, &root),
        Err(NamespaceError::Permission)
    );
    child
        .publish_map(true, b"0 1000 1\n", &parent, &root)
        .unwrap();
    assert_eq!(child.to_kernel(true, 0), Some(1000));
    let other = UserNamespace::child(
        &root,
        true,
        crate::namespace::NamespaceRlimits {
            locked_memory: u64::MAX,
            pending_signals: u64::MAX,
        },
    )
    .unwrap();
    let foreign = NamespaceAuthority {
        namespace: other,
        effective_capabilities: VALID_MASK,
        ..root.clone()
    };
    assert_eq!(
        child.publish_map(false, b"0 1001 1\n", &foreign, &root),
        Err(NamespaceError::Permission)
    );
}
#[test]
fn actual_namespace_map_oom_never_publishes_partial_ranges() {
    let parent = authority(0, 0, VALID_MASK);
    for stage in 0..5 {
        let child = UserNamespace::child(
            &parent,
            true,
            crate::namespace::NamespaceRlimits {
                locked_memory: u64::MAX,
                pending_signals: u64::MAX,
            },
        )
        .unwrap();
        allocation_failure::fail_after(stage);
        let result = child.publish_map(true, b"0 0 2\n", &parent, &parent);
        let consumed = allocation_failure::was_consumed();
        if consumed {
            assert_eq!(result, Err(NamespaceError::OutOfMemory));
            assert!(child.map(true).is_none());
        } else {
            assert_eq!(result, Ok(()));
            assert_eq!(child.to_kernel(true, 1), Some(1));
        }
    }
}
#[test]
fn actual_kernel_to_local_reverse_index_preserves_unsorted_input_without_copying_extents() {
    let parent = authority(0, 0, VALID_MASK);
    let child = UserNamespace::child(
        &parent,
        true,
        crate::namespace::NamespaceRlimits {
            locked_memory: u64::MAX,
            pending_signals: u64::MAX,
        },
    )
    .unwrap();
    child
        .publish_map(true, b"9 2 3\n0 100 4\n4 0 2\n", &parent, &parent)
        .unwrap();
    for (local, kernel) in [(9, 2), (11, 4), (0, 100), (3, 103), (4, 0), (5, 1)] {
        assert_eq!(child.to_kernel(true, local), Some(kernel));
        assert_eq!(child.map_kernel_id(true, kernel), Some(local));
    }
    assert_eq!(child.to_kernel(true, 6), None);
    assert_eq!(child.map_kernel_id(true, 5), None);
    assert_eq!(Arc::strong_count(&child), 1);
}

#[test]
fn actual_uts_copy_owns_names_and_child_capabilities_cannot_modify_parent_uts() {
    let parent = authority(1000, 1001, 0);
    namespace::initialize_uts(b"initial", b"(none)").unwrap();
    let initial = namespace::initial_uts();
    let user = UserNamespace::child(
        &parent,
        true,
        crate::namespace::NamespaceRlimits {
            locked_memory: u64::MAX,
            pending_signals: u64::MAX,
        },
    )
    .unwrap();
    let current = NamespaceAuthority {
        namespace: user.clone(),
        effective_capabilities: VALID_MASK,
        ..parent.clone()
    };
    let copied = initial.copy_for(user, parent.effective_uid).unwrap();
    assert_ne!(initial.id(), copied.id());
    assert_eq!(copied.set_name(true, b"child", &current), Ok(()));
    assert_eq!(&copied.snapshot().hostname[..6], b"child\0");
    assert_eq!(&initial.snapshot().hostname[..8], b"initial\0");
    assert_eq!(
        initial.set_name(true, b"escape", &current),
        Err(NamespaceError::Permission)
    );
    let before = copied.snapshot().hostname;
    assert_eq!(
        copied.set_name(true, &[b'x'; 65], &current),
        Err(NamespaceError::Invalid)
    );
    assert_eq!(copied.snapshot().hostname, before);
    copied.set_name(false, b"a\0\xff", &current).unwrap();
    assert_eq!(&copied.snapshot().domainname[..4], b"a\0\xff\0");
}

fn mapped_child(parent: &NamespaceAuthority) -> NamespaceAuthority {
    let child = UserNamespace::child(
        parent,
        true,
        crate::namespace::NamespaceRlimits {
            locked_memory: u64::MAX,
            pending_signals: u64::MAX,
        },
    )
    .unwrap();
    let current = NamespaceAuthority {
        namespace: child.clone(),
        effective_capabilities: VALID_MASK,
        ..parent.clone()
    };
    let uid_map: &[u8] = if parent.namespace.is_initial() {
        b"0 1000 1\n"
    } else {
        b"0 0 1\n"
    };
    let gid_map: &[u8] = if parent.namespace.is_initial() {
        b"0 1001 1\n"
    } else {
        b"0 0 1\n"
    };
    child.publish_map(true, uid_map, parent, parent).unwrap();
    child.control_setgroups(false, &current).unwrap();
    child.publish_map(false, gid_map, parent, parent).unwrap();
    current
}
#[test]
fn namespace_quota_counts_lifetime_per_kuid_and_releases_on_actual_final_drop() {
    use namespace::UserResource;
    let parent = authority(1000, 1001, 0);
    let current = mapped_child(&parent);
    current
        .namespace
        .set_resource_limit(UserResource::User, 1, &current)
        .unwrap();
    let first = UserNamespace::child(
        &current,
        true,
        crate::namespace::NamespaceRlimits {
            locked_memory: u64::MAX,
            pending_signals: u64::MAX,
        },
    )
    .unwrap();
    assert!(matches!(
        UserNamespace::child(
            &current,
            true,
            crate::namespace::NamespaceRlimits {
                locked_memory: u64::MAX,
                pending_signals: u64::MAX
            }
        ),
        Err(NamespaceError::QuotaLimit)
    ));
    let pin = first.clone();
    drop(first);
    assert!(matches!(
        UserNamespace::child(
            &current,
            true,
            crate::namespace::NamespaceRlimits {
                locked_memory: u64::MAX,
                pending_signals: u64::MAX
            }
        ),
        Err(NamespaceError::QuotaLimit)
    ));
    drop(pin);
    assert!(
        UserNamespace::child(
            &current,
            true,
            crate::namespace::NamespaceRlimits {
                locked_memory: u64::MAX,
                pending_signals: u64::MAX
            }
        )
        .is_ok()
    );
    current
        .namespace
        .set_resource_limit(UserResource::User, 0, &current)
        .unwrap();
    assert!(matches!(
        UserNamespace::child(
            &current,
            true,
            crate::namespace::NamespaceRlimits {
                locked_memory: u64::MAX,
                pending_signals: u64::MAX
            }
        ),
        Err(NamespaceError::QuotaLimit)
    ));
}
#[test]
fn namespace_quota_ancestor_denial_rolls_back_leaf_and_caps_cannot_modify_parent_limit() {
    use namespace::UserResource;
    let parent = authority(1000, 1001, 0);
    let current = mapped_child(&parent);
    let descendant = mapped_child(&current);
    namespace::initialize_uts(b"initial", b"(none)").unwrap();
    let uts = namespace::initial_uts();
    descendant
        .namespace
        .set_resource_limit(UserResource::Uts, 1, &descendant)
        .unwrap();
    current
        .namespace
        .set_resource_limit(UserResource::Uts, 0, &current)
        .unwrap();
    assert!(matches!(
        uts.copy_for(descendant.namespace.clone(), 1000),
        Err(NamespaceError::QuotaLimit)
    ));
    current
        .namespace
        .set_resource_limit(UserResource::Uts, 1, &current)
        .unwrap();
    let copied = uts.copy_for(descendant.namespace.clone(), 1000).unwrap();
    assert!(matches!(
        uts.copy_for(descendant.namespace.clone(), 1000),
        Err(NamespaceError::QuotaLimit)
    ));
    drop(copied);
    assert!(uts.copy_for(descendant.namespace.clone(), 1000).is_ok());
    assert_eq!(
        current
            .namespace
            .set_resource_limit(UserResource::Uts, 10, &descendant),
        Err(NamespaceError::Permission)
    );
}
#[test]
fn namespace_quota_allocation_and_object_oom_return_charges_without_partial_owner() {
    use namespace::UserResource;
    let parent = authority(1000, 1001, 0);
    let current = mapped_child(&parent);
    current
        .namespace
        .set_resource_limit(UserResource::User, 1, &current)
        .unwrap();
    // One cold quota-entry allocation is prewarmed; then inject chain token and object allocations.
    drop(
        UserNamespace::child(
            &current,
            true,
            crate::namespace::NamespaceRlimits {
                locked_memory: u64::MAX,
                pending_signals: u64::MAX,
            },
        )
        .unwrap(),
    );
    for stage in 0..2 {
        allocation_failure::fail_after(stage);
        let result = UserNamespace::child(
            &current,
            true,
            crate::namespace::NamespaceRlimits {
                locked_memory: u64::MAX,
                pending_signals: u64::MAX,
            },
        );
        let consumed = allocation_failure::was_consumed();
        assert!(consumed);
        assert!(matches!(
            result,
            Err(NamespaceError::QuotaLimit | NamespaceError::OutOfMemory)
        ));
        assert!(
            UserNamespace::child(
                &current,
                true,
                crate::namespace::NamespaceRlimits {
                    locked_memory: u64::MAX,
                    pending_signals: u64::MAX
                }
            )
            .is_ok()
        );
    }
}

#[test]
#[ignore = "CPU diagnostic for actual quota entry lookup and cold namespace lifetime; run in final consolidated verification"]
fn actual_namespace_quota_creation_cpu_cost() {
    let mut creator = authority(10_000, 1000, 0);
    let mut population = Vec::new();
    for uid in 10_000..11_024 {
        creator.effective_uid = uid;
        population.push(
            UserNamespace::child(
                &creator,
                true,
                crate::namespace::NamespaceRlimits {
                    locked_memory: u64::MAX,
                    pending_signals: u64::MAX,
                },
            )
            .unwrap(),
        );
    }
    creator.effective_uid = 10_500;
    for _ in 0..1000 {
        std::hint::black_box(
            UserNamespace::child(
                &creator,
                true,
                crate::namespace::NamespaceRlimits {
                    locked_memory: u64::MAX,
                    pending_signals: u64::MAX,
                },
            )
            .unwrap(),
        );
    }
    let mut samples = [0f64; 5];
    for sample in &mut samples {
        let start = std::time::Instant::now();
        for _ in 0..10_000 {
            std::hint::black_box(
                UserNamespace::child(
                    &creator,
                    true,
                    crate::namespace::NamespaceRlimits {
                        locked_memory: u64::MAX,
                        pending_signals: u64::MAX,
                    },
                )
                .unwrap(),
            );
        }
        *sample = start.elapsed().as_nanos() as f64 / 10_000.0;
    }
    samples.sort_by(f64::total_cmp);
    println!(
        "actual namespace creation with 1024 creator entries: {:.2} ns/op; user owner={} bytes",
        samples[2],
        core::mem::size_of::<UserNamespace>()
    );
    drop(population);
}

#[test]
fn actual_namespace_reference_retains_same_owner_and_typed_validation_allocates_nothing() {
    let parent = authority(1000, 1001, 0);
    let child = UserNamespace::child(
        &parent,
        true,
        crate::namespace::NamespaceRlimits {
            locked_memory: u64::MAX,
            pending_signals: u64::MAX,
        },
    )
    .unwrap();
    allocation_failure::fail_next();
    let reference = namespace::NamespaceReference::new(child.clone());
    let actual = reference.downcast::<UserNamespace>().unwrap();
    let wrong = reference.downcast::<namespace::UtsNamespace>();
    let allocated = allocation_failure::was_consumed();
    assert!(!allocated);
    assert!(Arc::ptr_eq(&actual, &child));
    assert_eq!(
        (reference.kind(), reference.id()),
        (namespace::NamespaceType::User, child.id())
    );
    assert!(matches!(wrong, Err(NamespaceError::Invalid)));
    let ancestor = reference
        .parent_reference()
        .unwrap()
        .unwrap()
        .downcast::<UserNamespace>()
        .unwrap();
    assert!(Arc::ptr_eq(&ancestor, &parent.namespace));
}

#[test]
fn actual_domain_namespace_reference_needs_no_reverse_domain_dependency() {
    #[derive(Debug)]
    struct DomainNamespace {
        owner: Arc<UserNamespace>,
    }
    impl namespace::NamespaceOwner for DomainNamespace {
        fn kind(&self) -> namespace::NamespaceType {
            namespace::NamespaceType::Uts
        }
        fn id(&self) -> u64 {
            99
        }
        fn owning_user(&self) -> Option<Arc<UserNamespace>> {
            Some(self.owner.clone())
        }
    }
    let parent = authority(1000, 1001, 0);
    let actual = Arc::new(DomainNamespace {
        owner: parent.namespace.clone(),
    });
    let reference = namespace::NamespaceReference::new(actual.clone());
    assert!(Arc::ptr_eq(
        &reference.downcast::<DomainNamespace>().unwrap(),
        &actual
    ));
    assert!(Arc::ptr_eq(
        &reference.owning_user().unwrap(),
        &parent.namespace
    ));
    assert!(matches!(
        reference.downcast::<namespace::UtsNamespace>(),
        Err(NamespaceError::Invalid)
    ));
    assert!(matches!(
        reference.parent_reference(),
        Err(NamespaceError::Invalid)
    ));
}
/// io_uring ring 内存按真实 UID 计费：达到预算后再申请得到 `QuotaLimit`，归还后可再次申请，
/// 其他 UID 的预算互不影响，子 UserNS 里的收费同时记到祖先上（上限不能靠新建 UserNS 绕过）。
#[test]
fn io_uring_ring_bytes_are_charged_per_uid_and_through_ancestor_namespaces() {
    use namespace::{RlimitCharge, RlimitResource};
    let _ = authority(0, 0, 0);
    let budget = namespace::IO_URING_BYTES_PER_UID;
    let acquire = |ns: Arc<UserNamespace>, uid: u32, bytes: u64| {
        RlimitCharge::acquire(ns, uid, RlimitResource::IoUringBytes, bytes, budget, false)
    };
    let root = namespace::initial_user();
    let first = acquire(root.clone(), 701_001, budget).unwrap();
    assert!(matches!(
        acquire(root.clone(), 701_001, 1),
        Err(NamespaceError::QuotaLimit)
    ));
    // 别的 UID 有自己的预算。
    let other = acquire(root.clone(), 701_002, budget).unwrap();
    drop(other);
    // 归还后可以再申请。
    drop(first);
    let again = acquire(root.clone(), 701_001, budget).unwrap();
    // 子 UserNS（所有者 701_001）里的收费沿祖先链记到所有者 UID 上：预算已满则被拒。
    let parent = NamespaceAuthority {
        namespace: root.clone(),
        effective_uid: 701_001,
        effective_gid: 701_001,
        effective_capabilities: 0,
    };
    let child = UserNamespace::child(
        &parent,
        true,
        namespace::NamespaceRlimits {
            locked_memory: u64::MAX,
            pending_signals: u64::MAX,
        },
    )
    .unwrap();
    assert!(matches!(
        acquire(child.clone(), 0, 1),
        Err(NamespaceError::QuotaLimit)
    ));
    drop(again);
    drop(acquire(child, 0, 4096).unwrap());
}
