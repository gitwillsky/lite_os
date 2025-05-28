mod fs;
mod timer;

use fs::*;
use timer::*;

const SYSCALL_WRITE: usize = 64;
const SYSCALL_GET_TIME_MSEC: usize = 169;

pub fn syscall(syscall_id: usize, args: [usize; 3]) -> isize {
    match syscall_id {
        SYSCALL_WRITE => sys_write(args[0], args[1] as *const u8, args[2]),
        SYSCALL_GET_TIME_MSEC => sys_get_time_msec(),
        _ => {
            println!("syscall: invalid syscall_id: {}", syscall_id);
            -1
        }
    }
}
