use core::arch::asm;

/// 系统调用
///
/// # Arguments
///
/// * `id` - 系统调用号
/// * `args` - 系统调用参数
///
/// # Returns
///
/// 系统调用返回值
fn syscall(id: usize, args: [usize; 3]) -> isize {
    let ret: isize;
    unsafe {
        asm!(
            "ecall",

            inlateout("x10") args[0] => ret,

            in("x11") args[1],
            in("x12") args[2],
            in("x17") id,
        );
    }
    ret
}

const SYSCALL_EXIT: usize = 93;

pub fn sys_exit(status: i32) -> isize {
    syscall(SYSCALL_EXIT, [status as usize, 0, 0])
}

const SYSCALL_WRITE: usize = 64;

pub fn sys_write(fd: usize, buf: &[u8]) -> isize {
    syscall(SYSCALL_WRITE, [fd, buf.as_ptr() as usize, buf.len()])
}
