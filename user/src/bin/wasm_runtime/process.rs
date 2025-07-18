//! 进程管理模块 - 利用LiteOS的进程管理系统调用

use alloc::string::String;
use alloc::string::ToString;
use user_lib::*;

/// 进程状态
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessStatus {
    Running,
    Exited(i32),
    Signaled(u32),
    Unknown,
}

/// 进程信息
#[derive(Debug)]
pub struct ProcessInfo {
    pub pid: usize,
    pub status: ProcessStatus,
}

/// 进程管理器
pub struct ProcessManager;

impl ProcessManager {
    /// 获取当前进程ID
    /// 利用LiteOS的getpid系统调用
    pub fn get_current_pid() -> usize {
        let pid = getpid();
        if pid >= 0 {
            pid as usize
        } else {
            0 // 如果获取失败，返回0
        }
    }
    
    /// 创建子进程
    /// 利用LiteOS的fork系统调用
    pub fn fork_process() -> Result<usize, String> {
        let pid = fork();
        
        match pid {
            pid if pid > 0 => {
                // 父进程：返回子进程PID
                Ok(pid as usize)
            }
            0 => {
                // 子进程：返回0
                Ok(0)
            }
            _ => {
                // fork失败
                Err(alloc::format!("Fork failed with error: {}", pid))
            }
        }
    }
    
    /// 执行新程序
    /// 利用LiteOS的exec系统调用
    pub fn exec_program(path: &str) -> Result<(), String> {
        let result = exec(path);
        
        if result != 0 {
            Err(alloc::format!("Exec failed for {}: {}", path, result))
        } else {
            Ok(())
        }
    }
    
    /// 执行新程序(带参数和环境变量)
    /// 利用LiteOS的execve系统调用
    pub fn exec_program_with_args(
        path: &str, 
        args: &[&str], 
        envs: &[&str]
    ) -> Result<(), String> {
        let result = execve(path, args, envs);
        
        if result != 0 {
            Err(alloc::format!("Execve failed for {}: {}", path, result))
        } else {
            Ok(())
        }
    }
    
    /// 等待任意子进程结束
    /// 利用LiteOS的wait系统调用
    pub fn wait_any_child() -> Result<ProcessInfo, String> {
        let mut exit_code = 0i32;
        let pid = wait(&mut exit_code as *mut i32);
        
        match pid {
            pid if pid > 0 => {
                Ok(ProcessInfo {
                    pid: pid as usize,
                    status: ProcessStatus::Exited(exit_code),
                })
            }
            -1 => {
                Err("No child processes".to_string())
            }
            -2 => {
                Err("No child processes have exited".to_string())
            }
            _ => {
                Err(alloc::format!("Wait failed with error: {}", pid))
            }
        }
    }
    
    /// 等待特定子进程结束
    /// 利用LiteOS的wait_pid系统调用
    pub fn wait_for_child(child_pid: usize) -> Result<ProcessInfo, String> {
        let mut exit_code = 0i32;
        let pid = wait_pid(child_pid, &mut exit_code as *mut i32);
        
        match pid {
            pid if pid > 0 => {
                Ok(ProcessInfo {
                    pid: pid as usize,
                    status: ProcessStatus::Exited(exit_code),
                })
            }
            -1 => {
                Err(alloc::format!("Child process {} not found", child_pid))
            }
            -2 => {
                Err(alloc::format!("Child process {} has not exited", child_pid))
            }
            _ => {
                Err(alloc::format!("Wait failed for child {}: {}", child_pid, pid))
            }
        }
    }
    
    /// 终止当前进程
    /// 利用LiteOS的exit系统调用
    pub fn exit_process(exit_code: i32) -> ! {
        exit(exit_code);
        loop {} // Unreachable but needed for ! return type
    }
    
    /// 让出CPU时间片
    /// 利用LiteOS的yield系统调用
    pub fn yield_cpu() {
        yield_();
    }
    
