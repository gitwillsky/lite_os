use core::arch::global_asm;

use crate::{
    arch::sbi::shutdown,
    loader::get_app_data_by_name,
    task::{
        context::TaskContext,
        task::{TaskControlBlock, TaskStatus},
        task_manager::{get_init_proc, set_init_proc},
    },
};

mod context;
mod pid;
mod processor;
mod task;
mod task_manager;

use alloc::sync::Arc;
pub use processor::*;
pub use task_manager::add_task;

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
    set_init_proc(Arc::new(TaskControlBlock::new(
        get_app_data_by_name("initproc").unwrap(),
    )));
}

pub fn suspend_current_and_run_next() {
    let task = take_current_task().unwrap();

    let mut task_inner = task.inner_exclusive_access();
    let task_cx_ptr = &mut task_inner.task_cx as *mut _;
    task_inner.task_status = TaskStatus::Ready;
    drop(task_inner);

    // push back to ready queue
    add_task(task);

    // jump to schedule cycle
    schedule(task_cx_ptr);
}

pub const IDLE_PID: usize = 0;

pub fn exit_current_and_run_next(exit_code: i32) {
    let task = take_current_task().unwrap();

    let pid = task.get_pid();
    if pid == IDLE_PID {
        println!(
            "[kernel] Idle process exit with exit_code {} ...",
            exit_code
        );
        if exit_code != 0 {
            shutdown()
        } else {
            shutdown()
        }
    }

    let mut inner = task.inner_exclusive_access();

    inner.task_status = TaskStatus::Zombie;
    inner.exit_code = exit_code;

    {
        let init_proc = get_init_proc().unwrap();
        let mut init_proc_inner = init_proc.inner_exclusive_access();
        for child in inner.children.iter() {
            child.inner_exclusive_access().parent = Some(Arc::downgrade(&init_proc));
            init_proc_inner.children.push(child.clone());
        }
    }

    inner.children.clear();
    // deallocate user space
    inner.memory_set.recycle_data_pages();
    drop(inner);

    drop(task);

    let mut _unused = TaskContext::zero_init();
    schedule(&mut _unused as *mut _);
}
