#![cfg(test)]
use super::*;

fn byte(vmo: &Arc<Vmo>, offset: u64) -> u8 {
    let mut value = [0];
    vmo.read(offset, &mut value).unwrap();
    value[0]
}

#[test]
#[ignore = "诊断生产快照/退休成本，不设置宿主时间阈值"]
fn snapshot_retirement_cpu_cost() {
    let parent = Vmo::create(PAGE_SIZE as u64, VmoOptions::default()).unwrap();
    parent.write(0, &[37]).unwrap();
    let siblings: Vec<_> = (0..272)
        .map(|_| {
            parent
                .create_child(0, parent.size(), ChildKind::Snapshot)
                .unwrap()
        })
        .collect();
    let mut pending = parent
        .create_child(0, parent.size(), ChildKind::Snapshot)
        .unwrap();
    let started = std::time::Instant::now();
    for _ in 0..4096 {
        let next = parent
            .create_child(0, parent.size(), ChildKind::Snapshot)
            .unwrap();
        drop(pending);
        pending = next;
    }
    println!(
        "snapshot/retirement with {} resident siblings: {:.2} us/op",
        siblings.len(),
        started.elapsed().as_secs_f64() * 1_000_000.0 / 4096.0
    );
}

#[test]
fn fork_exit_churn_releases_hidden_ancestors() {
    let parent = Vmo::create(PAGE_SIZE as u64, VmoOptions::default()).unwrap();
    parent.write(0, &[37]).unwrap();
    for _ in 0..64 {
        let child = parent
            .create_child(0, parent.size(), ChildKind::Snapshot)
            .unwrap();
        let hidden = match &parent.state.lock().relation {
            Some(Relation::Cow { parent, .. }) => Arc::downgrade(parent),
            _ => panic!("snapshot must freeze the original contents"),
        };
        child.write(0, &[91]).unwrap();
        assert_eq!(byte(&parent, 0), 37);
        drop(child);
        assert_eq!(byte(&parent, 0), 37);
        assert!(
            hidden.upgrade().is_none(),
            "exited child retained a hidden ancestor"
        );
    }
}

#[test]
fn fork_exit_churn_with_resident_siblings_releases_transient_ancestors() {
    let parent = Vmo::create(PAGE_SIZE as u64, VmoOptions::default()).unwrap();
    parent.write(0, &[37]).unwrap();
    let siblings: Vec<_> = (0..8)
        .map(|_| {
            parent
                .create_child(0, parent.size(), ChildKind::Snapshot)
                .unwrap()
        })
        .collect();
    for _ in 0..64 {
        let child = parent
            .create_child(0, parent.size(), ChildKind::Snapshot)
            .unwrap();
        let hidden = match &parent.state.lock().relation {
            Some(Relation::Cow { parent, .. }) => Arc::downgrade(parent),
            _ => panic!("snapshot must freeze the original contents"),
        };
        drop(child);
        let next = parent
            .create_child(0, parent.size(), ChildKind::Snapshot)
            .unwrap();
        assert!(
            hidden.upgrade().is_none(),
            "transient child retained a hidden ancestor"
        );
        drop(next);
    }
    for sibling in siblings {
        assert_eq!(byte(&sibling, 0), 37);
    }
}