    /// 发送信号给进程
    /// 利用LiteOS的kill系统调用
    pub fn send_signal(pid: usize, signal: u32) -> Result<(), String> {
        let result = kill(pid, signal);
        
        if result != 0 {
            Err(alloc::format!("Failed to send signal {} to process {}: {}", signal, pid, result))
        } else {
            Ok(())
        }
    }
    
    /// 设置信号处理函数
    /// 利用LiteOS的signal系统调用
    pub fn set_signal_handler(signal_num: u32, handler: usize) -> Result<(), String> {
        let result = signal(signal_num, handler);
        
        if result != 0 {
            Err(alloc::format!("Failed to set signal handler for signal {}: {}", signal_num, result))
        } else {
            Ok(())
        }
    }
    
    /// 暂停进程直到收到信号
    /// 利用LiteOS的pause系统调用
    pub fn pause_until_signal() -> Result<(), String> {
        let result = pause();
        
        if result != 0 {
            Err(alloc::format!("Pause failed: {}", result))
        } else {
            Ok(())
        }
    }
    
    /// 设置定时器信号
    /// 利用LiteOS的alarm系统调用
    pub fn set_alarm(seconds: u32) -> Result<u32, String> {
        let result = alarm(seconds);
        
        if result < 0 {
            Err(alloc::format!("Failed to set alarm: {}", result))
        } else {
            Ok(result as u32) // 返回之前设置的定时器剩余时间
        }
    }
}

/// 信号相关常量和功能
pub mod signals {
    use super::*;
    pub use user_lib::signals::*;
    
    /// 信号处理器类型
    pub type SignalHandler = fn();
    
    /// 默认信号处理器
    pub fn default_signal_handler() {
        println!("Received signal - using default handler");
    }
    
    /// 忽略信号处理器
    pub fn ignore_signal_handler() {
        // 什么都不做，忽略信号
    }
    
    /// SIGINT处理器(Ctrl+C)
    pub fn sigint_handler() {
        println!("Received SIGINT (Ctrl+C)");
        ProcessManager::exit_process(130); // 128 + SIGINT
    }
    
    /// SIGTERM处理器
    pub fn sigterm_handler() {
        println!("Received SIGTERM");
        ProcessManager::exit_process(143); // 128 + SIGTERM
    }
    
    /// 设置常用信号处理器
    pub fn setup_common_signal_handlers() -> Result<(), String> {
        // 设置SIGINT处理器
        ProcessManager::set_signal_handler(SIGINT, sigint_handler as usize)?;
        
        // 设置SIGTERM处理器
        ProcessManager::set_signal_handler(SIGTERM, sigterm_handler as usize)?;
        
        // 忽略SIGPIPE
        ProcessManager::set_signal_handler(SIGPIPE, SIG_IGN)?;
        
        Ok(())
    }
}

/// 进程权限管理
pub mod permissions {
    use super::*;
    
    /// 用户权限信息
    #[derive(Debug)]
    pub struct UserInfo {
        pub uid: u32,
        pub gid: u32,
        pub euid: u32,
        pub egid: u32,
    }
    
    /// 权限管理器
    pub struct PermissionManager;
    
    impl PermissionManager {
        /// 获取当前用户信息
        /// 利用LiteOS的getuid、getgid等系统调用
        pub fn get_current_user_info() -> UserInfo {
            UserInfo {
                uid: getuid(),
                gid: getgid(),
                euid: geteuid(),
                egid: getegid(),
            }
        }
        
        /// 设置用户ID
        /// 利用LiteOS的setuid系统调用
        pub fn set_user_id(uid: u32) -> Result<(), String> {
            let result = setuid(uid);
            if result != 0 {
                Err(alloc::format!("Failed to set UID to {}: {}", uid, result))
            } else {
                Ok(())
            }
        }
        
        /// 设置组ID
        /// 利用LiteOS的setgid系统调用
        pub fn set_group_id(gid: u32) -> Result<(), String> {
            let result = setgid(gid);
            if result != 0 {
                Err(alloc::format!("Failed to set GID to {}: {}", gid, result))
            } else {
                Ok(())
            }
        }
        
