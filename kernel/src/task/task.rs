use core::{
    cell::RefMut,
    error::Error,
    sync::atomic::{self, AtomicBool, AtomicI32, AtomicU32, AtomicU64, AtomicUsize},
};

use alloc::{
    boxed::Box,
    collections::BTreeMap,
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use spin::Mutex;

use crate::{
    fs::inode::Inode,
    memory::{
        KERNEL_SPACE, TRAP_CONTEXT, TRAP_CONTEXT_BASE, MAX_THREADS_PER_PROCESS, PAGE_SIZE,
        address::VirtualAddress,
        kernel_stack::KernelStack,
        mm::{self, MemorySet},
    },
    signal::SignalState,
    task::{
        add_task,
        context::TaskContext,
        pid::{PidHandle, alloc_pid},
    },
    trap::{TrapContext, trap_handler},
};

pub struct FileDescriptor {
    pub inode: Arc<dyn Inode>,
    pub offset: atomic::AtomicU64,
    pub flags: u32,
    pub mode: u32,
    /// Whether this descriptor has seen successful writes and needs sync on drop
    pub dirty_on_close: atomic::AtomicBool,
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

impl Drop for FileDescriptor {
    fn drop(&mut self) {
        // 仅当曾经发生过写入时才进行同步，避免批量关闭只读FD卡顿
        if self.dirty_on_close.load(atomic::Ordering::Acquire) {
            if let Err(e) = self.inode.sync() {
                warn!("Failed to sync file on close: {:?}", e);
            }
        }
    }
}

impl FileDescriptor {
    pub fn new(inode: Arc<dyn Inode>, flags: u32) -> Self {
        Self {
            inode,
            offset: atomic::AtomicU64::new(0),
            flags,
            mode: 0o644, // Default file mode
            dirty_on_close: atomic::AtomicBool::new(false),
        }
    }

    pub fn read_at(&self, buf: &mut [u8]) -> Result<usize, crate::fs::FileSystemError> {
        // 对于FIFO等特殊文件，先释放offset借用以避免阻塞时的借用冲突
        let current_offset = self.offset.load(atomic::Ordering::Relaxed);
        let result = self.inode.read_at(current_offset, buf);
        if let Ok(bytes_read) = result {
            self.offset
                .fetch_add(bytes_read as u64, atomic::Ordering::Relaxed);
        }
        result
    }

    pub fn write_at(&self, buf: &[u8]) -> Result<usize, crate::fs::FileSystemError> {
        // 对于FIFO等特殊文件，先释放offset借用以避免阻塞时的借用冲突
        let current_offset = self.offset.load(atomic::Ordering::Relaxed);
        let result = self.inode.write_at(current_offset, buf);
        if let Ok(bytes_written) = result {
            self.offset
                .fetch_add(bytes_written as u64, atomic::Ordering::Relaxed);
            if bytes_written > 0 {
                self
                    .dirty_on_close
                    .store(true, atomic::Ordering::Release);
            }
        }
        result
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum TaskStatus {
    Ready,
    Running,
    Zombie,
    Sleeping, // 对应Linux的TASK_INTERRUPTIBLE，可中断的睡眠/阻塞
    Stopped,  // 对应Linux的TASK_STOPPED，被信号暂停（如SIGTSTP）
}

#[derive(Debug)]
pub struct Memory {
    /// 用户态的内存空间（线程共享）
    pub memory_set: alloc::sync::Arc<Mutex<mm::MemorySet>>,
    /// 内核栈
    kernel_stack: KernelStack,
    /// 用户态 TrapContext 的虚拟地址（每线程独立）
    trap_cx_va: Mutex<usize>,
    /// 用户程序堆的基地址
    pub heap_base: AtomicUsize,
    /// 用户程序堆的顶部地址
    pub heap_top: AtomicUsize,
    /// 用户态的 TaskContext
    pub task_cx: Mutex<TaskContext>,
}

impl Memory {
    pub fn set_trap_context_va(&self, trap_context_va: usize) {
        *self.trap_cx_va.lock() = trap_context_va;
    }

    pub fn set_trap_context(&self, trap_context: TrapContext) {
        let va = *self.trap_cx_va.lock();
        let ppn = self
            .memory_set
            .lock()
            .trap_context_ppn(va);
        *ppn.get_mut() = trap_context;
    }

    pub fn trap_context(&self) -> &'static mut TrapContext {
        let va = *self.trap_cx_va.lock();
        let ppn = self
            .memory_set
            .lock()
            .trap_context_ppn(va);
        ppn.get_mut()
    }

    pub fn remove_area_with_start_vpn(&self, start_va: VirtualAddress) {
        self.memory_set
            .lock()
            .remove_area_with_start_vpn(start_va.floor());
    }
}

#[derive(Debug)]
pub struct File {
    /// 文件描述符表
    fd_table: BTreeMap<usize, Arc<FileDescriptor>>,
    /// 下一个可分配的文件描述符
    next_fd: usize,
}

impl File {
    /// 分配新的文件描述符
    pub fn alloc_fd(&mut self, file_desc: Arc<FileDescriptor>) -> Option<usize> {
        // 检查文件描述符数量限制
        const MAX_FD_COUNT: usize = 1024; // 每个进程最多打开1024个文件

        if self.fd_table.len() >= MAX_FD_COUNT {
            error!("[FD_ALLOC] CRITICAL: FD table full! {} open files (max {})",
                   self.fd_table.len(), MAX_FD_COUNT);
            return None; // 达到上限，返回None表示分配失败
        }

        // Log milestone FDs to track progress
        let start_fd = self.next_fd;
        // 寻找下一个可用的文件描述符
        let mut fd = self.next_fd;
        let mut search_count = 0;
        while self.fd_table.contains_key(&fd) {
            fd += 1;
            search_count += 1;
            // 修复：检查搜索次数而不是FD数值，防止FD编号大于MAX_FD_COUNT时错误退出
            // MAX_FD_COUNT是文件表大小限制，不是FD编号限制
            if search_count >= MAX_FD_COUNT {
                error!("[FD_ALLOC] CRITICAL: FD search exhausted after {} attempts! Table has {} entries",
                       search_count, self.fd_table.len());
                return None; // 防止无限循环
            }
        }

        // Log if search took a long time
        if search_count > 100 {
            warn!("[FD_ALLOC] Slow FD search: {} attempts to find FD {} (table fragmented?)",
                  search_count, fd);
        }

        self.fd_table.insert(fd, file_desc);
        self.next_fd = fd + 1;

        Some(fd)
    }

    /// 根据文件描述符获取FileDescriptor
    pub fn fd(&self, fd: usize) -> Option<Arc<FileDescriptor>> {
        self.fd_table.get(&fd).cloned()
    }

    /// 关闭文件描述符
    pub fn close_fd(&mut self, fd: usize) -> bool {
        self.fd_table.remove(&fd).is_some()
    }

    /// 关闭所有文件描述符（进程退出时调用）
    pub fn close_all_fds(&mut self) {
        self.fd_table.clear();
    }

    /// 关闭所有文件描述符并清理文件锁（进程退出时调用）
    pub fn close_all_fds_and_cleanup_locks(&mut self, pid: usize) {
        // 清理文件锁
        crate::fs::file_lock_manager().remove_process_locks(pid);
        self.fd_table.clear();
    }

    /// 复制文件描述符（用于 dup 系统调用）
    pub fn dup_fd(&mut self, fd: usize) -> Option<usize> {
        // 语义修正：dup 应与 oldfd 共享同一个“打开文件描述”（open file description），
        // 包括共享偏移、标志等。这里直接克隆 Arc 引用，而不是新建一个 FileDescriptor。
        self.fd_table
            .get(&fd)
            .cloned()
            .and_then(|shared_desc| self.alloc_fd(shared_desc))
    }

    /// 复制文件描述符到指定的文件描述符号（用于 dup2 系统调用）
    pub fn dup2_fd(&mut self, oldfd: usize, newfd: usize) -> Option<usize> {
        // 如果 oldfd 和 newfd 相同，则直接返回 newfd（如果 oldfd 有效）
        if oldfd == newfd {
            return if self.fd_table.contains_key(&oldfd) {
                Some(newfd)
            } else {
                None
            };
        }

        // 获取 oldfd 的共享文件描述符（open file description）
        let shared_desc = match self.fd_table.get(&oldfd).cloned() {
            Some(desc) => desc,
            None => return None,
        };

        // 如果 newfd 已存在，先关闭它
        if self.fd_table.contains_key(&newfd) {
            self.fd_table.remove(&newfd);
        }

        // 共享相同的 FileDescriptor（共享偏移与标志）
        self.fd_table.insert(newfd, shared_desc);

        // 更新 next_fd 以避免与新分配的 fd 冲突
        if newfd >= self.next_fd {
            self.next_fd = newfd + 1;
        }

        Some(newfd)
    }
}

#[derive(Debug)]
pub struct Sched {
    /// nice值 (-20到19, 影响动态优先级计算)
    pub nice: i32,
    /// 累计运行时间 (用于CFS调度算法)
    pub vruntime: u64,
    /// 进程优先级 (0-139, 0最高优先级，139最低优先级)
    pub priority: i32,
    /// 动态时间片大小 (微秒)
    pub time_slice: u64,
}

impl Sched {
    /// 计算动态优先级 (基于nice值)
    pub fn get_dynamic_priority(&self) -> i32 {
        // Linux-like priority calculation: priority = 20 + nice
        // 范围: 0-39 (nice: -20到19)
        (20 + self.nice).max(0).min(39)
    }

    /// 更新虚拟运行时间 (CFS算法核心)
    pub fn update_vruntime(&mut self, runtime_us: u64) {
        // 根据优先级调整权重，优先级越高权重越大，vruntime增长越慢
        let weight = match self.get_dynamic_priority() {
            0..=9 => 4,   // 高优先级
            10..=19 => 2, // 中等优先级
            20..=29 => 1, // 默认优先级
            _ => 1,       // 低优先级
        };
        self.vruntime += runtime_us / weight;
    }

    /// 计算时间片大小 (基于优先级)
    pub fn calculate_time_slice(&self) -> u64 {
        // 基础时间片为10ms，根据优先级调整
        let base_slice = 10000; // 10ms in microseconds
        let priority = self.get_dynamic_priority();

        match priority {
            0..=9 => base_slice * 2,       // 高优先级：20ms
            10..=19 => base_slice * 3 / 2, // 中等优先级：15ms
            20..=29 => base_slice,         // 默认优先级：10ms
            _ => base_slice / 2,           // 低优先级：5ms
        }
    }

    /// 设置nice值并更新优先级
    pub fn set_nice(&mut self, nice: i32) {
        self.nice = nice.max(-20).min(19);
        self.priority = self.get_dynamic_priority();
        self.time_slice = self.calculate_time_slice();
    }
}

/// Task Control block structure
pub struct TaskControlBlock {
    name: Mutex<String>,

    pid: PidHandle,
    /// 进程状态
    pub task_status: Mutex<TaskStatus>,

    pub mm: Memory,
    pub file: Arc<Mutex<File>>,
    pub sched: Mutex<Sched>,
    /// 信号状态
    pub signal_state: Mutex<SignalState>,

    /// 应用数据仅有可能出现在应用地址空间低于 base_size 字节的区域中。
    /// 借助它我们可以清楚的知道应用有多少数据驻留在内存中。
    base_size: usize,
    /// 父进程, 子进程有可能在父进程退出时还存活，因此需要弱引用
    parent: Mutex<Option<Weak<TaskControlBlock>>>,
    /// 子进程
    pub children: Mutex<Vec<Arc<TaskControlBlock>>>,
    /// 子进程退出时，父进程可以获取其退出码
    exit_code: AtomicI32,
    /// 当前工作目录
    pub cwd: Mutex<String>,

    /// 上次运行时的时间戳
    pub last_runtime: AtomicU64,
    /// 总CPU运行时间（微秒）
    pub total_cpu_time: AtomicU64,
    /// 用户态CPU时间（微秒）
    pub user_cpu_time: AtomicU64,
    /// 系统态CPU时间（微秒）
    pub kernel_cpu_time: AtomicU64,
    /// 进程创建时间戳（微秒）
    pub creation_time: AtomicU64,
    /// 进入内核态的时间戳（用于区分用户态/内核态时间）
    pub kernel_enter_time: AtomicU64,
    /// 是否在内核态运行
    pub in_kernel_mode: spin::Mutex<bool>,

    /// 用户ID
    pub uid: AtomicU32,
    /// 组ID
    pub gid: AtomicU32,
    /// 有效用户ID (用于权限检查)
    pub euid: AtomicU32,
    /// 有效组ID (用于权限检查)
    pub egid: AtomicU32,

    /// stdin 非阻塞标志 (用于 fcntl 设置)
    pub stdin_nonblock: AtomicBool,

    /// 睡眠唤醒时间（纳秒），0表示不在睡眠中
    pub wake_time_ns: AtomicU64,

    /// 被停止前的状态（用于SIGCONT恢复）
    pub prev_status_before_stop: Mutex<Option<TaskStatus>>,

    /// 进程启动时的命令行参数
    pub args: Mutex<Option<Vec<String>>>,
    /// 进程启动时的环境变量
    pub envs: Mutex<Option<Vec<String>>>,

    /// 线程组ID（等于进程主线程的PID）
    pub tgid: AtomicUsize,
    /// 当前线程所占用的 TrapContext 槽位索引
    pub thread_slot: AtomicUsize,
    /// 线程槽位位图（仅主线程拥有，其他线程共享引用）
    pub thread_slots: alloc::sync::Arc<Mutex<[bool; MAX_THREADS_PER_PROCESS]>>,
}

impl TaskControlBlock {
    pub fn new(name: &str, elf_data: &[u8]) -> Result<Self, Box<dyn Error>> {
        Self::new_with_pid(name, elf_data, alloc_pid())
    }

    pub fn new_with_pid(
        name: &str,
        elf_data: &[u8],
        pid: PidHandle,
    ) -> Result<Self, Box<dyn Error>> {
        let (memory_set, user_sp, entry_point) = MemorySet::from_elf(elf_data)?;
        let task_status = TaskStatus::Ready;
        let kernel_stack = KernelStack::new();
        let kernel_stack_top = kernel_stack.get_top();
        let trap_cx_va = TRAP_CONTEXT;
        let pid_raw = pid.raw();

        let mut tcb = Self {
            name: Mutex::new(name.to_string()),
            pid,
            task_status: Mutex::new(TaskStatus::Ready),
            mm: Memory {
                memory_set: alloc::sync::Arc::new(Mutex::new(memory_set)),
                kernel_stack,
                trap_cx_va: Mutex::new(trap_cx_va),
                heap_base: AtomicUsize::new(user_sp),
                heap_top: AtomicUsize::new(0),
                task_cx: Mutex::new(TaskContext::goto_trap_return(kernel_stack_top)),
            },
            file: Arc::new(Mutex::new(File {
                fd_table: BTreeMap::new(),
                next_fd: 3,
            })),
            sched: Mutex::new(Sched {
                nice: 0,
                vruntime: 0,
                priority: 20,
                time_slice: 10000,
            }),
            signal_state: Mutex::new(SignalState::new()),
            base_size: user_sp,
            parent: Mutex::new(None),
            children: Mutex::new(Vec::new()),
            exit_code: AtomicI32::new(0),
            cwd: Mutex::new("/".to_string()), // 新进程默认工作目录为根目录
            last_runtime: AtomicU64::new(0),
            total_cpu_time: AtomicU64::new(0),
            user_cpu_time: AtomicU64::new(0),
            kernel_cpu_time: AtomicU64::new(0),
            creation_time: AtomicU64::new(crate::timer::get_time_us()),
            kernel_enter_time: AtomicU64::new(0),
            in_kernel_mode: spin::Mutex::new(false),
            args: Mutex::new(None), // 初始化空的参数列表
            envs: Mutex::new(None), // 初始化空的环境变量列表
            uid: AtomicU32::new(0),
            gid: AtomicU32::new(0),
            euid: AtomicU32::new(0),
            egid: AtomicU32::new(0),
            stdin_nonblock: AtomicBool::new(false),
            wake_time_ns: AtomicU64::new(0),
            prev_status_before_stop: Mutex::new(None),
            tgid: AtomicUsize::new(pid_raw),
            thread_slot: AtomicUsize::new(MAX_THREADS_PER_PROCESS - 1),
            thread_slots: alloc::sync::Arc::new(Mutex::new([false; MAX_THREADS_PER_PROCESS])),
        };

        // prepare TrapContext in user space
        tcb.mm.set_trap_context(TrapContext::app_init_context(
            entry_point,
            user_sp,
            KERNEL_SPACE.wait().lock().token(),
            kernel_stack_top,
            trap_handler as usize,
        ));
        Ok(tcb)
    }

    pub fn exec(&self, name: &str, elf_data: &[u8]) -> Result<(), Box<dyn Error>> {
        self.exec_with_args(name, elf_data, None, None)
    }

    /// Execute a new program with arguments and environment variables
    pub fn exec_with_args(
        &self,
        name: &str,
        elf_data: &[u8],
        args: Option<&[String]>,
        envs: Option<&[String]>,
    ) -> Result<(), Box<dyn Error>> {
        let (memory_set, user_stack_top, entrypoint) = MemorySet::from_elf(elf_data)?;

        let kernel_stack_top = self.mm.kernel_stack.get_top();

        *self.mm.memory_set.lock() = memory_set;
        self.mm.set_trap_context_va(TRAP_CONTEXT);
        self.mm.set_trap_context(TrapContext::app_init_context(
            entrypoint,
            user_stack_top,
            KERNEL_SPACE.wait().lock().token(),
            kernel_stack_top,
            trap_handler as usize,
        ));
        *self.name.lock() = name.to_string();

        // 重置信号状态（exec时应该重置信号处理器）
        self.signal_state.lock().reset_for_exec();

        // 保存命令行参数和环境变量
        *self.args.lock() = args.map(|args| args.to_vec());
        *self.envs.lock() = envs.map(|envs| envs.to_vec());

        Ok(())
    }

    pub fn fork(self: &Arc<Self>) -> Result<Arc<Self>, crate::memory::mm::MemoryError> {
        let memory_set = MemorySet::form_existed_user(&self.mm.memory_set.lock())?;
        let trap_cx_va = TRAP_CONTEXT;

        // alloc a pid and a kernel stack in kernel space
        let pid = alloc_pid();
        let kernel_stack = KernelStack::new();
        let kernel_stack_top = kernel_stack.get_top();
        let file = {
            let file = self.file.lock();
            Arc::new(Mutex::new(File {
                fd_table: file.fd_table.clone(),
                next_fd: file.next_fd,
            }))
        };
        let sched = {
            let sched = self.sched.lock();
            Mutex::new(Sched {
                nice: sched.nice,
                vruntime: 0,
                priority: sched.priority,
                time_slice: sched.time_slice,
            })
        };

        let tcb = Arc::new(Self {
            name: Mutex::new(self.name.lock().clone()),
            pid,
            task_status: Mutex::new(TaskStatus::Ready),
            base_size: self.base_size,
            parent: Mutex::new(Some(Arc::downgrade(self))),
            children: Mutex::new(Vec::new()),
            exit_code: AtomicI32::new(0),
            cwd: Mutex::new(self.cwd.lock().clone()),
            last_runtime: AtomicU64::new(0),
            total_cpu_time: AtomicU64::new(0),
            user_cpu_time: AtomicU64::new(0),
            kernel_cpu_time: AtomicU64::new(0),
            creation_time: AtomicU64::new(crate::timer::get_time_us()),
            kernel_enter_time: AtomicU64::new(0),
            in_kernel_mode: spin::Mutex::new(false),
            uid: AtomicU32::new(self.uid.load(atomic::Ordering::Relaxed)),
            gid: AtomicU32::new(self.gid.load(atomic::Ordering::Relaxed)),
            euid: AtomicU32::new(self.euid.load(atomic::Ordering::Relaxed)),
            egid: AtomicU32::new(self.egid.load(atomic::Ordering::Relaxed)),
            stdin_nonblock: AtomicBool::new(self.stdin_nonblock.load(atomic::Ordering::Relaxed)),
            wake_time_ns: AtomicU64::new(0),
            prev_status_before_stop: Mutex::new(None),
            args: Mutex::new(self.args.lock().clone()),
            envs: Mutex::new(self.envs.lock().clone()),
            mm: Memory {
                memory_set: alloc::sync::Arc::new(Mutex::new(memory_set)),
                kernel_stack,
                trap_cx_va: Mutex::new(trap_cx_va),
                heap_base: AtomicUsize::new(self.mm.heap_base.load(atomic::Ordering::Relaxed)),
                heap_top: AtomicUsize::new(self.mm.heap_top.load(atomic::Ordering::Relaxed)),
                task_cx: Mutex::new(TaskContext::goto_trap_return(kernel_stack_top)),
            },
            file,
            sched,
            signal_state: Mutex::new(self.signal_state.lock().clone_for_fork()),
            tgid: AtomicUsize::new(self.tgid()),
            thread_slot: AtomicUsize::new(MAX_THREADS_PER_PROCESS - 1),
            thread_slots: alloc::sync::Arc::new(Mutex::new([false; MAX_THREADS_PER_PROCESS])),
        });

        self.children.lock().push(tcb.clone());
        tcb.mm.trap_context().kernel_sp = kernel_stack_top;
        Ok(tcb)
    }

    /// 线程组ID（进程ID）
    pub fn tgid(&self) -> usize {
        self.tgid.load(atomic::Ordering::Relaxed)
    }

    /// 获取当前线程TrapContext虚拟地址
    pub fn trap_context_va(&self) -> usize {
        *self.mm.trap_cx_va.lock()
    }

    /// 在当前进程内创建线程
    /// entry: 用户函数入口地址；user_sp: 线程用户栈顶；arg: 传入a0
    pub fn spawn_thread(self: &Arc<Self>, entry: usize, user_sp: usize, arg: usize) -> Result<Arc<Self>, Box<dyn Error>> {
        // 分配线程槽位
        let slot = {
            let mut slots = self.thread_slots.lock();
            // 主线程占用最高槽位（MAX-1），线程从低位开始分配
            let mut found = None;
            for i in 0..(MAX_THREADS_PER_PROCESS - 1) {
                if !slots[i] {
                    slots[i] = true;
                    found = Some(i);
                    break;
                }
            }
            found.ok_or("No free thread slot")?
        };

        let pid = alloc_pid();
        let kernel_stack = KernelStack::new();
        let kernel_stack_top = kernel_stack.get_top();
        let trap_cx_va = TRAP_CONTEXT_BASE + slot * PAGE_SIZE;

        let tcb = Arc::new(Self {
            name: Mutex::new(self.name.lock().clone()),
            pid,
            task_status: Mutex::new(TaskStatus::Ready),
            base_size: self.base_size,
            parent: Mutex::new(Some(Arc::downgrade(self))),
            children: Mutex::new(Vec::new()),
            exit_code: AtomicI32::new(0),
            cwd: Mutex::new(self.cwd.lock().clone()),
            last_runtime: AtomicU64::new(0),
            total_cpu_time: AtomicU64::new(0),
            user_cpu_time: AtomicU64::new(0),
            kernel_cpu_time: AtomicU64::new(0),
            creation_time: AtomicU64::new(crate::timer::get_time_us()),
            kernel_enter_time: AtomicU64::new(0),
            in_kernel_mode: spin::Mutex::new(false),
            uid: AtomicU32::new(self.uid.load(atomic::Ordering::Relaxed)),
            gid: AtomicU32::new(self.gid.load(atomic::Ordering::Relaxed)),
            euid: AtomicU32::new(self.euid.load(atomic::Ordering::Relaxed)),
            egid: AtomicU32::new(self.egid.load(atomic::Ordering::Relaxed)),
            stdin_nonblock: AtomicBool::new(self.stdin_nonblock.load(atomic::Ordering::Relaxed)),
            wake_time_ns: AtomicU64::new(0),
            prev_status_before_stop: Mutex::new(None),
            args: Mutex::new(self.args.lock().clone()),
            envs: Mutex::new(self.envs.lock().clone()),
            mm: Memory {
                memory_set: self.mm.memory_set.clone(),
                kernel_stack,
                trap_cx_va: Mutex::new(trap_cx_va),
                heap_base: AtomicUsize::new(self.mm.heap_base.load(atomic::Ordering::Relaxed)),
                heap_top: AtomicUsize::new(self.mm.heap_top.load(atomic::Ordering::Relaxed)),
                task_cx: Mutex::new(TaskContext::goto_trap_return(kernel_stack_top)),
            },
            file: self.file.clone(),
            // 注意：不能在同一表达式里多次 self.sched.lock()，否则自旋锁可能因临时值生命周期导致重入死锁。
            // 这里一次性获取父任务的调度信息，避免重复加锁。
            sched: {
                let parent_sched = self.sched.lock();
                Mutex::new(Sched {
                    nice: parent_sched.nice,
                    vruntime: 0,
                    priority: parent_sched.priority,
                    time_slice: parent_sched.time_slice,
                })
            },
            signal_state: Mutex::new(SignalState::new()),
            tgid: AtomicUsize::new(self.tgid()),
            thread_slot: AtomicUsize::new(slot),
            thread_slots: self.thread_slots.clone(),
        });

        // 初始化线程TrapContext
        tcb.mm.set_trap_context(TrapContext::app_init_context(
            entry,
            user_sp,
            KERNEL_SPACE.wait().lock().token(),
            kernel_stack_top,
            trap_handler as usize,
        ));
        // a0 = arg
        tcb.mm.trap_context().x[10] = arg;

        Ok(tcb)
    }

    pub fn name(&self) -> String {
        self.name.lock().clone()
    }

    pub fn is_zombie(&self) -> bool {
        *self.task_status.lock() == TaskStatus::Zombie
    }

    pub fn is_ready(&self) -> bool {
        *self.task_status.lock() == TaskStatus::Ready
    }

    /// 获取用户ID
    pub fn uid(&self) -> u32 {
        self.uid.load(atomic::Ordering::Relaxed)
    }

    /// 获取组ID
    pub fn gid(&self) -> u32 {
        self.gid.load(atomic::Ordering::Relaxed)
    }

    /// 获取有效用户ID
    pub fn euid(&self) -> u32 {
        self.euid.load(atomic::Ordering::Relaxed)
    }

    /// 获取有效组ID
    pub fn egid(&self) -> u32 {
        self.egid.load(atomic::Ordering::Relaxed)
    }

    /// 设置用户ID (需要root权限)
    pub fn set_uid(&self, uid: u32) -> Result<(), i32> {
        // 只有root用户可以设置任意UID
        if self.euid.load(atomic::Ordering::Relaxed) != 0
            && self.euid.load(atomic::Ordering::Relaxed) != uid
        {
            return Err(-1); // EPERM
        }
        self.uid.store(uid, atomic::Ordering::Relaxed);
        self.euid.store(uid, atomic::Ordering::Relaxed);
        Ok(())
    }

    /// 设置组ID (需要root权限)
    pub fn set_gid(&self, gid: u32) -> Result<(), i32> {
        // 只有root用户可以设置任意GID
        if self.euid.load(atomic::Ordering::Relaxed) != 0
            && self.egid.load(atomic::Ordering::Relaxed) != gid
        {
            return Err(-1); // EPERM
        }
        self.gid.store(gid, atomic::Ordering::Relaxed);
        self.egid.store(gid, atomic::Ordering::Relaxed);
        Ok(())
    }

    /// 设置有效用户ID
    pub fn set_euid(&self, euid: u32) -> Result<(), i32> {
        // 只有root用户或设置为实际UID才允许
        if self.euid.load(atomic::Ordering::Relaxed) != 0
            && euid != self.uid.load(atomic::Ordering::Relaxed)
        {
            return Err(-1); // EPERM
        }
        self.euid.store(euid, atomic::Ordering::Relaxed);
        Ok(())
    }

    /// 设置有效组ID
    pub fn set_egid(&self, egid: u32) -> Result<(), i32> {
        // 只有root用户或设置为实际GID才允许
        if self.euid.load(atomic::Ordering::Relaxed) != 0
            && egid != self.gid.load(atomic::Ordering::Relaxed)
        {
            return Err(-1); // EPERM
        }
        self.egid.store(egid, atomic::Ordering::Relaxed);
        Ok(())
    }

    /// 检查是否为root用户
    pub fn is_root(&self) -> bool {
        self.euid.load(atomic::Ordering::Relaxed) == 0
    }

    /// 检查对文件的访问权限
    pub fn check_file_permission(
        &self,
        file_mode: u32,
        file_uid: u32,
        file_gid: u32,
        requested: u32,
    ) -> bool {
        // root用户拥有所有权限
        if self.euid.load(atomic::Ordering::Relaxed) == 0 {
            return true;
        }

        let mut effective_mode = 0;

        // 检查用户权限
        if self.euid.load(atomic::Ordering::Relaxed) == file_uid {
            effective_mode = (file_mode >> 6) & 0o7; // 用户权限位
        }
        // 检查组权限
        else if self.egid.load(atomic::Ordering::Relaxed) == file_gid {
            effective_mode = (file_mode >> 3) & 0o7; // 组权限位
        }
        // 其他用户权限
        else {
            effective_mode = file_mode & 0o7; // 其他用户权限位
        }

        (effective_mode & requested) == requested
    }

    pub fn pid(&self) -> usize {
        self.pid.0
    }

    pub fn set_exit_code(&self, exit_code: i32) {
        self.exit_code.store(exit_code, atomic::Ordering::Relaxed);
    }

    pub fn exit_code(&self) -> i32 {
        self.exit_code.load(atomic::Ordering::Relaxed)
    }

    pub fn set_parent(&self, parent: Weak<TaskControlBlock>) {
        *self.parent.lock() = Some(parent);
    }

    pub fn parent(&self) -> Option<Arc<TaskControlBlock>> {
        self.parent.lock().as_ref().and_then(|w| w.upgrade())
    }

    pub fn wakeup(self: &Arc<Self>) {
        let current_status = *self.task_status.lock();
        if current_status == TaskStatus::Sleeping || current_status == TaskStatus::Stopped {
            *self.task_status.lock() = TaskStatus::Ready;
            add_task(self.clone());
        }
    }
}

impl core::fmt::Debug for TaskControlBlock {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            r#"
            TaskControlBlock {{
                pid: {},
                name: {},
                parent: {:?},
                children: {:?},
                exit_code: {},
                task_status: {:?}
            }}"#,
            self.pid(),
            self.name(),
            self.parent().map(|parent| parent.name()),
            self.children
                .lock()
                .iter()
                .collect::<Vec<_>>(),
            self.exit_code(),
            self.task_status.lock()
        )
    }
}
