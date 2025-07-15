use core::{cell::RefMut, error::Error, sync::atomic};

use alloc::{
    boxed::Box, string::{String, ToString}, sync::{Arc, Weak}, vec::Vec, collections::BTreeMap
};

use crate::{
    memory::{
        KERNEL_SPACE, TRAP_CONTEXT,
        address::{PhysicalPageNumber, VirtualAddress},
        mm::{self, MemorySet},
    },
    task::{
        context::TaskContext,
        pid::{KernelStack, PidHandle, alloc_pid},
        signal::SignalState,
    },
    trap::{TrapContext, trap_handler},
    fs::inode::Inode,
    thread::ThreadManager,
};

pub struct FileDescriptor {
    pub inode: Arc<dyn Inode>,
    pub offset: atomic::AtomicU64,
    pub flags: u32,
    pub mode: u32,
}

impl core::fmt::Debug for FileDescriptor {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("FileDescriptor")
            .field("offset", &self.offset)
            .field("flags", &self.flags)
            .field("mode", &self.mode)
            .finish()
    }
}

impl FileDescriptor {
    pub fn new(inode: Arc<dyn Inode>, flags: u32) -> Self {
        Self {
            inode,
            offset: atomic::AtomicU64::new(0),
            flags,
            mode: 0o644, // Default file mode
        }
    }

    pub fn read_at(&self, buf: &mut [u8]) -> Result<usize, crate::fs::FileSystemError> {
        // 对于FIFO等特殊文件，先释放offset借用以避免阻塞时的借用冲突
        let current_offset = self.offset.load(atomic::Ordering::Relaxed);
        let result = self.inode.read_at(current_offset, buf);
        if let Ok(bytes_read) = result {
            self.offset.fetch_add(bytes_read as u64, atomic::Ordering::Relaxed);
        }
        result
    }

    pub fn write_at(&self, buf: &[u8]) -> Result<usize, crate::fs::FileSystemError> {
        // 对于FIFO等特殊文件，先释放offset借用以避免阻塞时的借用冲突
        let current_offset = self.offset.load(atomic::Ordering::Relaxed);
        let result = self.inode.write_at(current_offset, buf);
        if let Ok(bytes_written) = result {
            self.offset.fetch_add(bytes_written as u64, atomic::Ordering::Relaxed);
        }
        result
    }
}

#[derive(Debug, Copy, Clone, PartialEq)]
pub enum TaskStatus {
    Ready,
    Running,
    Exited,
    Zombie,
    Sleeping,    // 对应Linux的TASK_INTERRUPTIBLE，可中断的睡眠/阻塞
}

/// 进程管理相关状态
#[derive(Debug)]
pub struct ProcessManagement {
    /// 父进程, 子进程有可能在父进程退出时还存活，因此需要弱引用
    pub parent: Option<Weak<TaskControlBlock>>,
    /// 子进程
    pub children: Vec<Arc<TaskControlBlock>>,
    /// 子进程退出时，父进程可以获取其退出码
    pub exit_code: i32,
    /// 当前工作目录
    pub cwd: String,
    /// 是否为主线程 (每个进程的第一个任务)
    pub is_main_thread: bool,
    /// 线程组ID (TGID) - 对于主线程等于PID，对于其他线程等于主线程PID
    pub tgid: usize,
}

/// 调度相关状态
#[derive(Debug)]
pub struct SchedulingInfo {
    /// 进程状态
    pub task_status: TaskStatus,
    /// 用户态的 TaskContext
    pub task_cx: TaskContext,
    /// 进程优先级 (0-139, 0最高优先级，139最低优先级)
    pub priority: i32,
    /// nice值 (-20到19, 影响动态优先级计算)
    pub nice: i32,
    /// 累计运行时间 (用于CFS调度算法)
    pub vruntime: u64,
    /// 上次运行时的时间戳
    pub last_runtime: u64,
    /// 动态时间片大小 (微秒)
    pub time_slice: u64,
    /// CPU亲和性掩码
    pub cpu_affinity: u64,
}

