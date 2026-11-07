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

    let resident = Vmo::create((1024 * PAGE_SIZE) as u64, VmoOptions::default()).unwrap();
    resident
        .write(0, &vec![37; resident.size() as usize])
        .unwrap();
    for written_pages in [0, 1] {
        let started = std::time::Instant::now();
        for _ in 0..256 {
            let child = resident
                .create_child(0, resident.size(), ChildKind::Snapshot)
                .unwrap();
            if written_pages == 1 {
                resident.write(0, &[91]).unwrap();
            }
            drop(child);
            assert_eq!(byte(&resident, resident.size() - 1), 37);
        }
        println!(
            "snapshot/retirement with 1024 resident pages, {written_pages} private pages: {:.2} us/op",
            started.elapsed().as_secs_f64() * 1_000_000.0 / 256.0
        );
    }
}

#[test]
fn fork_exit_churn_releases_hidden_ancestors() {
    let parent = Vmo::create((4 * PAGE_SIZE) as u64, VmoOptions::default()).unwrap();
    for index in 0..3 {
        parent.write((index * PAGE_SIZE) as u64, &[37]).unwrap();
    }
    for _ in 0..64 {
        let original = byte(&parent, 0);
        let changed = original ^ 1;
        let child = parent
            .create_child(0, parent.size(), ChildKind::Snapshot)
            .unwrap();
        let hidden = match &parent.state.lock().relation {
            Some(Relation::Cow { parent, .. }) => Arc::downgrade(parent),
            _ => panic!("snapshot must freeze the original contents"),
        };
        child.write(PAGE_SIZE as u64, &[91]).unwrap();
        parent.write(0, &[changed]).unwrap();
        parent.write((3 * PAGE_SIZE) as u64, &[17]).unwrap();
        assert_eq!(byte(&child, 0), original);
        drop(child);
        assert_eq!(byte(&parent, 0), changed);
        assert_eq!(byte(&parent, PAGE_SIZE as u64), 37);
        assert_eq!(byte(&parent, (3 * PAGE_SIZE) as u64), 17);
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
        false,
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
fn fault_around_collects_only_resident_file_pages_read_only_without_requests() {
    struct NoRequests;
    impl PagerSink for NoRequests {
        fn request(&self, _: PagerRequest) -> Result<(), ()> {
            panic!("fault-around must not issue a pager request")
        }
    }
    let file = Vmo::create_paged(
        (4 * PAGE_SIZE) as u64,
        VmoOptions::default(),
        PagerBinding {
            sink: Arc::new(NoRequests),
            key: 1,
        },
        false,
    )
    .unwrap();
    let supply = Vmo::create((4 * PAGE_SIZE) as u64, VmoOptions::default()).unwrap();
    for page in [0, 2] {
        supply.write((page * PAGE_SIZE) as u64, &[7]).unwrap();
        file.supply_pages(
            (page * PAGE_SIZE) as u64,
            PAGE_SIZE as u64,
            &supply,
            (page * PAGE_SIZE) as u64,
        )
        .unwrap();
    }
    let around = |vmo: &Arc<Vmo>, index: usize| {
        let mut collected = Vec::new();
        vmo.with_resolved_around(
            index,
            false,
            Some(0..4),
            |_| Ok(()),
            |(), pages| {
                collected = pages
                    .iter()
                    .flatten()
                    .map(|(index, page)| (*index, page.writable))
                    .collect::<Vec<_>>();
            },
        )
        .unwrap();
        collected
    };
    // 主缺页的页 0 被跳过；只有驻留页 2 被收集且只读；页 1、3 缺失，既不分配也不请求。
    assert_eq!(around(&file, 0), vec![(2, false)]);
    assert!(file.state.lock().requested.is_empty());

    // 私有快照读到祖先的文件页，同样只读；自身写时复制的副本不是文件页，不收集。
    let snapshot = file
        .create_child(0, file.size(), ChildKind::AtLeastOnWrite)
        .unwrap();
    snapshot.write(0, &[9]).unwrap();
    assert_eq!(around(&snapshot, 2), Vec::new(), "页 0 是私有副本，页 2 是主页");
    assert_eq!(around(&snapshot, 0), vec![(2, false)]);

    // 不设窗口时不收集任何页（写缺页的路径）。
    let mut called = false;
    file.with_resolved_around(0, false, None, |_| Ok(()), |(), _| called = true)
        .unwrap();
    assert!(!called);

    // 匿名对象的已提交页不属于 pager 补页成本，映射为只读只会多一次写缺页。
    let anonymous = Vmo::create((2 * PAGE_SIZE) as u64, VmoOptions::default()).unwrap();
    anonymous.write(0, &[1]).unwrap();
    anonymous.write(PAGE_SIZE as u64, &[1]).unwrap();
    let mut collected = usize::MAX;
    anonymous
        .with_resolved_around(
            0,
            false,
            Some(0..2),
            |_| Ok(()),
            |(), pages| collected = pages.iter().flatten().count(),
        )
        .unwrap();
    assert_eq!(collected, 0);
}

#[test]
fn reading_an_uncommitted_root_anonymous_page_maps_the_shared_zero_page_until_it_is_written() {
    super::zero_page::initialize();
    let object = Vmo::create((3 * PAGE_SIZE) as u64, VmoOptions::default()).unwrap();
    let read = |vmo: &Arc<Vmo>, index: usize| {
        let _tree = vmo.lock_tree().unwrap();
        vmo.resolve_locked(index, false).ok().unwrap()
    };
    let first = read(&object, 0);
    let second = read(&object, 1);
    assert!(!first.writable, "the zero page is never mapped writable");
    assert_eq!(first.ppn.as_usize(), second.ppn.as_usize(), "one global zero page");
    assert_eq!(object.committed_bytes().unwrap(), 0, "a read commits nothing");

    // 写入提交真实页；读到的不再是零页，其余下标仍共享零页。
    object.write(0, &[5]).unwrap();
    assert_eq!(object.committed_bytes().unwrap(), PAGE_SIZE as u64);
    let after = read(&object, 0);
    assert_ne!(after.ppn.as_usize(), first.ppn.as_usize());
    assert_eq!(read(&object, 2).ppn.as_usize(), first.ppn.as_usize());
    let mut byte = [0xff];
    object.read(PAGE_SIZE as u64, &mut byte).unwrap();
    assert_eq!(byte, [0], "reading through the zero page observes zeros");

    // 有祖先的对象不使用零页：祖先之后的写入必须对读者可见。
    let child = object
        .create_child(0, object.size(), ChildKind::AtLeastOnWrite)
        .unwrap();
    assert_ne!(read(&child, 2).ppn.as_usize(), first.ppn.as_usize());
    // 写解析永远得到真实页。
    let _tree = object.lock_tree().unwrap();
    let written = object.resolve_locked(1, true).ok().unwrap();
    assert!(written.writable);
    assert_ne!(written.ppn.as_usize(), first.ppn.as_usize());
}

fn page(tag: u8) -> Page {
    let frame = frame_allocator::alloc().unwrap();
    // SAFETY: 新页独占，指针指向其首字节。
    unsafe { *frame.ppn.as_page_mut_ptr() = tag };
    Page::new(frame, PageContent::Anonymous)
}

fn tag(page: &Page) -> u8 {
    // SAFETY: 页由列表独占持有，指针指向其首字节。
    unsafe { *page.frame.ppn.as_page_mut_ptr() }
}

#[test]
fn page_list_keeps_order_counts_and_frees_empty_chunks() {
    use super::page_list::{CHUNK_PAGES, PageList};
    let mut list = PageList::new();
    // 稀疏（跨块）与稠密（同块）混合，乱序插入。
    let indexes = [0, 3, 15, 16, 17, 100, 7 * CHUNK_PAGES + 2];
    for &index in indexes.iter().rev() {
        assert!(list.try_insert(index, page(index as u8)).unwrap().is_none());
    }
    assert_eq!(list.len(), indexes.len());
    assert_eq!(
        list.iter_from(&0).map(|(index, _)| index).collect::<Vec<_>>(),
        indexes
    );
    assert_eq!(
        list.iter_from(&16).map(|(index, _)| index).collect::<Vec<_>>(),
        indexes[3..]
    );
    assert_eq!(list.ceiling(&4).map(|(index, _)| index), Some(15));
    assert_eq!(list.ceiling(&101).map(|(index, _)| index), Some(7 * CHUNK_PAGES + 2));
    assert!(list.ceiling(&1000).is_none());
    assert_eq!(list.first_key_value().map(|(index, _)| index), Some(0));
    assert_eq!(tag(list.get(&17).unwrap()), 17);
    // 替换返回旧页且不改变计数。
    let old = list.try_insert(17, page(99)).unwrap().unwrap();
    assert_eq!((tag(&old), tag(list.get(&17).unwrap()), list.len()), (17, 99, indexes.len()));
    // 移除最后一页后块被释放：同块再插入需要重新分配（用 insert_reserved 会 panic，故走 try_insert）。
    assert_eq!(tag(&list.remove(&100).unwrap()), 100);
    assert!(list.remove(&100).is_none());
    assert!(!list.contains_key(&100));
    let mut seen = Vec::new();
    list.for_each_mut(|index, _| seen.push(index));
    assert_eq!(seen, [0, 3, 15, 16, 17, 7 * CHUNK_PAGES + 2]);
    for index in seen {
        drop(list.remove(&index));
    }
    assert!(list.is_empty() && list.first_key_value().is_none());
}

#[test]
fn page_list_reservation_makes_the_commit_phase_allocation_free() {
    use super::page_list::{CHUNK_PAGES, PageList};
    let mut source = PageList::new();
    for index in [20, 21, 40, 41] {
        source.try_insert(index, page(index as u8)).unwrap();
    }
    // 目标只为真实存在的页建块（平移 -20 后落在块 0 与块 1），而不是为整个窗口建块。
    let mut target = PageList::new();
    target.reserve_for_shifted(&source, 20, 1000).unwrap();
    while let Some((index, _)) = source.first_key_value() {
        let moved = source.remove(&index).unwrap();
        assert!(target.insert_reserved(index - 20, moved).is_none());
    }
    assert_eq!(
        target.iter_from(&0).map(|(index, _)| index).collect::<Vec<_>>(),
        [0, 1, 20, 21]
    );
    assert_eq!(target.get(&(CHUNK_PAGES + 4)).map(tag), Some(40));
    // 窗口之外的页不建块。
    let mut narrow = PageList::new();
    narrow.reserve_for_shifted(&target, 0, 2).unwrap();
    assert!(narrow.try_insert(0, page(1)).is_ok());
    assert!(std::panic::catch_unwind(core::panic::AssertUnwindSafe(|| {
        narrow.insert_reserved(5 * CHUNK_PAGES, page(2))
    }))
    .is_err());
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
        false,
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
        false,
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
        false,
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
        false,
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
        false,
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
        false,
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

    // slice 的 DMA 固定必须落到 backing owner；完成回写也不能把仍固定的页算作可淘汰。
    let slice = file.create_child(2 * page, page, ChildKind::Slice).unwrap();
    let pinned = slice.pin(0, page).unwrap();
    file.begin_writeback(2 * page, page).unwrap();
    file.end_writeback(2 * page, page).unwrap();
    assert_eq!(file.evict_clean(8), 0);
    assert_eq!(
        file.op_range(RangeOp::Decommit, 2 * page, page),
        Err(VmError::BadState)
    );
    drop(pinned);
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
