mod fs;
mod timer;

use fs::*;
use timer::*;

const SYSCALL_WRITE: usize = 64;
const SYSCALL_GET_TIME_MSEC: usize = 169;
const SYSCALL_READ: usize = 63;

pub fn syscall(syscall_id: usize, args: [usize; 3]) -> isize {
    println!("[syscall] id={}, args=[{:#x}, {:#x}, {:#x}]", syscall_id, args[0], args[1], args[2]);
    match syscall_id {
        SYSCALL_WRITE => sys_write(args[0], args[1] as *const u8, args[2]),
        SYSCALL_GET_TIME_MSEC => sys_get_time_msec(),
        SYSCALL_READ => sys_read(args[0], args[1] as *mut u8, args[2]),
        _ => {
            println!("syscall: invalid syscall_id: {}", syscall_id);
            -1
        }
    }
}