/// 内存管理相关状态
#[derive(Debug)]
pub struct MemoryManagement {
    /// 用户态的内存空间
    pub memory_set: mm::MemorySet,
    /// 用户态的 TrapContext 的物理页号
    pub trap_cx_ppn: PhysicalPageNumber,
    /// 应用数据仅有可能出现在应用地址空间低于 base_size 字节的区域中。
    /// 借助它我们可以清楚的知道应用有多少数据驻留在内存中。
    pub base_size: usize,
}

/// 文件系统相关状态
#[derive(Debug)]
pub struct FileSystemInfo {
    /// 文件描述符表
    pub fd_table: BTreeMap<usize, Arc<FileDescriptor>>,
    /// 下一个可分配的文件描述符
    pub next_fd: usize,
}

/// 安全相关状态
#[derive(Debug, Clone)]
pub struct SecurityInfo {
    /// 用户ID
    pub uid: u32,
    /// 组ID
    pub gid: u32,
    /// 有效用户ID (用于权限检查)
    pub euid: u32,
    /// 有效组ID (用于权限检查)
    pub egid: u32,
}

/// 定时器相关状态
#[derive(Debug)]
pub struct TimerInfo {
    /// alarm定时器时间 (微秒时间戳)
    pub alarm_time: Option<u64>,
}

/// 重构后的任务控制块内部结构
/// 采用Linux内核task_struct的设计理念，按功能模块分组
#[derive(Debug)]
pub struct TaskControlBlockInner {
    /// 进程管理相关
    pub process: ProcessManagement,
    /// 调度相关
    pub sched: SchedulingInfo,
    /// 内存管理相关
    pub mm: MemoryManagement,
    /// 文件系统相关
    pub files: FileSystemInfo,
    /// 安全相关
    pub security: SecurityInfo,
    /// 信号状态
    pub signal_state: SignalState,
    /// 定时器相关
    pub timer: TimerInfo,
    /// 线程管理器 (仅主线程拥有)
    pub thread_manager: Option<ThreadManager>,
}

/// Task Control block structure
#[derive(Debug)]
pub struct TaskControlBlock {
    pub pid: PidHandle,
    pub kernel_stack: KernelStack,

    inner: spin::Mutex<TaskControlBlockInner>,
}

impl TaskControlBlock {
    pub fn new(elf_data: &[u8]) -> Result<Self, Box<dyn Error>> {
        let (memory_set, user_sp, entry_point) = MemorySet::from_elf(elf_data)?;

        let task_status = TaskStatus::Ready;

        let pid = alloc_pid();
        let pid_val = pid.0;
        let kernel_stack = KernelStack::new(pid_val);
        let kernel_stack_top = kernel_stack.get_top();

        // 获取用户空间中TRAP_CONTEXT映射的物理页面
        let trap_cx_ppn = memory_set
            .translate(VirtualAddress::from(TRAP_CONTEXT).into())
            .expect("TRAP_CONTEXT should be mapped")
            .ppn();

        let tcb = Self {
            pid,
            kernel_stack,
            inner: spin::Mutex::new(TaskControlBlockInner {
                process: ProcessManagement {
                    parent: None,
                    children: Vec::new(),
                    exit_code: 0,
                    cwd: "/".to_string(),
                    is_main_thread: true,
                    tgid: pid_val,
                },
                sched: SchedulingInfo {
                    task_status,
                    task_cx: TaskContext::goto_trap_return(kernel_stack_top),
                    priority: 20,
                    nice: 0,
                    vruntime: 0,
                    last_runtime: 0,
                    time_slice: 10000,
                    cpu_affinity: u64::MAX, // 默认可在所有CPU运行
                },
                mm: MemoryManagement {
                    memory_set,
                    trap_cx_ppn,
                    base_size: user_sp,
                },
                files: FileSystemInfo {
                    fd_table: BTreeMap::new(),
                    next_fd: 3,
                },
                security: SecurityInfo {
                    uid: 0,
                    gid: 0,
                    euid: 0,
                    egid: 0,
                },
                signal_state: SignalState::new(),
                timer: TimerInfo {
                    alarm_time: None,
                },
                thread_manager: None,
            }),
        };

        // prepare TrapContext in user space
        let trap_cx = tcb.inner_exclusive_access().get_trap_cx();
        *trap_cx = TrapContext::app_init_context(
            entry_point,
            user_sp,
            KERNEL_SPACE.wait().lock().token(),
            kernel_stack_top,
            trap_handler as usize,
        );
        Ok(tcb)
    }

