// 生产 supervisor VMA placement、publication、retirement 与 fork；仅物理 allocator/fence 为 host seam。

#[test]
fn retired_context_slot_is_reused_without_pid_dependent_address_or_live_collision() {
    let mut memory = MemorySet::empty();
    let range = crate::arch::mmu::THREAD_TRAP_CONTEXT_RANGE.unwrap();
    let first = memory.allocate_thread_trap_context().unwrap();
    let second = memory.allocate_thread_trap_context().unwrap();
    assert_eq!(first, range.end - config::PAGE_SIZE);
    assert_eq!(second, first - config::PAGE_SIZE);
    for _ in 0..128 {
        memory.remove_thread_trap_context(first);
        assert!(memory.page_table.translate(VirtualAddress::from(first).floor()).is_none());
        assert!(memory.page_table.translate(VirtualAddress::from(second).floor()).is_some());
        assert_eq!(memory.allocate_thread_trap_context().unwrap(), first);
        assert_eq!(memory.areas.len(), 2);
    }
}

#[test]
fn allocation_failure_preserves_hint_and_partial_translation_rollback() {
    let mut memory = MemorySet::empty();
    let first = memory.allocate_thread_trap_context().unwrap();
    let hint = memory.thread_context_hint;
    crate::memory::frame_allocator::FAIL_NEXT_FRAME.with(|flag| flag.set(true));
    assert!(matches!(memory.allocate_thread_trap_context(), Err(MemoryError::OutOfMemory)));
    assert_eq!(memory.thread_context_hint, hint);
    assert_eq!(memory.areas.len(), 1);
    assert!(memory.page_table.translate(VirtualAddress::from(first).floor()).is_some());
    assert!(memory.page_table.translate(VirtualAddress::from(first - config::PAGE_SIZE).floor()).is_none());
    assert_eq!(memory.allocate_thread_trap_context().unwrap(), first - config::PAGE_SIZE);
}

#[test]
fn gap_search_skips_whole_occupied_vma_and_fork_does_not_clone_other_thread_contexts() {
    let mut parent = MemorySet::empty();
    let range = crate::arch::mmu::THREAD_TRAP_CONTEXT_RANGE.unwrap();
    parent.insert_framed_area((range.end - 3 * config::PAGE_SIZE).into(), range.end.into(), MapPermission::R | MapPermission::W).unwrap();
    let address = parent.allocate_thread_trap_context().unwrap();
    assert_eq!(address, range.end - 4 * config::PAGE_SIZE);
    // canonical trap page is outside the temporary-context range and must survive fork.
    let canonical = range.end + config::PAGE_SIZE;
    parent.insert_framed_area(canonical.into(), (canonical + config::PAGE_SIZE).into(), MapPermission::R | MapPermission::W).unwrap();
    let mut child = MemorySet::empty();
    parent.try_clone_for_fork(&mut child).unwrap();
    assert!(child.page_table.translate(VirtualAddress::from(address).floor()).is_none());
    assert!(child.page_table.translate(VirtualAddress::from(canonical).floor()).is_some());
    assert_eq!(child.allocate_thread_trap_context().unwrap(), range.end - config::PAGE_SIZE);
}
