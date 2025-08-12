use core::arch::asm;
use alloc::string::String;
use alloc::vec::Vec;

/// CPU核心信息结构体
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct CpuCoreInfo {
    pub total_cores: u32,     // 总核心数
    pub active_cores: u32,    // 活跃核心数
}

/// 进程信息结构体（与内核中的定义保持一致）
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct ProcessInfo {
    pub pid: u32,
    pub ppid: u32,
    pub uid: u32,
    pub gid: u32,
    pub euid: u32,
    pub egid: u32,
    pub status: u32,     // 0=Ready, 1=Running, 2=Zombie, 3=Sleeping
    pub priority: i32,
    pub nice: i32,
    pub vruntime: u64,
    pub heap_base: usize,
    pub heap_top: usize,
    pub last_runtime: u64,
    pub total_cpu_time: u64,  // 总CPU时间（微秒）
    pub cpu_percent: u32,     // CPU使用率百分比（0-10000，支持两位小数）
    pub core_id: u32,         // 进程运行的核心ID
    pub name: [u8; 32],       // 进程名（固定长度，以0结尾）
}

/// 系统统计信息结构体（与内核中的定义保持一致）
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct SystemStats {
    pub total_processes: u32,
    pub running_processes: u32,
    pub sleeping_processes: u32,
    pub zombie_processes: u32,
    pub total_memory: usize,
    pub used_memory: usize,
    pub free_memory: usize,
    pub system_uptime: u64,      // 系统运行时间（微秒）
    pub cpu_user_time: u64,      // 用户态CPU时间（微秒）
    pub cpu_system_time: u64,    // 系统态CPU时间（微秒）
    pub cpu_idle_time: u64,      // 空闲CPU时间（微秒）
    pub cpu_usage_percent: u32,  // 总CPU使用率百分比（0-10000）
}

// 系统调用ID定义
const SYSCALL_READ: usize = 63;
const SYSCALL_WRITE: usize = 64;
const SYSCALL_EXIT: usize = 93;
const SYSCALL_YIELD: usize = 124;
const SYSCALL_GETPID: usize = 172;
const SYSCALL_GETTID: usize = 178;
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
const SYSCALL_CHMOD: usize = 507;
const SYSCALL_CHOWN: usize = 508;
const SYSCALL_GET_ARGS: usize = 509;
const SYSCALL_FCNTL: usize = 25;

// 权限相关系统调用
const SYSCALL_GETUID: usize = 102;
const SYSCALL_GETGID: usize = 104;
const SYSCALL_SETUID: usize = 146;
const SYSCALL_SETGID: usize = 147;
const SYSCALL_GETEUID: usize = 107;
const SYSCALL_GETEGID: usize = 108;
const SYSCALL_SETEUID: usize = 148;
const SYSCALL_SETEGID: usize = 149;

// 内存管理系统调用
const SYSCALL_BRK: usize = 214;
const SYSCALL_SBRK: usize = 215;
const SYSCALL_MMAP: usize = 223;
const SYSCALL_MUNMAP: usize = 216;
// 共享内存
const SYSCALL_SHM_CREATE: usize = 2300;
const SYSCALL_SHM_MAP: usize = 2301;
const SYSCALL_SHM_CLOSE: usize = 2302;

// 信号相关系统调用
const SYSCALL_KILL: usize = 129;
const SYSCALL_SIGNAL: usize = 48;
const SYSCALL_SIGACTION: usize = 134;
const SYSCALL_SIGPROCMASK: usize = 135;
const SYSCALL_SIGRETURN: usize = 139;
const SYSCALL_PAUSE: usize = 34;
const SYSCALL_ALARM: usize = 37;

// 进程监控系统调用
const SYSCALL_GET_PROCESS_LIST: usize = 700;
const SYSCALL_GET_PROCESS_INFO: usize = 701;
const SYSCALL_GET_CPU_CORE_INFO: usize = 703;
const SYSCALL_GET_SYSTEM_STATS: usize = 702;

