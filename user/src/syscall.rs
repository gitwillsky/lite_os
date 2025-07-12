use core::arch::asm;
use alloc::string::String;

// 系统调用ID定义
const SYSCALL_READ: usize = 63;
const SYSCALL_WRITE: usize = 64;
const SYSCALL_EXIT: usize = 93;
const SYSCALL_YIELD: usize = 124;
const SYSCALL_FORK: usize = 220;
const SYSCALL_EXEC: usize = 221;
const SYSCALL_WAIT: usize = 260;
const SYSCALL_SHUTDOWN: usize = 110;

// 文件系统系统调用
const SYSCALL_OPEN: usize = 56;
const SYSCALL_CLOSE: usize = 57;
const SYSCALL_LISTDIR: usize = 500;
const SYSCALL_MKDIR: usize = 501;
const SYSCALL_REMOVE: usize = 502;
const SYSCALL_STAT: usize = 80;
const SYSCALL_READ_FILE: usize = 503;
const SYSCALL_CHDIR: usize = 504;
const SYSCALL_GETCWD: usize = 505;

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

pub fn exit(status: i32) -> isize {
    syscall(SYSCALL_EXIT, [status as usize, 0, 0])
}

pub fn write(fd: usize, buf: &[u8]) -> isize {
    syscall(SYSCALL_WRITE, [fd, buf.as_ptr() as usize, buf.len()])
}

pub fn read(fd: usize, buf: &mut [u8]) -> isize {
    syscall(SYSCALL_READ, [fd, buf.as_mut_ptr() as usize, buf.len()])
}

/// 创建一个子进程
/// 返回值：原进程返回新创建的子进程的 Pid，新创建的子进程返回 0
pub fn fork() -> isize {
    syscall(SYSCALL_FORK, [0, 0, 0])
}

/// 功能：执行一个程序
/// 参数：path 表示程序的路径
/// 返回值：如果执行成功则返回 0，如果执行失败则返回 -1
pub fn exec(path: &str) -> isize {
    let mut null_terminated_path = String::from(path);
    null_terminated_path.push('\0');
    syscall(SYSCALL_EXEC, [null_terminated_path.as_ptr() as usize, 0, 0])
}

/// 功能：当前进程主动让出 CPU 的执行权
/// 返回值：无
pub fn yield_() {
    syscall(SYSCALL_YIELD, [0, 0, 0]);
}

/// 功能：当前进程等待一个子进程变为僵尸进程，回收其全部资源并收集其返回值。
/// 参数：pid 表示要等待的子进程的进程 ID，如果为 -1 的话表示等待任意一个子进程；
/// exit_code 表示保存子进程返回值的地址，如果这个地址为 0 的话表示不必保存。
/// 返回值：如果要等待的子进程不存在则返回 -1；否则如果要等待的子进程均未结束则返回 -2；
/// 否则返回结束的子进程的进程 ID。
fn sys_wait(pid: isize, exit_code: *mut i32) -> isize {
    syscall(SYSCALL_WAIT, [pid as usize, exit_code as usize, 0])
}

/// 功能：关闭系统
/// 返回值：无
pub fn shutdown() -> isize {
    syscall(SYSCALL_SHUTDOWN, [0, 0, 0])
}

/// 等待任意一个子进程结束
pub fn wait(exit_code: *mut i32) -> isize {
    loop {
        match sys_wait(-1, exit_code) {
            -2 => {
                yield_();
            }
            exit_code => return exit_code,
        }
    }
}

/// 等待指定进程结束
pub fn wait_pid(pid: usize, exit_code: *mut i32) -> isize {
    loop {
        match sys_wait(pid as isize, exit_code) {
            -2 => {
                yield_();
            }
            exit_code => return exit_code,
        }
    }
}

// 文件系统系统调用封装

/// 打开文件
pub fn open(path: &str, flags: u32) -> isize {
    let mut null_terminated_path = String::from(path);
    null_terminated_path.push('\0');
    syscall(SYSCALL_OPEN, [null_terminated_path.as_ptr() as usize, flags as usize, 0])
}

/// 关闭文件
pub fn close(fd: usize) -> isize {
    syscall(SYSCALL_CLOSE, [fd, 0, 0])
}

/// 列出目录内容
pub fn listdir(path: &str, buf: &mut [u8]) -> isize {
    let mut null_terminated_path = String::from(path);
    null_terminated_path.push('\0');
    syscall(SYSCALL_LISTDIR, [null_terminated_path.as_ptr() as usize, buf.as_mut_ptr() as usize, buf.len()])
}

/// 创建目录
pub fn mkdir(path: &str) -> isize {
    let mut null_terminated_path = String::from(path);
    null_terminated_path.push('\0');
    syscall(SYSCALL_MKDIR, [null_terminated_path.as_ptr() as usize, 0, 0])
}

/// 删除文件或目录
pub fn remove(path: &str) -> isize {
    let mut null_terminated_path = String::from(path);
    null_terminated_path.push('\0');
    syscall(SYSCALL_REMOVE, [null_terminated_path.as_ptr() as usize, 0, 0])
}

/// 获取文件信息
pub fn stat(path: &str, buf: &mut [u8]) -> isize {
    let mut null_terminated_path = String::from(path);
    null_terminated_path.push('\0');
    syscall(SYSCALL_STAT, [null_terminated_path.as_ptr() as usize, buf.as_mut_ptr() as usize, 0])
}

/// 读取文件内容
pub fn read_file(path: &str, buf: &mut [u8]) -> isize {
    let mut null_terminated_path = String::from(path);
    null_terminated_path.push('\0');
    syscall(SYSCALL_READ_FILE, [null_terminated_path.as_ptr() as usize, buf.as_mut_ptr() as usize, buf.len()])
}

/// 改变当前工作目录
pub fn chdir(path: &str) -> isize {
    let mut null_terminated_path = String::from(path);
    null_terminated_path.push('\0');
    syscall(SYSCALL_CHDIR, [null_terminated_path.as_ptr() as usize, 0, 0])
}

/// 获取当前工作目录
pub fn getcwd(buf: &mut [u8]) -> isize {
    syscall(SYSCALL_GETCWD, [buf.as_mut_ptr() as usize, buf.len(), 0])
}
