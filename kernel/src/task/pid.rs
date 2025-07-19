use alloc::vec::Vec;
use lazy_static::lazy_static;

use crate::{
    memory::{
        KERNEL_SPACE, KERNEL_STACK_SIZE, PAGE_SIZE, TRAMPOLINE, address::VirtualAddress,
        mm::MapPermission,
    },
    sync::UPSafeCell,
};

pub fn alloc_pid() -> PidHandle {
    PID_ALLOCATOR.exclusive_access().alloc()
}

pub fn dealloc_pid(pid: PidHandle) {
    PID_ALLOCATOR.exclusive_access().dealloc(pid.0);
}

lazy_static! {
    static ref PID_ALLOCATOR: UPSafeCell<PidAllocator> = UPSafeCell::new(PidAllocator::new());
}

#[derive(Debug)]
pub struct PidHandle(pub usize);

impl Drop for PidHandle {
    fn drop(&mut self) {
        PID_ALLOCATOR.exclusive_access().dealloc(self.0);
    }
}

struct PidAllocator {
    current: usize,
    recycled: Vec<usize>,
}

impl PidAllocator {
    pub fn new() -> Self {
        Self {
            current: 1, // 从 PID 1 开始分配，保留 PID 0 给 idle 进程
            recycled: Vec::new(),
        }
    }

    pub fn alloc(&mut self) -> PidHandle {
        if let Some(pid) = self.recycled.pop() {
            PidHandle(pid)
        } else {
            let pid = self.current;
            self.current += 1;
            PidHandle(pid)
        }
    }

    pub fn dealloc(&mut self, pid: usize) {
        assert!(pid < self.current);
        assert!(
            !self.recycled.contains(&pid),
            "pid {} is already deallocated",
            pid
        );
        self.recycled.push(pid);
    }
}

#[derive(Debug)]
pub struct KernelStack {
    pid: usize,
}

impl KernelStack {
    pub fn new(pid: usize) -> Self {
        let (bottom, top) = kernel_stack_position(pid);

        KERNEL_SPACE.wait().lock().insert_framed_area(
            bottom.into(),
            top.into(),
            MapPermission::R | MapPermission::W,
        );

        Self { pid }
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
        let (_, top) = kernel_stack_position(self.pid);
        top
    }
}

impl Drop for KernelStack {
    fn drop(&mut self) {
        let (bottom, _) = kernel_stack_position(self.pid);
        KERNEL_SPACE
            .wait()
            .lock()
            .remove_area_with_start_vpn(VirtualAddress::from(bottom).into());
    }
}

/// 获取应用内核栈的地址范围，返回 (bottom, top)
fn kernel_stack_position(app_id: usize) -> (usize, usize) {
    let top = TRAMPOLINE - app_id * (KERNEL_STACK_SIZE + PAGE_SIZE);

    let bottom = top - KERNEL_STACK_SIZE;
    (bottom, top)
}
