pub mod pcb;
pub mod scheduler;
use crate::task::context::TaskContext;
use crate::task::*;

pub fn init() {
    println!("[process::init] called");
    println!("Process module initialized");
}

pub fn run_first_process() {
    println!("[process::run_first_process] called");
    crate::task::task_manager::init();

    let task_manager = crate::task::task_manager::TASK_MANAGER.wait();
    let task_cx_ptr = {
        let task_manager_guard = task_manager.lock();
        task_manager_guard.get_first_task_cx_ptr()
    }; // 锁在这里被释放

    // 为第一次任务切换创建一个临时的任务上下文
    let mut dummy_cx = TaskContext::zero_init();
    let dummy_cx_ptr = &mut dummy_cx as *mut _;

    unsafe {
        crate::task::__switch(dummy_cx_ptr, task_cx_ptr);
    }
    panic!("run_first_process should never return");
}

pub fn add_process() {
    println!("[process::add_process] called");
}
