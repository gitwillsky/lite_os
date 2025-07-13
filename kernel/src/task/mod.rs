use core::arch::global_asm;

use alloc::sync::Arc;

use crate::{
    loader::get_app_data_by_name,
    task::{context::TaskContext, task::TaskControlBlock, task_manager::set_init_proc},
};

mod context;
mod pid;
mod processor;
mod task;
mod task_manager;

pub use processor::*;
pub use task_manager::add_task;
pub use task::FileDescriptor;

global_asm!(include_str!("switch.S"));

unsafe extern "C" {
    /// Switch to the context of 'next_task_cx_ptr', saving the current context
    /// in `current_task_cx_ptr`
    pub unsafe fn __switch(
        current_task_cx_ptr: *mut TaskContext,
        next_task_cx_ptr: *const TaskContext,
    );
}

pub fn init() {
    let elf_data = get_app_data_by_name("initproc").expect("Failed to get init proc data");
    let init_proc = TaskControlBlock::new(elf_data.as_slice());
    match init_proc {
        Ok(tcb) => set_init_proc(Arc::new(tcb)),
        Err(e) => panic!("Failed to create init proc: {:?}", e),
    }
}
