use crate::id::IdAllocator;
use alloc::vec::Vec;
use lazy_static::lazy_static;
use spin::Mutex;

use crate::{
    memory::{
        KERNEL_SPACE, KERNEL_STACK_SIZE, PAGE_SIZE, TRAMPOLINE, address::VirtualAddress,
        mm::MapPermission,
    },
};

pub const INIT_PID: usize = 1;

#[derive(Debug)]
pub struct PidHandle(pub usize);

impl Drop for PidHandle {
    fn drop(&mut self) {
        PID_ALLOCATOR.lock().dealloc(self.0);
    }
}

impl PidHandle {
    /// 获取原始PID数值（不移动所有权）
    #[inline]
    pub fn raw(&self) -> usize { self.0 }
}

pub fn alloc_pid() -> PidHandle {
    PidHandle((PID_ALLOCATOR.lock().alloc()))
}

pub fn dealloc_pid(pid: PidHandle) {
    PID_ALLOCATOR.lock().dealloc(pid.0);
}

impl From<usize> for PidHandle {
    fn from(pid: usize) -> Self {
        PidHandle(pid)
    }
}

lazy_static! {
    // 0 IDLE 1 INIT PROC
    static ref PID_ALLOCATOR: Mutex<IdAllocator> = Mutex::new(IdAllocator::new(2));
}