    pub fn get_pid(&self) -> usize {
        self.pid.0
    }

    pub fn inner_exclusive_access(&self) -> spin::MutexGuard<'_, TaskControlBlockInner> {
        self.inner.lock()
    }

    pub fn exec(&self, elf_data: &[u8]) -> Result<(), Box<dyn Error>> {
        let (memory_set, user_stack_top, entrypoint) = MemorySet::from_elf(elf_data)?;
        let trap_cx_ppn = memory_set
            .translate(VirtualAddress::from(TRAP_CONTEXT).into())
            .unwrap()
            .ppn();

        let mut inner = self.inner_exclusive_access();
        inner.mm.trap_cx_ppn = trap_cx_ppn;
        inner.mm.memory_set = memory_set;
        // 重置信号状态（exec时应该重置信号处理器）
        inner.signal_state.reset_for_exec();
        // 重置vruntime以确保公平调度
        inner.sched.vruntime = 0;
        debug!("exec: reset vruntime to 0 for PID {}", self.get_pid());
        let trap_cx = inner.get_trap_cx();
        *trap_cx = TrapContext::app_init_context(
            entrypoint,
            user_stack_top,
            KERNEL_SPACE.wait().lock().token(),
            self.kernel_stack.get_top(),
            trap_handler as usize,
        );
        Ok(())
    }

    /// Execute a new program with arguments and environment variables
    pub fn exec_with_args(
        &self,
        elf_data: &[u8],
        args: &[String],
        envs: &[String]
    ) -> Result<(), Box<dyn Error>> {
        let (memory_set, user_stack_top, entrypoint) = MemorySet::from_elf_with_args(elf_data, args, envs)?;
        let trap_cx_ppn = memory_set
            .translate(VirtualAddress::from(TRAP_CONTEXT).into())
            .unwrap()
            .ppn();
        let mut inner = self.inner_exclusive_access();
        inner.mm.trap_cx_ppn = trap_cx_ppn;
        inner.mm.memory_set = memory_set;
        // 重置信号状态（exec时应该重置信号处理器）
        inner.signal_state.reset_for_exec();
        // 重置vruntime以确保公平调度
        inner.sched.vruntime = 0;
        debug!("exec_with_args: reset vruntime to 0 for PID {}", self.get_pid());
        let trap_cx = inner.get_trap_cx();
        *trap_cx = TrapContext::app_init_context(
            entrypoint,
            user_stack_top,
            KERNEL_SPACE.wait().lock().token(),
            self.kernel_stack.get_top(),
            trap_handler as usize,
        );
        Ok(())
    }

    pub fn fork(self: &Arc<Self>) -> Arc<Self> {
        let mut parent_inner = self.inner_exclusive_access();
        let memory_set = MemorySet::form_existed_user(&parent_inner.mm.memory_set);
        let trap_cx_ppn = memory_set
            .translate(VirtualAddress::from(TRAP_CONTEXT).into())
            .unwrap()
            .ppn();

        // alloc a pid and a kernel stack in kernel space
        let pid = alloc_pid();
        let pid_val = pid.0;
        let kernel_stack = KernelStack::new(pid_val);
        let kernel_stack_top = kernel_stack.get_top();

        let tcb = Arc::new(TaskControlBlock {
            pid,
            kernel_stack,
            inner: spin::Mutex::new(TaskControlBlockInner {
                process: ProcessManagement {
                    parent: Some(Arc::downgrade(self)),
                    children: Vec::new(),
                    exit_code: 0,
                    cwd: parent_inner.process.cwd.clone(),
                    is_main_thread: true,
                    tgid: pid_val, // 新进程的主线程
                },
                sched: SchedulingInfo {
                    task_status: TaskStatus::Ready,
                    task_cx: TaskContext::goto_trap_return(kernel_stack_top),
                    priority: parent_inner.sched.priority,
                    nice: parent_inner.sched.nice,
                    vruntime: 0,
                    last_runtime: 0,
                    time_slice: parent_inner.sched.time_slice,
                    cpu_affinity: parent_inner.sched.cpu_affinity,
                },
                mm: MemoryManagement {
                    memory_set,
                    trap_cx_ppn,
                    base_size: parent_inner.mm.base_size,
                },
                files: FileSystemInfo {
                    fd_table: parent_inner.files.fd_table.clone(),
                    next_fd: parent_inner.files.next_fd,
                },
                security: parent_inner.security.clone(),
                signal_state: parent_inner.signal_state.clone_for_fork(),
                timer: TimerInfo {
                    alarm_time: None, // 子进程不继承父进程的alarm
                },
                thread_manager: None, // 子进程默认不启用多线程
            }),
        });

        parent_inner.process.children.push(tcb.clone());
        let trap_cx = tcb.inner_exclusive_access().get_trap_cx();
        trap_cx.kernel_sp = kernel_stack_top;
        tcb
    }

    /// 初始化线程管理器（为支持多线程进程）
    pub fn init_thread_manager(self: &Arc<Self>) {
        let mut inner = self.inner_exclusive_access();
        debug!("init_thread_manager called for PID {}, task addr: {:p}, current thread_manager: {}", 
               self.get_pid(), self.as_ref(), inner.thread_manager.is_some());
        if inner.thread_manager.is_none() {
            let trap_cx_ppn = inner.mm.trap_cx_ppn; // 获取陷入上下文页面号
            inner.thread_manager = Some(crate::thread::ThreadManager::new(Arc::clone(self), trap_cx_ppn));
            debug!("Thread manager initialized for process PID {}, task addr: {:p}", 
                   self.get_pid(), self.as_ref());
        } else {
            debug!("Thread manager already exists for process PID {}, task addr: {:p}", 
                   self.get_pid(), self.as_ref());
        }
    }

    /// 检查是否支持多线程
    pub fn supports_threading(&self) -> bool {
        self.inner_exclusive_access().thread_manager.is_some()
    }
}