        /// 设置有效用户ID
        /// 利用LiteOS的seteuid系统调用
        pub fn set_effective_user_id(euid: u32) -> Result<(), String> {
            let result = seteuid(euid);
            if result != 0 {
                Err(alloc::format!("Failed to set EUID to {}: {}", euid, result))
            } else {
                Ok(())
            }
        }
        
        /// 设置有效组ID
        /// 利用LiteOS的setegid系统调用
        pub fn set_effective_group_id(egid: u32) -> Result<(), String> {
            let result = setegid(egid);
            if result != 0 {
                Err(alloc::format!("Failed to set EGID to {}: {}", egid, result))
            } else {
                Ok(())
            }
        }
        
        /// 修改文件权限
        /// 利用LiteOS的chmod系统调用
        pub fn change_file_mode(path: &str, mode: u32) -> Result<(), String> {
            let result = chmod(path, mode);
            if result != 0 {
                Err(alloc::format!("Failed to chmod {} to {:o}: {}", path, mode, result))
            } else {
                Ok(())
            }
        }
        
        /// 修改文件所有者
        /// 利用LiteOS的chown系统调用
        pub fn change_file_owner(path: &str, uid: u32, gid: u32) -> Result<(), String> {
            let result = chown(path, uid, gid);
            if result != 0 {
                Err(alloc::format!("Failed to chown {} to {}:{}: {}", path, uid, gid, result))
            } else {
                Ok(())
            }
        }
    }
    
    /// 权限常量
    pub mod mode_constants {
        pub const S_IRUSR: u32 = 0o400; // 用户读权限
        pub const S_IWUSR: u32 = 0o200; // 用户写权限
        pub const S_IXUSR: u32 = 0o100; // 用户执行权限
        pub const S_IRGRP: u32 = 0o040; // 组读权限
        pub const S_IWGRP: u32 = 0o020; // 组写权限
        pub const S_IXGRP: u32 = 0o010; // 组执行权限
        pub const S_IROTH: u32 = 0o004; // 其他读权限
        pub const S_IWOTH: u32 = 0o002; // 其他写权限
        pub const S_IXOTH: u32 = 0o001; // 其他执行权限
        
        pub const S_IRWXU: u32 = S_IRUSR | S_IWUSR | S_IXUSR; // 用户所有权限
        pub const S_IRWXG: u32 = S_IRGRP | S_IWGRP | S_IXGRP; // 组所有权限
        pub const S_IRWXO: u32 = S_IROTH | S_IWOTH | S_IXOTH; // 其他所有权限
    }
}

/// 内存管理相关功能
pub mod memory {
    use super::*;
    
    /// 内存管理器
    pub struct MemoryManager;
    
    impl MemoryManager {
        /// 调整程序数据段大小
        /// 利用LiteOS的brk系统调用
        pub fn set_program_break(new_brk: usize) -> Result<usize, String> {
            let result = brk(new_brk);
            if result < 0 {
                Err(alloc::format!("Failed to set program break to {}: {}", new_brk, result))
            } else {
                Ok(result as usize)
            }
        }
        
        /// 相对调整程序数据段大小
        /// 利用LiteOS的sbrk系统调用
        pub fn adjust_program_break(increment: isize) -> Result<usize, String> {
            let result = sbrk(increment);
            if result < 0 {
                Err(alloc::format!("Failed to adjust program break by {}: {}", increment, result))
            } else {
                Ok(result as usize)
            }
        }
        
        /// 创建内存映射
        /// 利用LiteOS的mmap系统调用
        pub fn create_memory_mapping(addr: usize, length: usize, prot: i32) -> Result<usize, String> {
            let result = mmap(addr, length, prot);
            if result < 0 {
                Err(alloc::format!("Failed to create memory mapping: {}", result))
            } else {
                Ok(result as usize)
            }
        }
        
        /// 解除内存映射
        /// 利用LiteOS的munmap系统调用
        pub fn remove_memory_mapping(addr: usize, length: usize) -> Result<(), String> {
            let result = munmap(addr, length);
            if result != 0 {
                Err(alloc::format!("Failed to remove memory mapping: {}", result))
            } else {
                Ok(())
            }
        }
    }
}