/// 每个常驻快照保留一层隐藏节点；整条祖先链释放时析构深度必须恒定，不能随链长递归。
/// 在只有 64 KiB 栈的线程上释放 4096 层链：递归析构会在这里栈溢出（内核中表现为撞 guard page）。
#[test]
fn releasing_a_deep_hidden_ancestor_chain_uses_bounded_stack() {
    std::thread::Builder::new()
        .stack_size(64 * 1024)
        .spawn(|| {
            let parent = Vmo::create(PAGE_SIZE as u64, VmoOptions::default()).unwrap();
            parent.write(0, &[11]).unwrap();
            let siblings: Vec<_> = (0..4096)
                .map(|_| {
                    parent
                        .create_child(0, parent.size(), ChildKind::Snapshot)
                        .unwrap()
                })
                .collect();
            assert_eq!(byte(&siblings[4095], 0), 11);
            drop(siblings);
            drop(parent);
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn snapshot_prunes_ancestors_when_exit_notification_precedes_mapping_retirement() {
    let parent = Vmo::create(PAGE_SIZE as u64, VmoOptions::default()).unwrap();
    parent.write(0, &[37]).unwrap();
    let retired = parent
        .create_child(0, parent.size(), ChildKind::Snapshot)
        .unwrap();
    let hidden = match &parent.state.lock().relation {
        Some(Relation::Cow { parent, .. }) => Arc::downgrade(parent),
        _ => panic!("snapshot must freeze the original contents"),
    };
    // 新快照先于上一个子进程的映射退休，直接父节点暂时有两个存活子对象。
    let pending = parent
        .create_child(0, parent.size(), ChildKind::Snapshot)
        .unwrap();
    drop(retired);
    let next = parent
        .create_child(0, parent.size(), ChildKind::Snapshot)
        .unwrap();
    assert!(
        hidden.upgrade().is_none(),
        "retired mapping retained an intermediate ancestor"
    );
    assert_eq!(byte(&pending, 0), 37);
    assert_eq!(byte(&next, 0), 37);
    assert_eq!(byte(&parent, 0), 37);
}

#[test]
fn surviving_offset_snapshot_keeps_contents_and_live_sibling_isolation() {
    let original = Vmo::create((4 * PAGE_SIZE) as u64, VmoOptions::default()).unwrap();
    original.write(PAGE_SIZE as u64, &[11]).unwrap();
    original.write((2 * PAGE_SIZE) as u64, &[22]).unwrap();
    let survivor = original
        .create_child(
            PAGE_SIZE as u64,
            (2 * PAGE_SIZE) as u64,
            ChildKind::Snapshot,
        )
        .unwrap();
    let sibling = survivor
        .create_child(0, survivor.size(), ChildKind::Snapshot)
        .unwrap();
    survivor.write(0, &[33]).unwrap();
    drop(original);
    assert_eq!(byte(&survivor, 0), 33);
    assert_eq!(byte(&survivor, PAGE_SIZE as u64), 22);
    assert_eq!(byte(&sibling, 0), 11);
    drop(survivor);
    assert_eq!(byte(&sibling, 0), 11);
    assert_eq!(byte(&sibling, PAGE_SIZE as u64), 22);
}

#[test]
fn collapse_preserves_truncated_inheritance_after_regrowth() {
    let parent = Vmo::create((4 * PAGE_SIZE) as u64, VmoOptions { resizable: true }).unwrap();
    parent.write(0, &[7]).unwrap();
    parent.write((2 * PAGE_SIZE) as u64, &[99]).unwrap();
    let snapshot = parent
        .create_child(0, parent.size(), ChildKind::Snapshot)
        .unwrap();
    parent.set_size(PAGE_SIZE as u64).unwrap();
    let child = parent
        .create_child(0, (4 * PAGE_SIZE) as u64, ChildKind::Snapshot)
        .unwrap();
    drop(parent);
    assert_eq!(byte(&child, 0), 7);
    assert_eq!(byte(&child, (2 * PAGE_SIZE) as u64), 0);
    assert_eq!(byte(&snapshot, 0), 7);
    assert_eq!(byte(&snapshot, (2 * PAGE_SIZE) as u64), 99);
}

#[test]
fn available_read_never_requests_pages_and_observes_supplied_cow_content() {
    struct NoRequests;
    impl PagerSink for NoRequests {
        fn request(&self, _: PagerRequest) -> Result<(), ()> {
            panic!("an available read must not issue a pager request")
        }
    }
    let file = Vmo::create_paged(
        (2 * PAGE_SIZE) as u64,
        VmoOptions::default(),
        PagerBinding {
            sink: Arc::new(NoRequests),
            key: 1,
        },
    )
    .unwrap();
    let mut output = vec![0xcc; 2 * PAGE_SIZE];
    assert!(!file.read_available(0, &mut output).unwrap());
    assert!(file.state.lock().requested.is_empty());
    let supply = Vmo::create((2 * PAGE_SIZE) as u64, VmoOptions::default()).unwrap();
    supply.write(0, &[7]).unwrap();
    supply.write(PAGE_SIZE as u64, &[9]).unwrap();
    file.supply_pages(0, PAGE_SIZE as u64, &supply, 0).unwrap();
    assert!(!file.read_available(0, &mut output).unwrap());
    assert!(file.state.lock().requested.is_empty());
    file.supply_pages(
        PAGE_SIZE as u64,
        PAGE_SIZE as u64,
        &supply,
        PAGE_SIZE as u64,
    )
    .unwrap();
    let snapshot = file
        .create_child(0, file.size(), ChildKind::AtLeastOnWrite)
        .unwrap();
    assert!(snapshot.read_available(0, &mut output).unwrap());
    assert_eq!((output[0], output[PAGE_SIZE]), (7, 9));
    assert!(snapshot.state.lock().committed.is_empty());
    assert!(matches!(
        file.read_available(file.size(), &mut [0]),
        Err(VmError::OutOfRange)
    ));
}

#[test]
fn available_read_does_not_enqueue_behind_a_held_tree_lock() {
    let object = Vmo::create(PAGE_SIZE as u64, VmoOptions::default()).unwrap();
    object.write(0, &[11]).unwrap();
    let held = object.tree.lock.lock().unwrap();
    let mut byte = [0];
    assert!(!object.read_available(0, &mut byte).unwrap());
    drop(held);
    assert!(object.read_available(0, &mut byte).unwrap());
    assert_eq!(byte, [11]);
}

#[test]
fn pager_read_cannot_grant_write_before_dirty_transition_and_writeback_rearms_it() {
    struct NoRequests;
    impl PagerSink for NoRequests {
        fn request(&self, _: PagerRequest) -> Result<(), ()> {
            panic!("page was supplied");
        }
    }
    let file = Vmo::create_paged(
        PAGE_SIZE as u64,
        VmoOptions::default(),
        PagerBinding {
            sink: Arc::new(NoRequests),
            key: 1,
        },
    )
    .unwrap();
    let source = Vmo::create(PAGE_SIZE as u64, VmoOptions::default()).unwrap();
    source.write(0, &[7]).unwrap();
    file.supply_pages(0, PAGE_SIZE as u64, &source, 0).unwrap();
    assert!(
        !file
            .with_resolved(0, false, |page| Ok(page.writable))
            .unwrap()
    );
    assert!(
        file.first_dirty_range(0, PAGE_SIZE as u64)
            .unwrap()
            .is_none()
    );
    assert!(
        file.with_resolved(0, true, |page| Ok(page.writable))
            .unwrap()
    );
    assert_eq!(
        file.first_dirty_range(0, PAGE_SIZE as u64).unwrap(),
        Some((0, PAGE_SIZE as u64))
    );
    file.begin_writeback(0, PAGE_SIZE as u64).unwrap();
    assert!(
        !file
            .with_resolved(0, false, |page| Ok(page.writable))
            .unwrap()
    );
    assert!(
        file.first_dirty_range(0, PAGE_SIZE as u64)
            .unwrap()
            .is_none()
    );
}

#[test]
fn rejected_pager_delivery_does_not_leave_a_request_that_can_never_complete() {
    use core::sync::atomic::{AtomicUsize, Ordering};
    struct Reject(AtomicUsize);
    impl PagerSink for Reject {
        fn request(&self, _: PagerRequest) -> Result<(), ()> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Err(())
        }
    }
    let sink = Arc::new(Reject(AtomicUsize::new(0)));
    let file = Vmo::create_paged(
        PAGE_SIZE as u64,
        VmoOptions::default(),
        PagerBinding {
            sink: sink.clone(),
            key: 1,
        },
    )
    .unwrap();
    for _ in 0..2 {
        assert_eq!(file.read(0, &mut [0]), Err(VmError::Io));
        assert!(
            file.state.lock().requested.is_empty(),
            "failed delivery must retire its request admission"
        );
    }
    assert_eq!(sink.0.load(Ordering::Relaxed), 2);
}

#[test]
fn supplied_page_invalidated_before_waiter_resumes_is_not_a_pager_failure() {
    struct Accept;
    impl PagerSink for Accept {
        fn request(&self, _: PagerRequest) -> Result<(), ()> {
            Ok(())
        }
    }
    let file = Vmo::create_paged(
        PAGE_SIZE as u64,
        VmoOptions::default(),
        PagerBinding {
            sink: Arc::new(Accept),
            key: 1,
        },
    )
    .unwrap();
    let request = file.request_page(0).unwrap();
    let source = Vmo::create(PAGE_SIZE as u64, VmoOptions::default()).unwrap();
    source.write(0, &[7]).unwrap();
    file.supply_pages(0, PAGE_SIZE as u64, &source, 0).unwrap();
    file.op_range(RangeOp::Decommit, 0, PAGE_SIZE as u64)
        .unwrap();
    assert_eq!(
        file.wait_for_page(&request),
        Ok(()),
        "successful delivery remains successful after invalidation; the resolver must request again"
    );
    let failed = file.request_page(0).unwrap();
    file.fail_range(0, PAGE_SIZE as u64).unwrap();
    let retry = file.request_page(0).unwrap();
    assert!(!Arc::ptr_eq(&failed, &retry));
    assert_eq!(file.wait_for_page(&failed), Err(VmError::Io));
    assert_eq!(file.wait_for_page(&request), Ok(()));
    source.write(0, &[9]).unwrap();
    file.supply_pages(0, PAGE_SIZE as u64, &source, 0).unwrap();
    assert_eq!(file.wait_for_page(&retry), Ok(()));
    assert_eq!(file.wait_for_page(&failed), Err(VmError::Io));
}

#[test]
fn shrinking_a_paged_object_completes_requests_beyond_the_new_end() {
    struct Accept;
    impl PagerSink for Accept {
        fn request(&self, _: PagerRequest) -> Result<(), ()> {
            Ok(())
        }
    }
    let file = Vmo::create_paged(
        (2 * PAGE_SIZE) as u64,
        VmoOptions { resizable: true },
        PagerBinding {
            sink: Arc::new(Accept),
            key: 1,
        },
    )
    .unwrap();
    let kept = file.request_page(0).unwrap();
    let beyond = file.request_page(1).unwrap();
    file.set_size(PAGE_SIZE as u64).unwrap();
    // 越界请求完成后等待者回到解析循环，由新大小判定越界；不能永远等一个不再存在的页。
    assert_eq!(file.wait_for_page(&beyond), Ok(()));
    assert_eq!(
        file.read(PAGE_SIZE as u64, &mut [0]),
        Err(VmError::OutOfRange)
    );
    assert!(
        Arc::ptr_eq(&kept, &file.request_page(0).unwrap()),
        "in-range request survives"
    );
    file.set_size((2 * PAGE_SIZE) as u64).unwrap();
    let regrown = file.request_page(1).unwrap();
    assert!(
        !Arc::ptr_eq(&beyond, &regrown),
        "regrowth issues a new request"
    );
    file.fail_range(PAGE_SIZE as u64, PAGE_SIZE as u64).unwrap();
    assert_eq!(
        file.wait_for_page(&beyond),
        Ok(()),
        "a later failure cannot rewrite it"
    );
    assert_eq!(file.wait_for_page(&regrown), Err(VmError::Io));
}

#[test]
fn detaching_the_pager_fails_outstanding_and_later_requests() {
    use core::sync::atomic::{AtomicUsize, Ordering};
    struct Count(AtomicUsize);
    impl PagerSink for Count {
        fn request(&self, _: PagerRequest) -> Result<(), ()> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
    }
    let sink = Arc::new(Count(AtomicUsize::new(0)));
    let file = Vmo::create_paged(
        (2 * PAGE_SIZE) as u64,
        VmoOptions::default(),
        PagerBinding {
            sink: sink.clone(),
            key: 1,
        },
    )
    .unwrap();
    let outstanding = file.request_page(0).unwrap();
    file.detach_pager().unwrap();
    assert_eq!(file.wait_for_page(&outstanding), Err(VmError::Io));
    assert!(file.state.lock().requested.is_empty());
    // 内容 owner 已退休：仍存活的对象不再把请求投给它，缺页直接失败。
    assert_eq!(file.read(PAGE_SIZE as u64, &mut [0]), Err(VmError::Io));
    assert_eq!(sink.0.load(Ordering::Relaxed), 1, "no request after detach");
}

#[test]
fn resident_observation_keeps_sparse_cow_and_slice_backing_without_faulting() {
    let parent = Vmo::create((PAGE_SIZE * 8) as u64, VmoOptions::default()).unwrap();
    parent.write(PAGE_SIZE as u64, &[17]).unwrap();
    parent.write((PAGE_SIZE * 6) as u64, &[23]).unwrap();
    let child = parent
        .create_child(0, parent.size(), ChildKind::Snapshot)
        .unwrap();
    assert_eq!(child.resident_pages(0, 8).unwrap(), 2);
    assert_eq!(child.committed_bytes().unwrap(), 0);
    child.write(PAGE_SIZE as u64, &[31]).unwrap();
    assert_eq!(child.resident_pages(0, 8).unwrap(), 2);
    assert_eq!(child.resident_pages(2, 4).unwrap(), 0);
    let slice = child
        .create_child(PAGE_SIZE as u64, (PAGE_SIZE * 6) as u64, ChildKind::Slice)
        .unwrap();
    assert_eq!(slice.resident_pages(0, 6).unwrap(), 2);
    assert_eq!(child.committed_bytes().unwrap(), PAGE_SIZE as u64);
    assert_eq!(byte(&parent, PAGE_SIZE as u64), 17);
}

#[test]
fn eviction_takes_only_clean_pages_and_waits_for_writeback_end() {
    struct NoRequests;
    impl PagerSink for NoRequests {
        fn request(&self, _: PagerRequest) -> Result<(), ()> {
            Ok(())
        }
    }
    let page = PAGE_SIZE as u64;
    let file = Vmo::create_paged(
        3 * page,
        VmoOptions::default(),
        PagerBinding {
            sink: Arc::new(NoRequests),
            key: 1,
        },
    )
    .unwrap();
    let source = Vmo::create(3 * page, VmoOptions::default()).unwrap();
    source.op_range(RangeOp::Commit, 0, 3 * page).unwrap();
    file.supply_pages(0, 3 * page, &source, 0).unwrap();
    // 页 1 被写脏；页 2 写脏后开始回写但存储尚未确认。
    file.write(page, &[2]).unwrap();
    file.write(2 * page, &[3]).unwrap();
    file.begin_writeback(2 * page, page).unwrap();

    assert_eq!(file.evict_clean(8), 1, "only the clean page 0 is evictable");
    assert_eq!(file.committed_bytes().unwrap(), 2 * page);

    file.end_writeback(2 * page, page).unwrap();
    assert_eq!(
        file.evict_clean(8),
        1,
        "page 2 becomes evictable after writeback end"
    );
    assert_eq!(file.committed_bytes().unwrap(), page);
    assert_eq!(
        file.first_dirty_range(0, 3 * page).unwrap(),
        Some((page, page)),
        "the dirty page stays resident and dirty"
    );
}
