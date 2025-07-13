use core::arch::asm;
use alloc::string::String;

// 系统调用ID定义
const SYSCALL_READ: usize = 63;
const SYSCALL_WRITE: usize = 64;
const SYSCALL_EXIT: usize = 93;
const SYSCALL_YIELD: usize = 124;
const SYSCALL_GETPID: usize = 172;
const SYSCALL_FORK: usize = 220;
const SYSCALL_EXEC: usize = 221;
const SYSCALL_EXECVE: usize = 222;
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
const SYSCALL_LSEEK: usize = 62;
const SYSCALL_PIPE: usize = 59;
const SYSCALL_DUP: usize = 23;
const SYSCALL_DUP2: usize = 24;
const SYSCALL_FLOCK: usize = 143;
const SYSCALL_MKFIFO: usize = 506;

// 信号相关系统调用
const SYSCALL_KILL: usize = 129;
const SYSCALL_SIGNAL: usize = 48;
const SYSCALL_SIGACTION: usize = 134;
const SYSCALL_SIGPROCMASK: usize = 135;
const SYSCALL_SIGRETURN: usize = 139;
const SYSCALL_PAUSE: usize = 34;
const SYSCALL_ALARM: usize = 37;

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

/// 功能：执行新程序，支持参数和环境变量传递
/// 参数：
/// - path: 程序路径
/// - argv: 参数数组
/// - envp: 环境变量数组
/// 返回值：如果执行成功则返回 0，如果执行失败则返回 -1
pub fn execve(path: &str, argv: &[&str], envp: &[&str]) -> isize {
    use alloc::vec::Vec;
    
    let mut null_terminated_path = String::from(path);
    null_terminated_path.push('\0');
    
    // Build null-terminated argument strings
    let mut arg_strings: Vec<String> = Vec::new();
    for arg in argv {
        let mut s = String::from(*arg);
        s.push('\0');
        arg_strings.push(s);
    }
    
    // Build null-terminated environment strings  
    let mut env_strings: Vec<String> = Vec::new();
    for env in envp {
        let mut s = String::from(*env);
        s.push('\0');
        env_strings.push(s);
    }
    
    // Build argv pointer array
    let mut argv_ptrs: Vec<*const u8> = Vec::new();
    for arg_str in &arg_strings {
        argv_ptrs.push(arg_str.as_ptr());
    }
    argv_ptrs.push(core::ptr::null()); // Null terminator
    
    // Build envp pointer array
    let mut envp_ptrs: Vec<*const u8> = Vec::new();
    for env_str in &env_strings {
        envp_ptrs.push(env_str.as_ptr());
    }
    envp_ptrs.push(core::ptr::null()); // Null terminator
    
    syscall(
        SYSCALL_EXECVE, 
        [
            null_terminated_path.as_ptr() as usize,
            argv_ptrs.as_ptr() as usize,
            envp_ptrs.as_ptr() as usize,
        ]
    )
}

/// 功能：当前进程主动让出 CPU 的执行权
/// 返回值：无
pub fn yield_() {
    syscall(SYSCALL_YIELD, [0, 0, 0]);
}

