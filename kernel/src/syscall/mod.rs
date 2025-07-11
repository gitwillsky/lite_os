mod fs;
mod process;
mod timer;

use fs::*;
use process::*;

const SYSCALL_READ: usize = 63;
const SYSCALL_WRITE: usize = 64;
const SYSCALL_EXIT: usize = 93;
const SYSCALL_YIELD: usize = 124;
const SYSCALL_FORK: usize = 220;
const SYSCALL_EXEC: usize = 221;
const SYSCALL_WAIT: usize = 260;
const SYSCALL_SHUTDOWN: usize = 110;

pub fn syscall(syscall_id: usize, args: [usize; 3]) -> isize {
    match syscall_id {
        SYSCALL_READ => sys_read(args[0], args[1] as *mut u8, args[2]),
        SYSCALL_WRITE => sys_write(args[0], args[1] as *const u8, args[2]),
        SYSCALL_EXIT => sys_exit(args[0] as i32),
        SYSCALL_YIELD => sys_yield(),
        SYSCALL_FORK => sys_fork(),
        SYSCALL_EXEC => sys_exec(args[0] as *const u8),
        SYSCALL_WAIT => sys_wait_pid(args[0] as isize, args[1] as *mut i32),
        SYSCALL_SHUTDOWN => sys_shutdown(),
        _ => {
            println!("syscall: invalid syscall_id: {}", syscall_id);
            -1
        }
    }
}
