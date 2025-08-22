use super::{KERNEL_SPACE, KERNEL_STACK_SIZE, MapPermission, PAGE_SIZE, address::VirtualAddress};
use crate::id::IdAllocator;
use lazy_static::lazy_static;
use spin::Mutex;

#[derive(Debug)]
pub struct KernelStack {
    handle: KernelStackHandle,
}

impl KernelStack {
    pub fn new() -> Self {
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

        // 本地刷新TLB，确保新映射立即可见
        unsafe { core::arch::asm!("sfence.vma") }

        Self { handle }
    }

    pub fn push_on_top<T>(&self, data: T) -> *mut T
    where
        T: Sized,
    {
        let kernel_stack_top = self.get_top();
        let ptr_mut = (kernel_stack_top - core::mem::size_of::<T>()) as *mut T;
        unsafe {
            *ptr_mut = data;
        }
        ptr_mut
    }

    pub fn get_top(&self) -> usize {
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

        // 本地刷新TLB，避免保留对已移除栈页的陈旧映射
        unsafe { core::arch::asm!("sfence.vma") }
    }
}

/// 获取应用内核栈的地址范围，返回 (bottom, top)
fn kernel_stack_position(app_id: usize) -> (usize, usize) {
    // 关键修复：内核栈必须位于 TrapContext 预留窗口（[TRAP_CONTEXT_BASE, TRAMPOLINE)）之下，
    // 之前以 TRAMPOLINE 为锚点导致在 128KB 栈尺寸时与 64*PAGE 的 TrapContext 窗口产生重叠（底部跨过 BASE 1 页）。
    // 现在改为以 TRAP_CONTEXT_BASE 为锚点，向低地址递减分配，并保留 1 页守护页间隔。
    let top = super::TRAP_CONTEXT_BASE - app_id * (KERNEL_STACK_SIZE + PAGE_SIZE);
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
    static ref KernelStackHandleAllocator: Mutex<IdAllocator> = Mutex::new(IdAllocator::new(1));
}
