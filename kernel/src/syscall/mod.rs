mod fs;
mod timer;

use fs::*;
use timer::*;
use crate::task::task_manager::TASK_MANAGER;

const SYSCALL_WRITE: usize = 64;
const SYSCALL_GET_TIME_MSEC: usize = 169;
const SYSCALL_READ: usize = 63;
const SYSCALL_EXIT: usize = 93;

pub fn syscall(syscall_id: usize, args: [usize; 3]) -> isize {
    println!("[syscall] id={}, args=[{:#x}, {:#x}, {:#x}]", syscall_id, args[0], args[1], args[2]);
    match syscall_id {
        SYSCALL_WRITE => sys_write(args[0], args[1] as *const u8, args[2]),
        SYSCALL_GET_TIME_MSEC => sys_get_time_msec(),
        SYSCALL_READ => sys_read(args[0], args[1] as *mut u8, args[2]),
        SYSCALL_EXIT => sys_exit(args[0] as i32),
        _ => {
            println!("syscall: invalid syscall_id: {}", syscall_id);
            -1
        }
    }
}

pub fn sys_exit(exit_code: i32) -> isize {
    println!("[sys_exit] Task exiting with code: {}", exit_code);
    let guard = TASK_MANAGER.wait().lock();
    (&*guard).exit_current_and_run_next();
    unreachable!("sys_exit should not return")
}
