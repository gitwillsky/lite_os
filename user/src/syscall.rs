use core::arch::asm;
use syscall_abi::{
    SYSCALL_CLOSE, SYSCALL_EXIT_GROUP, SYSCALL_FSTAT, SYSCALL_FSYNC, SYSCALL_FTRUNCATE,
    SYSCALL_GETDENTS64, SYSCALL_LSEEK, SYSCALL_MKDIRAT, SYSCALL_OPENAT, SYSCALL_READ,
    SYSCALL_RENAMEAT2, SYSCALL_SCHED_YIELD, SYSCALL_UNLINKAT, SYSCALL_WRITE,
};

pub const AT_FDCWD: isize = -100;
pub const O_RDWR: usize = 2;
pub const O_CREAT: usize = 0x40;
pub const O_TRUNC: usize = 0x200;
pub const O_DIRECTORY: usize = 0x10000;
pub const AT_REMOVEDIR: usize = 0x200;

/// @description 按 Linux/riscv64 ABI 发起系统调用，参数依次装入 `a0..a5`，编号装入 `a7`。
///
/// @param id Linux/riscv64 系统调用编号。
/// @param args 六个系统调用参数；未使用的位置必须由调用方显式传入零。
/// @return kernel 通过 `a0` 返回的原始值；负值表示 `-errno`。
#[inline(always)]
pub fn syscall(id: usize, args: [usize; 6]) -> isize {
    let ret: isize;
    // SAFETY: register assignment follows the Linux RISC-V syscall ABI; ecall transfers to the
    // kernel and declares all explicit inputs/output without touching memory or the stack.
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

pub fn openat(path: &[u8], flags: usize, mode: usize) -> isize {
    openat_from(AT_FDCWD, path, flags, mode)
}
pub fn openat_from(dirfd: isize, path: &[u8], flags: usize, mode: usize) -> isize {
    syscall(
        SYSCALL_OPENAT,
        [dirfd as usize, path.as_ptr() as usize, flags, mode, 0, 0],
    )
}
pub fn read(fd: usize, buf: &mut [u8]) -> isize {
    syscall(
        SYSCALL_READ,
        [fd, buf.as_mut_ptr() as usize, buf.len(), 0, 0, 0],
    )
}
pub fn close(fd: usize) -> isize {
    syscall(SYSCALL_CLOSE, [fd, 0, 0, 0, 0, 0])
}
pub fn fsync(fd: usize) -> isize {
    syscall(SYSCALL_FSYNC, [fd, 0, 0, 0, 0, 0])
}
pub fn ftruncate(fd: usize, size: usize) -> isize {
    syscall(SYSCALL_FTRUNCATE, [fd, size, 0, 0, 0, 0])
}
pub fn fstat(fd: usize, stat: &mut [u8; 128]) -> isize {
    syscall(SYSCALL_FSTAT, [fd, stat.as_mut_ptr() as usize, 0, 0, 0, 0])
}
pub fn getdents64(fd: usize, buffer: &mut [u8]) -> isize {
    syscall(
        SYSCALL_GETDENTS64,
        [fd, buffer.as_mut_ptr() as usize, buffer.len(), 0, 0, 0],
    )
}
pub fn lseek(fd: usize, offset: isize, whence: usize) -> isize {
    syscall(SYSCALL_LSEEK, [fd, offset as usize, whence, 0, 0, 0])
}
pub fn renameat2(old: &[u8], new: &[u8]) -> isize {
    syscall(
        SYSCALL_RENAMEAT2,
        [
            AT_FDCWD as usize,
            old.as_ptr() as usize,
            AT_FDCWD as usize,
            new.as_ptr() as usize,
            0,
            0,
        ],
    )
}
pub fn unlinkat(path: &[u8]) -> isize {
    unlinkat_from(AT_FDCWD, path, 0)
}
pub fn unlinkat_from(dirfd: isize, path: &[u8], flags: usize) -> isize {
    syscall(
        SYSCALL_UNLINKAT,
        [dirfd as usize, path.as_ptr() as usize, flags, 0, 0, 0],
    )
}
pub fn mkdirat(path: &[u8], mode: usize) -> isize {
    syscall(
        SYSCALL_MKDIRAT,
        [AT_FDCWD as usize, path.as_ptr() as usize, mode, 0, 0, 0],
    )
}

/// @description 主动让出处理器，使用 Linux/riscv64 `sched_yield` 编号。
///
/// @return 成功返回零，失败返回负的 Linux errno。
pub fn sched_yield() -> isize {
    syscall(SYSCALL_SCHED_YIELD, [0, 0, 0, 0, 0, 0])
}