/// 功能：获取当前进程的PID
/// 返回值：当前进程的PID
pub fn getpid() -> isize {
    syscall(SYSCALL_GETPID, [0, 0, 0])
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

/// 创建命名管道（FIFO）
pub fn mkfifo(path: &str, mode: u32) -> isize {
    let mut null_terminated_path = String::from(path);
    null_terminated_path.push('\0');
    syscall(SYSCALL_MKFIFO, [null_terminated_path.as_ptr() as usize, mode as usize, 0])
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

/// 复制文件描述符
pub fn dup(fd: usize) -> isize {
    syscall(SYSCALL_DUP, [fd, 0, 0])
}

/// 复制文件描述符到指定的文件描述符号
pub fn dup2(oldfd: usize, newfd: usize) -> isize {
    syscall(SYSCALL_DUP2, [oldfd, newfd, 0])
}

/// 文件锁定
pub fn flock(fd: usize, operation: i32) -> isize {
    syscall(SYSCALL_FLOCK, [fd, operation as usize, 0])
}

/// 设置文件偏移量
pub fn lseek(fd: usize, offset: isize, whence: usize) -> isize {
    syscall(SYSCALL_LSEEK, [fd, offset as usize, whence])
}

/// 创建管道
pub fn pipe(pipefd: &mut [i32; 2]) -> isize {
    syscall(SYSCALL_PIPE, [pipefd.as_mut_ptr() as usize, 0, 0])
}

// 信号相关系统调用

/// 发送信号给进程
pub fn kill(pid: usize, sig: u32) -> isize {
    syscall(SYSCALL_KILL, [pid, sig as usize, 0])
}

/// 设置信号处理函数
pub fn signal(sig: u32, handler: usize) -> isize {
    syscall(SYSCALL_SIGNAL, [0, sig as usize, handler])
}

/// sigaction结构体
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct SigAction {
    pub sa_handler: usize,
    pub sa_mask: u64,
    pub sa_flags: u32,
    pub sa_restorer: usize,
}

/// 设置信号动作
pub fn sigaction(sig: u32, act: *const SigAction, oldact: *mut SigAction) -> isize {
    syscall(SYSCALL_SIGACTION, [sig as usize, act as usize, oldact as usize])
}

/// 设置信号掩码
pub fn sigprocmask(how: i32, set: *const u64, oldset: *mut u64) -> isize {
    syscall(SYSCALL_SIGPROCMASK, [how as usize, set as usize, oldset as usize])
}

/// 从信号处理函数返回
pub fn sigreturn() -> isize {
    syscall(SYSCALL_SIGRETURN, [0, 0, 0])
}

/// 暂停进程直到收到信号
pub fn pause() -> isize {
    syscall(SYSCALL_PAUSE, [0, 0, 0])
}

/// 设置定时器信号
pub fn alarm(seconds: u32) -> isize {
    syscall(SYSCALL_ALARM, [seconds as usize, 0, 0])
}

// 信号常量

/// 信号编号
pub mod signals {
    pub const SIGHUP: u32 = 1;
    pub const SIGINT: u32 = 2;
    pub const SIGQUIT: u32 = 3;
    pub const SIGILL: u32 = 4;
    pub const SIGTRAP: u32 = 5;
    pub const SIGABRT: u32 = 6;
    pub const SIGBUS: u32 = 7;
    pub const SIGFPE: u32 = 8;
    pub const SIGKILL: u32 = 9;
    pub const SIGUSR1: u32 = 10;
    pub const SIGSEGV: u32 = 11;
    pub const SIGUSR2: u32 = 12;
    pub const SIGPIPE: u32 = 13;
    pub const SIGALRM: u32 = 14;
    pub const SIGTERM: u32 = 15;
    pub const SIGSTKFLT: u32 = 16;
    pub const SIGCHLD: u32 = 17;
    pub const SIGCONT: u32 = 18;
    pub const SIGSTOP: u32 = 19;
    pub const SIGTSTP: u32 = 20;
    pub const SIGTTIN: u32 = 21;
    pub const SIGTTOU: u32 = 22;
    pub const SIGURG: u32 = 23;
    pub const SIGXCPU: u32 = 24;
    pub const SIGXFSZ: u32 = 25;
    pub const SIGVTALRM: u32 = 26;
    pub const SIGPROF: u32 = 27;
    pub const SIGWINCH: u32 = 28;
    pub const SIGIO: u32 = 29;
    pub const SIGPWR: u32 = 30;
    pub const SIGSYS: u32 = 31;
    
    /// sigaction标志常量
    pub const SA_NOCLDSTOP: u32 = 1;
    pub const SA_NOCLDWAIT: u32 = 2;
    pub const SA_SIGINFO: u32 = 4;
    pub const SA_RESTART: u32 = 0x10000000;
    pub const SA_NODEFER: u32 = 0x40000000;
    pub const SA_RESETHAND: u32 = 0x80000000;
    pub const SA_ONSTACK: u32 = 0x08000000;
}

/// 信号掩码操作常量
pub const SIG_BLOCK: i32 = 0;
pub const SIG_UNBLOCK: i32 = 1;
pub const SIG_SETMASK: i32 = 2;

/// 特殊信号处理器值
pub const SIG_DFL: usize = 0;  // 默认动作
pub const SIG_IGN: usize = 1;  // 忽略信号

/// 文件锁定操作常量
pub mod flock_consts {
    pub const LOCK_SH: i32 = 1;   // 共享锁
    pub const LOCK_EX: i32 = 2;   // 排他锁
    pub const LOCK_NB: i32 = 4;   // 非阻塞
    pub const LOCK_UN: i32 = 8;   // 解锁
}
