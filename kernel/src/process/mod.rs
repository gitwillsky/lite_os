pub mod pcb;
pub mod scheduler;
use crate::task::*;

pub fn init() {
    println!("Process module initialized");
}

pub fn run_first_process() {
    crate::task::task_manager::init();
    let task_manager = crate::task::task_manager::TASK_MANAGER.wait();
    let task_manager = task_manager.lock();
    task_manager.run_first_task();
}
