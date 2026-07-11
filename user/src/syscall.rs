use core::arch::asm;
use syscall_abi::{SYSCALL_EXIT_GROUP, SYSCALL_SCHED_YIELD, SYSCALL_WRITE};

/// @description 按 Linux/riscv64 ABI 发起系统调用，参数依次装入 `a0..a5`，编号装入 `a7`。
///
/// @param id Linux/riscv64 系统调用编号。
/// @param args 六个系统调用参数；未使用的位置必须由调用方显式传入零。
/// @return kernel 通过 `a0` 返回的原始值；负值表示 `-errno`。
#[inline(always)]
pub fn syscall(id: usize, args: [usize; 6]) -> isize {
    let ret: isize;
    unsafe {
        asm!(
            "ecall",
            inlateout("x10") args[0] => ret,
            in("x11") args[1],
            in("x12") args[2],
            in("x13") args[3],
            in("x14") args[4],
            in("x15") args[5],
            in("x17") id,
            options(nostack),
        );
    }
    ret
}

/// @description 终止当前 thread group。
///
/// @param status 传递给 kernel 的退出状态。
/// @return 此函数不返回；若 kernel 错误返回，则停留在本地死循环。
pub fn exit_group(status: i32) -> ! {
    let _ = syscall(SYSCALL_EXIT_GROUP, [status as usize, 0, 0, 0, 0, 0]);
    loop {
        core::hint::spin_loop();
    }
}

/// @description 向文件描述符写入字节。
///
/// @param fd 目标文件描述符。
/// @param buf 待写入的字节切片。
/// @return 成功写入的字节数，或负的 Linux errno。
pub fn write(fd: usize, buf: &[u8]) -> isize {
    syscall(
        SYSCALL_WRITE,
        [fd, buf.as_ptr() as usize, buf.len(), 0, 0, 0],
    )
}

/// @description 主动让出处理器，使用 Linux/riscv64 `sched_yield` 编号。
///
/// @return 成功返回零，失败返回负的 Linux errno。
pub fn sched_yield() -> isize {
    syscall(SYSCALL_SCHED_YIELD, [0, 0, 0, 0, 0, 0])
}