// 时间相关系统调用
const SYSCALL_GET_TIME_MS: usize = 800;
const SYSCALL_GET_TIME_US: usize = 801;
const SYSCALL_GET_TIME_NS: usize = 802;
const SYSCALL_TIME: usize = 803;
const SYSCALL_GETTIMEOFDAY: usize = 804;
const SYSCALL_NANOSLEEP: usize = 101;

// Watchdog 相关系统调用
const SYSCALL_WATCHDOG_CONFIGURE: usize = 900;
const SYSCALL_WATCHDOG_START: usize = 901;
const SYSCALL_WATCHDOG_STOP: usize = 902;
const SYSCALL_WATCHDOG_FEED: usize = 903;
const SYSCALL_WATCHDOG_GET_INFO: usize = 904;
const SYSCALL_WATCHDOG_SET_PRESET: usize = 905;

// 线程相关系统调用
const SYSCALL_THREAD_CREATE: usize = 1000;
const SYSCALL_THREAD_EXIT: usize = 1001;
const SYSCALL_THREAD_JOIN: usize = 1002;

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
pub fn syscall(id: usize, args: [usize; 3]) -> isize {
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

pub fn gettid() -> isize {
    syscall(SYSCALL_GETTID, [0, 0, 0])
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

// 线程 API（薄封装）
pub fn thread_create(entry: usize, user_sp: usize, arg: usize) -> isize {
    syscall(SYSCALL_THREAD_CREATE, [entry, user_sp, arg])
}

pub fn thread_exit(code: i32) -> ! {
    syscall(SYSCALL_THREAD_EXIT, [code as usize, 0, 0]);
    loop {}
}

pub fn thread_join(tid: usize, exit_code: &mut i32) -> isize {
    syscall(SYSCALL_THREAD_JOIN, [tid, exit_code as *mut i32 as usize, 0])
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

/// 非阻塞式等待指定进程结束
/// 返回值：-1表示进程不存在，-2表示进程还在运行，其他值表示进程已结束
pub fn wait_pid_nb(pid: usize, exit_code: *mut i32) -> isize {
    sys_wait(pid as isize, exit_code)
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

// 权限相关系统调用封装

pub fn getuid() -> u32 {
    syscall(SYSCALL_GETUID, [0, 0, 0]) as u32
}

pub fn getgid() -> u32 {
    syscall(SYSCALL_GETGID, [0, 0, 0]) as u32
}

pub fn geteuid() -> u32 {
    syscall(SYSCALL_GETEUID, [0, 0, 0]) as u32
}

pub fn getegid() -> u32 {
    syscall(SYSCALL_GETEGID, [0, 0, 0]) as u32
}

pub fn setuid(uid: u32) -> isize {
    syscall(SYSCALL_SETUID, [uid as usize, 0, 0])
}

pub fn setgid(gid: u32) -> isize {
    syscall(SYSCALL_SETGID, [gid as usize, 0, 0])
}

pub fn seteuid(euid: u32) -> isize {
    syscall(SYSCALL_SETEUID, [euid as usize, 0, 0])
}

pub fn setegid(egid: u32) -> isize {
    syscall(SYSCALL_SETEGID, [egid as usize, 0, 0])
}

pub fn chmod(path: &str, mode: u32) -> isize {
    let mut null_terminated_path = String::from(path);
    null_terminated_path.push('\0');
    syscall(SYSCALL_CHMOD, [null_terminated_path.as_ptr() as usize, mode as usize, 0])
}

pub fn chown(path: &str, uid: u32, gid: u32) -> isize {
    let mut null_terminated_path = String::from(path);
    null_terminated_path.push('\0');
    syscall(SYSCALL_CHOWN, [null_terminated_path.as_ptr() as usize, uid as usize, gid as usize])
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

/// 获取进程启动时的命令行参数
/// 返回: 参数个数，如果出错则返回负数
pub fn get_args(argc_buf: &mut usize, argv_buf: &mut [u8]) -> isize {
    syscall(SYSCALL_GET_ARGS, [argc_buf as *mut usize as usize, argv_buf.as_mut_ptr() as usize, argv_buf.len()])
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

/// fcntl - 文件控制操作
/// 参数：
/// - fd: 文件描述符
/// - cmd: fcntl 命令
/// - arg: 命令参数（可选）
/// 返回值：根据命令不同返回不同值，错误时返回负数
pub fn fcntl(fd: usize, cmd: i32, arg: usize) -> isize {
    syscall(SYSCALL_FCNTL, [fd, cmd as usize, arg])
}

/// 获取文件状态标志
pub fn fcntl_getfl(fd: usize) -> isize {
    fcntl(fd, fcntl_consts::F_GETFL, 0)
}

/// 设置文件状态标志
pub fn fcntl_setfl(fd: usize, flags: u32) -> isize {
    fcntl(fd, fcntl_consts::F_SETFL, flags as usize)
}

/// 获取文件描述符标志
pub fn fcntl_getfd(fd: usize) -> isize {
    fcntl(fd, fcntl_consts::F_GETFD, 0)
}

/// 设置文件描述符标志
pub fn fcntl_setfd(fd: usize, flags: i32) -> isize {
    fcntl(fd, fcntl_consts::F_SETFD, flags as usize)
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

/// 内存管理系统调用

/// 调整程序的数据段大小（堆管理）
pub fn brk(new_brk: usize) -> isize {
    syscall(SYSCALL_BRK, [new_brk, 0, 0])
}

/// 相对调整程序的数据段大小
pub fn sbrk(increment: isize) -> isize {
    syscall(SYSCALL_SBRK, [increment as usize, 0, 0])
}

/// 创建内存映射
pub fn mmap(addr: usize, length: usize, prot: i32) -> isize {
    syscall(SYSCALL_MMAP, [addr, length, prot as usize])
}

/// 解除内存映射
pub fn munmap(addr: usize, length: usize) -> isize {
    syscall(SYSCALL_MUNMAP, [addr, length, 0])
}

/// 内存保护标志
pub mod mmap_flags {
    pub const PROT_READ: i32 = 1;
    pub const PROT_WRITE: i32 = 2;
    pub const PROT_EXEC: i32 = 4;
    pub const PROT_NONE: i32 = 0;
}

// 共享内存封装
pub fn shm_create(size: usize) -> isize {
    syscall(SYSCALL_SHM_CREATE, [size, 0, 0])
}

pub fn shm_map(handle: usize, prot: i32) -> isize {
    syscall(SYSCALL_SHM_MAP, [handle, prot as usize, 0])
}

pub fn shm_close(handle: usize) -> isize {
    syscall(SYSCALL_SHM_CLOSE, [handle, 0, 0])
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

/// fcntl 命令常量
pub mod fcntl_consts {
    pub const F_GETFL: i32 = 3;   // 获取文件状态标志
    pub const F_SETFL: i32 = 4;   // 设置文件状态标志
    pub const F_GETFD: i32 = 1;   // 获取文件描述符标志
    pub const F_SETFD: i32 = 2;   // 设置文件描述符标志
}

/// 文件描述符标志
pub mod fd_flags {
    pub const FD_CLOEXEC: i32 = 1; // close-on-exec
}

/// 文件打开和状态标志常量
pub mod open_flags {
    pub const O_RDONLY: u32 = 0o0;    // 只读
    pub const O_WRONLY: u32 = 0o1;    // 只写
    pub const O_RDWR: u32 = 0o2;      // 读写
    pub const O_CREAT: u32 = 0o100;   // 创建文件
    pub const O_TRUNC: u32 = 0o1000;  // 截断文件
    pub const O_NONBLOCK: u32 = 0o4000;  // 非阻塞
    pub const O_APPEND: u32 = 0o2000;    // 追加模式
}

/// 错误码常量
pub mod errno {
    pub const EAGAIN: i32 = 11;       // 资源暂时不可用
    pub const EWOULDBLOCK: i32 = 11;  // 操作会阻塞（与EAGAIN相同）
    pub const EBADF: i32 = 9;         // 无效的文件描述符
    pub const EINVAL: i32 = 22;       // 无效的参数
    pub const EPERM: i32 = 1;         // 操作不允许
}

/// 获取进程列表
/// 参数：
/// - pids: 进程ID数组缓冲区
/// - max_count: 缓冲区最大容量
/// 返回值：实际进程数量
pub fn get_process_list(pids: &mut [u32]) -> isize {
    syscall(SYSCALL_GET_PROCESS_LIST, [pids.as_mut_ptr() as usize, pids.len(), 0])
}

/// 获取所有进程ID的数量
pub fn get_process_count() -> isize {
    syscall(SYSCALL_GET_PROCESS_LIST, [0, 0, 0])
}

/// 获取特定进程的详细信息
/// 参数：
/// - pid: 进程ID
/// - info: 用于存储进程信息的结构体
/// 返回值：成功返回0，失败返回-1
pub fn get_process_info(pid: u32, info: &mut ProcessInfo) -> isize {
    syscall(SYSCALL_GET_PROCESS_INFO, [pid as usize, info as *mut ProcessInfo as usize, 0])
}

/// 获取系统统计信息
/// 参数：
/// - stats: 用于存储系统统计信息的结构体
/// 返回值：成功返回0，失败返回-1
pub fn get_system_stats(stats: &mut SystemStats) -> isize {
    syscall(SYSCALL_GET_SYSTEM_STATS, [stats as *mut SystemStats as usize, 0, 0])
}

pub fn get_cpu_core_info() -> Option<CpuCoreInfo> {
    let mut core_info = CpuCoreInfo {
        total_cores: 0,
        active_cores: 0,
    };

    let result = syscall(SYSCALL_GET_CPU_CORE_INFO, [&mut core_info as *mut CpuCoreInfo as usize, 0, 0]);
    if result == 0 {
        Some(core_info)
    } else {
        None
    }
}

// 时间相关结构体和函数

/// POSIX timespec 结构体
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct TimeSpec {
    pub tv_sec: u64,  // 秒
    pub tv_nsec: u64, // 纳秒
}

/// POSIX timeval 结构体
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct TimeVal {
    pub tv_sec: u64,  // 秒
    pub tv_usec: u64, // 微秒
}

/// 获取当前时间（毫秒）
pub fn get_time_ms() -> isize {
    syscall(SYSCALL_GET_TIME_MS, [0, 0, 0])
}

/// 获取当前时间（微秒）
pub fn get_time_us() -> isize {
    syscall(SYSCALL_GET_TIME_US, [0, 0, 0])
}

/// 获取当前时间（纳秒）
pub fn get_time_ns() -> isize {
    syscall(SYSCALL_GET_TIME_NS, [0, 0, 0])
}

/// 获取 Unix 时间戳（秒）
pub fn time() -> isize {
    syscall(SYSCALL_TIME, [0, 0, 0])
}

/// POSIX gettimeofday - 获取当前时间和时区
pub fn gettimeofday(tv: &mut TimeVal, tz: *mut u8) -> isize {
    syscall(SYSCALL_GETTIMEOFDAY, [tv as *mut TimeVal as usize, tz as usize, 0])
}

/// 获取当前时间的便利函数
pub fn get_current_time() -> TimeVal {
    let mut tv = TimeVal { tv_sec: 0, tv_usec: 0 };
    gettimeofday(&mut tv, core::ptr::null_mut());
    tv
}

/// POSIX nanosleep - 高精度睡眠
/// 参数：
/// - req: 要睡眠的时间
/// - rem: 如果被信号中断，剩余时间（可以为null）
/// 返回值：成功返回0，失败返回-1
pub fn nanosleep(req: &TimeSpec, rem: *mut TimeSpec) -> isize {
    syscall(SYSCALL_NANOSLEEP, [req as *const TimeSpec as usize, rem as usize, 0])
}

/// 毫秒级睡眠（便利函数）
pub fn sleep_ms(ms: u64) -> isize {
    let req = TimeSpec {
        tv_sec: ms / 1000,
        tv_nsec: (ms % 1000) * 1_000_000,
    };
    nanosleep(&req, core::ptr::null_mut())
}

/// 微秒级睡眠（便利函数）
pub fn sleep_us(us: u64) -> isize {
    let req = TimeSpec {
        tv_sec: us / 1_000_000,
        tv_nsec: (us % 1_000_000) * 1000,
    };
    nanosleep(&req, core::ptr::null_mut())
}

/// 秒级睡眠（便利函数）
pub fn sleep(seconds: u64) -> isize {
    let req = TimeSpec {
        tv_sec: seconds,
        tv_nsec: 0,
    };
    nanosleep(&req, core::ptr::null_mut())
}

// Watchdog 相关结构体和函数

/// Watchdog 配置结构体
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct WatchdogConfig {
    /// 超时时间（微秒）
    pub timeout_us: u64,
    /// 是否启用
    pub enabled: bool,
    /// 是否在超时时重启系统
    pub reboot_on_timeout: bool,
    /// 预警时间（微秒），在超时前这个时间发出警告
    pub warning_time_us: u64,
}

impl Default for WatchdogConfig {
    fn default() -> Self {
        Self {
            timeout_us: 30_000_000, // 30 秒
            enabled: false,
            reboot_on_timeout: true,
            warning_time_us: 5_000_000, // 5 秒预警
        }
    }
}

/// Watchdog 状态
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WatchdogState {
    Disabled,
    Active,
    Warning,
    Timeout,
}

/// Watchdog 信息结构体
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct WatchdogInfo {
    pub state: WatchdogState,
    pub config: WatchdogConfig,
    pub time_since_feed_us: u64,
    pub feed_count: u64,
    pub timeout_count: u64,
}

/// 配置 watchdog
pub fn watchdog_configure(config: &WatchdogConfig) -> isize {
    syscall(SYSCALL_WATCHDOG_CONFIGURE, [config as *const WatchdogConfig as usize, 0, 0])
}

/// 启动 watchdog
pub fn watchdog_start() -> isize {
    syscall(SYSCALL_WATCHDOG_START, [0, 0, 0])
}

/// 停止 watchdog
pub fn watchdog_stop() -> isize {
    syscall(SYSCALL_WATCHDOG_STOP, [0, 0, 0])
}

/// 喂狗（重置计时器）
pub fn watchdog_feed() -> isize {
    syscall(SYSCALL_WATCHDOG_FEED, [0, 0, 0])
}

/// 获取 watchdog 信息
pub fn watchdog_get_info(info: &mut WatchdogInfo) -> isize {
    syscall(SYSCALL_WATCHDOG_GET_INFO, [info as *mut WatchdogInfo as usize, 0, 0])
}

/// 设置 watchdog 预设配置
/// preset: 0=开发模式, 1=生产模式, 2=严格模式, 3=测试模式
pub fn watchdog_set_preset(preset: u32) -> isize {
    syscall(SYSCALL_WATCHDOG_SET_PRESET, [preset as usize, 0, 0])
}

/// Watchdog 预设配置模块
pub mod watchdog_presets {
    use super::WatchdogConfig;

    /// 开发模式配置（较长超时时间）
    pub fn development() -> WatchdogConfig {
        WatchdogConfig {
            timeout_us: 60_000_000, // 60 秒
            enabled: true,
            reboot_on_timeout: false, // 开发时不重启
            warning_time_us: 10_000_000, // 10 秒预警
        }
    }

    /// 生产模式配置（较短超时时间）
    pub fn production() -> WatchdogConfig {
        WatchdogConfig {
            timeout_us: 30_000_000, // 30 秒
            enabled: true,
            reboot_on_timeout: true,
            warning_time_us: 5_000_000, // 5 秒预警
        }
    }

    /// 严格模式配置（很短超时时间）
    pub fn strict() -> WatchdogConfig {
        WatchdogConfig {
            timeout_us: 10_000_000, // 10 秒
            enabled: true,
            reboot_on_timeout: true,
            warning_time_us: 2_000_000, // 2 秒预警
        }
    }

    /// 测试模式配置（用于测试）
    pub fn testing() -> WatchdogConfig {
        WatchdogConfig {
            timeout_us: 5_000_000, // 5 秒
            enabled: true,
            reboot_on_timeout: false,
            warning_time_us: 1_000_000, // 1 秒预警
        }
    }
}
