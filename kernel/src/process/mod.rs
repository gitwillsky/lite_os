pub mod pcb;
pub mod scheduler;
use crate::task::*;

pub fn init() {
    println!("[process::init] called");
    println!("Process module initialized");
}

pub fn run_first_process() {
    println!("[process::run_first_process] called");
    crate::task::task_manager::init();
    let task_manager = crate::task::task_manager::TASK_MANAGER.wait();
    let task_manager = task_manager.lock();
    task_manager.run_first_task();
}

pub fn add_process() {
    println!("[process::add_process] called");
}
