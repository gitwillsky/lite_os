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

        KERNEL_SPACE
            .wait()
            .lock()
            .insert_framed_area(bottom.into(), top.into(), MapPermission::R | MapPermission::W)
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
        KERNEL_SPACE
            .wait()
            .lock()
            .remove_area_with_start_vpn(VirtualAddress::from(bottom).into());

        // 本地刷新TLB，避免保留对已移除栈页的陈旧映射
        unsafe { core::arch::asm!("sfence.vma") }
    }
}

/// 获取应用内核栈的地址范围，返回 (bottom, top)
fn kernel_stack_position(app_id: usize) -> (usize, usize) {
    let top = super::TRAMPOLINE - app_id * (KERNEL_STACK_SIZE + PAGE_SIZE);

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