impl TaskControlBlockInner {
    pub fn get_trap_cx(&self) -> &'static mut TrapContext {
        self.mm.trap_cx_ppn.get_mut()
    }

    pub fn get_user_token(&self) -> usize {
        self.mm.memory_set.token()
    }

    pub fn get_status(&self) -> TaskStatus {
        self.sched.task_status
    }

    pub fn is_zombie(&self) -> bool {
        self.sched.task_status == TaskStatus::Zombie
    }

    /// 计算动态优先级 (基于nice值)
    pub fn get_dynamic_priority(&self) -> i32 {
        // Linux-like priority calculation: priority = 20 + nice
        // 范围: 0-39 (nice: -20到19)
        (20 + self.sched.nice).max(0).min(39)
    }

    /// 更新虚拟运行时间 (CFS算法核心)
    pub fn update_vruntime(&mut self, runtime_us: u64) {
        // 根据优先级调整权重，优先级越高权重越大，vruntime增长越慢
        let weight = match self.get_dynamic_priority() {
            0..=9 => 4,    // 高优先级
            10..=19 => 2,  // 中等优先级
            20..=29 => 1,  // 默认优先级
            _ => 1,        // 低优先级
        };
        self.sched.vruntime += runtime_us / weight;
    }

    /// 计算时间片大小 (基于优先级)
    pub fn calculate_time_slice(&self) -> u64 {
        // 基础时间片为10ms，根据优先级调整
        let base_slice = 10000; // 10ms in microseconds
        let priority = self.get_dynamic_priority();

        match priority {
            0..=9 => base_slice * 2,    // 高优先级：20ms
            10..=19 => base_slice * 3 / 2, // 中等优先级：15ms
            20..=29 => base_slice,      // 默认优先级：10ms
            _ => base_slice / 2,        // 低优先级：5ms
        }
    }

    /// 设置nice值并更新优先级
    pub fn set_nice(&mut self, nice: i32) {
        self.sched.nice = nice.max(-20).min(19);
        self.sched.priority = self.get_dynamic_priority();
        self.sched.time_slice = self.calculate_time_slice();
    }

    /// 分配新的文件描述符
    pub fn alloc_fd(&mut self, file_desc: Arc<FileDescriptor>) -> usize {
        let fd = self.files.next_fd;
        self.files.fd_table.insert(fd, file_desc);
        self.files.next_fd += 1;
        fd
    }

    /// 根据文件描述符获取FileDescriptor
    pub fn get_fd(&self, fd: usize) -> Option<Arc<FileDescriptor>> {
        self.files.fd_table.get(&fd).cloned()
    }

    /// 关闭文件描述符
    pub fn close_fd(&mut self, fd: usize) -> bool {
        self.files.fd_table.remove(&fd).is_some()
    }

    /// 关闭所有文件描述符（进程退出时调用）
    pub fn close_all_fds(&mut self) {
        self.files.fd_table.clear();
    }

    /// 关闭所有文件描述符并清理文件锁（进程退出时调用）
    pub fn close_all_fds_and_cleanup_locks(&mut self, pid: usize) {
        // 清理文件锁
        crate::fs::get_file_lock_manager().remove_process_locks(pid);
        self.files.fd_table.clear();
    }

    /// 复制文件描述符（用于 dup 系统调用）
    pub fn dup_fd(&mut self, fd: usize) -> Option<usize> {
        if let Some(file_desc) = self.files.fd_table.get(&fd) {
            let new_fd = self.files.next_fd;
            // 获取当前偏移量值
            let current_offset = file_desc.offset.load(atomic::Ordering::Relaxed);
            // 创建新的 FileDescriptor，复制当前偏移量
            let new_file_desc = Arc::new(FileDescriptor {
                inode: file_desc.inode.clone(),
                offset: atomic::AtomicU64::new(current_offset),
                flags: file_desc.flags,
                mode: file_desc.mode,
            });
            self.files.fd_table.insert(new_fd, new_file_desc);
            self.files.next_fd += 1;
            Some(new_fd)
        } else {
            None
        }
    }

    /// 复制文件描述符到指定的文件描述符号（用于 dup2 系统调用）
    pub fn dup2_fd(&mut self, oldfd: usize, newfd: usize) -> Option<usize> {
        // 如果 oldfd 和 newfd 相同，则直接返回 newfd（如果 oldfd 有效）
        if oldfd == newfd {
            return if self.files.fd_table.contains_key(&oldfd) {
                Some(newfd)
            } else {
                None
            };
        }

        // 首先获取 oldfd 的文件描述符信息
        let (inode, current_offset, flags, mode) = {
            if let Some(file_desc) = self.files.fd_table.get(&oldfd) {
                let current_offset = file_desc.offset.load(atomic::Ordering::Relaxed);
                (file_desc.inode.clone(), current_offset, file_desc.flags, file_desc.mode)
            } else {
                return None;
            }
        };

        // 如果 newfd 已存在，先关闭它
        if self.files.fd_table.contains_key(&newfd) {
            self.files.fd_table.remove(&newfd);
        }

        // 创建新的 FileDescriptor，复制当前偏移量
        let new_file_desc = Arc::new(FileDescriptor {
            inode,
            offset: atomic::AtomicU64::new(current_offset),
            flags,
            mode,
        });
        self.files.fd_table.insert(newfd, new_file_desc);

        // 更新 next_fd 以避免与新分配的 fd 冲突
        if newfd >= self.files.next_fd {
            self.files.next_fd = newfd + 1;
        }

        Some(newfd)
    }

    /// 发送信号给进程
    pub fn send_signal(&mut self, signal: crate::task::signal::Signal) {
        self.signal_state.add_pending_signal(signal);
    }

    /// 检查是否有可处理的信号
    pub fn has_pending_signals(&self) -> bool {
        self.signal_state.has_deliverable_signals()
    }

    /// 获取下一个待处理的信号
    pub fn next_signal(&self) -> Option<crate::task::signal::Signal> {
        self.signal_state.next_deliverable_signal()
    }

    /// 设置信号处理器
    pub fn set_signal_handler(&self, signal: crate::task::signal::Signal, handler: crate::task::signal::SignalDisposition) {
        self.signal_state.set_handler(signal, handler);
    }

    /// 获取信号处理器
    pub fn get_signal_handler(&self, signal: crate::task::signal::Signal) -> crate::task::signal::SignalDisposition {
        self.signal_state.get_handler(signal)
    }

    /// 设置信号掩码
    pub fn set_signal_mask(&self, mask: crate::task::signal::SignalSet) {
        self.signal_state.set_signal_mask(mask);
    }

    /// 获取信号掩码
    pub fn get_signal_mask(&self) -> crate::task::signal::SignalSet {
        self.signal_state.get_signal_mask()
    }

    /// 阻塞信号
    pub fn block_signals(&self, signals: crate::task::signal::SignalSet) {
        self.signal_state.block_signals(signals);
    }

    /// 解除阻塞信号
    pub fn unblock_signals(&self, signals: crate::task::signal::SignalSet) {
        self.signal_state.unblock_signals(signals);
    }

    /// 获取用户ID
    pub fn get_uid(&self) -> u32 {
        self.security.uid
    }

    /// 获取组ID
    pub fn get_gid(&self) -> u32 {
        self.security.gid
    }

    /// 获取有效用户ID
    pub fn get_euid(&self) -> u32 {
        self.security.euid
    }

    /// 获取有效组ID
    pub fn get_egid(&self) -> u32 {
        self.security.egid
    }

    /// 设置用户ID (需要root权限)
    pub fn set_uid(&mut self, uid: u32) -> Result<(), i32> {
        // 只有root用户可以设置任意UID
        if self.security.euid != 0 && self.security.euid != uid {
            return Err(-1); // EPERM
        }
        self.security.uid = uid;
        self.security.euid = uid;
        Ok(())
    }

    /// 设置组ID (需要root权限)
    pub fn set_gid(&mut self, gid: u32) -> Result<(), i32> {
        // 只有root用户可以设置任意GID
        if self.security.euid != 0 && self.security.egid != gid {
            return Err(-1); // EPERM
        }
        self.security.gid = gid;
        self.security.egid = gid;
        Ok(())
    }

    /// 设置有效用户ID
    pub fn set_euid(&mut self, euid: u32) -> Result<(), i32> {
        // 只有root用户或设置为实际UID才允许
        if self.security.euid != 0 && euid != self.security.uid {
            return Err(-1); // EPERM
        }
        self.security.euid = euid;
        Ok(())
    }

    /// 设置有效组ID
    pub fn set_egid(&mut self, egid: u32) -> Result<(), i32> {
        // 只有root用户或设置为实际GID才允许
        if self.security.euid != 0 && egid != self.security.gid {
            return Err(-1); // EPERM
        }
        self.security.egid = egid;
        Ok(())
    }

    /// 检查是否为root用户
    pub fn is_root(&self) -> bool {
        self.security.euid == 0
    }

    /// 检查对文件的访问权限
    pub fn check_file_permission(&self, file_mode: u32, file_uid: u32, file_gid: u32, requested: u32) -> bool {
        // root用户拥有所有权限
        if self.security.euid == 0 {
            return true;
        }

        let mut effective_mode = 0;

        // 检查用户权限
        if self.security.euid == file_uid {
            effective_mode = (file_mode >> 6) & 0o7; // 用户权限位
        }
        // 检查组权限
        else if self.security.egid == file_gid {
            effective_mode = (file_mode >> 3) & 0o7; // 组权限位
        }
        // 其他用户权限
        else {
            effective_mode = file_mode & 0o7; // 其他用户权限位
        }

        (effective_mode & requested) == requested
    }

    /// 启用多线程支持
    pub fn enable_threading(&mut self, task: Arc<TaskControlBlock>, trap_cx_ppn: PhysicalPageNumber) {
        if self.thread_manager.is_none() {
            self.thread_manager = Some(ThreadManager::new(task, trap_cx_ppn));
        }
    }

    /// 获取线程管理器
    pub fn get_thread_manager(&mut self) -> Option<&mut ThreadManager> {
        self.thread_manager.as_mut()
    }

    /// 检查是否启用了多线程
    pub fn is_threading_enabled(&self) -> bool {
        self.thread_manager.is_some()
    }
}
