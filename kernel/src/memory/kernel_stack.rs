use super::{KERNEL_SPACE, KERNEL_STACK_SIZE, MapPermission, PAGE_SIZE, address::VirtualAddress};
use crate::id::IdAllocator;
use lazy_static::lazy_static;
use spin::Mutex;

#[derive(Debug)]
pub(crate) struct KernelStack {
    handle: KernelStackHandle,
}

impl KernelStack {
    pub(crate) fn new() -> Self {
        let handle = KernelStackHandle(KernelStackHandleAllocator.lock().alloc());
        let (bottom, top) = kernel_stack_position(handle.0);

        // 在栈底预留 1 页守护页，防止向下越界破坏相邻对象导致不可预期行为；
        // 溢出将立刻触发缺页异常，便于定位问题
        let mapped_bottom = bottom + PAGE_SIZE;

        KERNEL_SPACE
            .wait()
            .lock()
            .insert_framed_area(
                mapped_bottom.into(),
                top.into(),
                MapPermission::R | MapPermission::W,
            )
            .expect("Failed to allocate kernel stack memory");

        super::mm::MemorySet::flush_tlb_all_cpus()
            .expect("SBI RFENCE failed after kernel stack mapping");

        Self { handle }
    }

    pub(crate) fn get_top(&self) -> usize {
        let (_, top) = kernel_stack_position(self.handle.0);
        top
    }
}

impl Drop for KernelStack {
    fn drop(&mut self) {
        let (bottom, _) = kernel_stack_position(self.handle.0);
        let mapped_bottom = bottom + PAGE_SIZE;
        KERNEL_SPACE
            .wait()
            .lock()
            .remove_area_with_start_vpn(VirtualAddress::from(mapped_bottom).into());

        super::mm::MemorySet::flush_tlb_all_cpus()
            .expect("SBI RFENCE failed after kernel stack unmapping");
    }
}

/// 获取应用内核栈的地址范围，返回 (bottom, top)
fn kernel_stack_position(app_id: usize) -> (usize, usize) {
    // 内核栈位于单一 TrapContext 页之下，并在相邻栈之间保留一页守护间隔。
    let top = super::TRAP_CONTEXT - app_id * (KERNEL_STACK_SIZE + PAGE_SIZE);
    let bottom = top - KERNEL_STACK_SIZE;
    (bottom, top)
}

#[derive(Debug)]
struct KernelStackHandle(usize);

impl Drop for KernelStackHandle {
    fn drop(&mut self) {
        KernelStackHandleAllocator.lock().dealloc(self.0);
    }
}

lazy_static! {
    // OWNER: kernel-stack module exclusively allocates virtual stack handles.
    static ref KernelStackHandleAllocator: Mutex<IdAllocator> = Mutex::new(IdAllocator::new(1));
}
