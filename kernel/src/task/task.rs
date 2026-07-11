use core::{
    error::Error,
    sync::atomic::{self, AtomicU64, AtomicUsize},
};

use alloc::{
    boxed::Box,
    collections::BTreeMap,
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};
use spin::Mutex;

use crate::{
    fs::inode::Inode,
    memory::{
        KERNEL_SPACE, TRAP_CONTEXT,
        address::VirtualAddress,
        kernel_stack::KernelStack,
        mm::{self, MemorySet, UserAccessError},
    },
    signal::SignalState,
    sync::IrqMutex,
    task::{context::TaskContext, pid::PidHandle, task_manager::set_task_status},
    trap::{TrapContext, trap_handler},
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
    /// 用户态的 TaskContext
    pub task_cx: Mutex<TaskContext>,
}

impl Memory {
    /// @description 覆盖 supervisor-only trap context 页中的保存上下文。
    ///
    /// @param trap_context 待写入的完整上下文值。
    /// @return 无返回值；trap context 映射缺失表示 kernel 内部不变量损坏并 panic。
    pub fn set_trap_context(&self, trap_context: TrapContext) {
        let va = *self.trap_cx_va.lock();
        let memory_set = self.memory_set.lock();
        let ppn = memory_set.trap_context_ppn(va);
        let offset = VirtualAddress::from(va).page_offset();
        assert!(offset + core::mem::size_of::<TrapContext>() <= crate::memory::PAGE_SIZE);
        let ptr = unsafe { ppn.as_page_mut_ptr().add(offset).cast::<TrapContext>() };
        assert!(
            ptr.is_aligned(),
            "TrapContext physical address is not aligned"
        );
        // SAFETY: memory_set guard 保证映射和 FrameTracker 在写入期间存活；trap context
        // 位于单页 supervisor-only framed area，当前 task 是该上下文的唯一软件写者。
        unsafe { ptr.write(trap_context) };
    }

    /// @description 复制 supervisor-only trap context，禁止底层用户映射引用逃逸锁。
    ///
    /// @return 当前完整 trap context 的 owned clone；映射缺失表示 kernel 内部不变量损坏并 panic。
    pub fn load_trap_context(&self) -> TrapContext {
        let va = *self.trap_cx_va.lock();
        let memory_set = self.memory_set.lock();
        let ppn = memory_set.trap_context_ppn(va);
        let offset = VirtualAddress::from(va).page_offset();
        assert!(offset + core::mem::size_of::<TrapContext>() <= crate::memory::PAGE_SIZE);
        let ptr = unsafe { ppn.as_page_ptr().add(offset).cast::<TrapContext>() };
        assert!(
            ptr.is_aligned(),
            "TrapContext physical address is not aligned"
        );
        // SAFETY: memory_set guard 保证映射和 FrameTracker 存活；只读引用仅用于本行 clone，
        // 不会离开 guard 生命周期，TrapContext 已由 set_trap_context 完整初始化。
        unsafe { (&*ptr).clone() }
    }

    /// @description 从用户地址空间复制字节到 kernel 缓冲区，地址空间锁覆盖整个复制。
    ///
    /// @param user_address 用户源地址。
    /// @param destination kernel 目标缓冲区。
    /// @return 完整成功返回 `Ok(())`；fault、权限错误或 overflow 返回 `UserAccessError`。
    pub fn copy_from_user(
        &self,
        user_address: usize,
        destination: &mut [u8],
    ) -> Result<(), UserAccessError> {
        self.memory_set
            .lock()
            .copy_from_user(user_address, destination)
    }

    /// @description 将 kernel 缓冲区复制到用户地址空间，地址空间锁覆盖整个复制。
    ///
    /// @param user_address 用户目标地址。
    /// @param source kernel 源缓冲区。
    /// @return 完整成功返回 `Ok(())`；fault、权限错误或 overflow 返回 `UserAccessError`。
    pub fn copy_to_user(&self, user_address: usize, source: &[u8]) -> Result<(), UserAccessError> {
        self.memory_set.lock().copy_to_user(user_address, source)
    }

    /// @description 从用户空间复制有上限的 NUL 结尾 UTF-8 字符串。
    ///
    /// @param user_address 用户字符串首地址。
    /// @param max_len 不含 NUL 的最大字节数。
    /// @return 成功返回 owned 字符串；fault、未终止或非法 UTF-8 返回明确错误。
    pub fn copy_user_string(
        &self,
        user_address: usize,
        max_len: usize,
    ) -> Result<String, UserAccessError> {
        self.memory_set
            .lock()
            .copy_user_string(user_address, max_len)
    }

    /// @description 检查用户指令地址是否具有 U-mode execute 权限。
    ///
    /// @param user_address 待检查地址。
    /// @return `U|X` leaf 映射存在时返回 `true`。
    pub fn is_user_executable(&self, user_address: usize) -> bool {
        self.memory_set.lock().is_user_executable(user_address)
    }

    /// @description 检查完整用户地址范围是否可由 kernel copyout。
    ///
    /// @param user_address 用户目标地址。
    /// @param len 待写长度。
    /// @return 完整 `U|W` 范围存在时返回 `true`。
    pub fn is_user_writable(&self, user_address: usize, len: usize) -> bool {
        self.memory_set.lock().is_user_writable(user_address, len)
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
            error!(
                "[FD_ALLOC] CRITICAL: FD table full! {} open files (max {})",
                self.fd_table.len(),
                MAX_FD_COUNT
            );
            return None; // 达到上限，返回None表示分配失败
        }

        // Log milestone FDs to track progress
        // 寻找下一个可用的文件描述符
        let mut fd = self.next_fd;
        let mut search_count = 0;
        while self.fd_table.contains_key(&fd) {
            fd += 1;
            search_count += 1;
            // 修复：检查搜索次数而不是FD数值，防止FD编号大于MAX_FD_COUNT时错误退出
            // MAX_FD_COUNT是文件表大小限制，不是FD编号限制
            if search_count >= MAX_FD_COUNT {
                error!(
                    "[FD_ALLOC] CRITICAL: FD search exhausted after {} attempts! Table has {} entries",
                    search_count,
                    self.fd_table.len()
                );
                return None; // 防止无限循环
            }
        }

        // Log if search took a long time
        if search_count > 100 {
            warn!(
                "[FD_ALLOC] Slow FD search: {} attempts to find FD {} (table fragmented?)",
                search_count, fd
            );
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

    /// 关闭标记了O_CLOEXEC的文件描述符（execve时调用）
    pub fn close_cloexec_fds(&mut self) {
        const O_CLOEXEC: u32 = 0o2000000;
        let mut fds_to_close = Vec::new();

        // 收集需要关闭的文件描述符
        for (&fd, file_desc) in &self.fd_table {
            if (file_desc.flags & O_CLOEXEC) != 0 {
                fds_to_close.push(fd);
            }
        }

        // 关闭标记了O_CLOEXEC的文件描述符
        for fd in fds_to_close {
            self.fd_table.remove(&fd);
        }
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
}

#[derive(Debug)]
pub struct Sched {
    /// 本次运行开始的 monotonic 时间，只在 sched mutex 内访问。
    pub last_runtime: u64,
    /// nice值 (-20到19, 影响动态优先级计算)
    pub nice: i32,
    /// 累计运行时间 (用于CFS调度算法)
    pub vruntime: u64,
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
}

/// Task Control block structure
pub struct TaskControlBlock {
    name: Mutex<String>,

    pid: PidHandle,
    /// 进程状态
    // timer softirq 与 task context 共同转换该状态；普通 spin lock 会在同 hart 再入时死锁。
    pub task_status: IrqMutex<TaskStatus>,

    pub mm: Memory,
    pub file: Arc<Mutex<File>>,
    pub sched: Mutex<Sched>,
    /// 信号状态
    pub signal_state: Mutex<SignalState>,

    /// 任务退出状态
    exit_code: Mutex<i32>,
    /// 当前工作目录
    pub cwd: Mutex<String>,

    /// 只作为下次 CPU 选择的亲和性 hint，不发布 task 状态。
    pub last_cpu: AtomicUsize,

    /// 当前最小 credentials 状态；uid/euid 必须在同一临界区检查并更新。
    credentials: Mutex<Credentials>,

    /// 睡眠唤醒时间（纳秒），0表示不在睡眠中
    pub wake_time_ns: AtomicU64,

    /// 被停止前的状态（用于SIGCONT恢复）
    pub prev_status_before_stop: Mutex<Option<TaskStatus>>,
}

struct Credentials {
    uid: u32,
    euid: u32,
}

impl TaskControlBlock {
    pub fn new_with_pid(
        name: &str,
        elf_data: &[u8],
        pid: PidHandle,
    ) -> Result<Self, Box<dyn Error>> {
        let (memory_set, user_sp, entry_point) = MemorySet::from_elf(elf_data)?;
        let kernel_stack = KernelStack::new();
        let kernel_stack_top = kernel_stack.get_top();
        let trap_cx_va = TRAP_CONTEXT;
        let tcb = Self {
            name: Mutex::new(name.to_string()),
            pid,
            task_status: IrqMutex::new(TaskStatus::Ready),
            mm: Memory {
                memory_set: alloc::sync::Arc::new(Mutex::new(memory_set)),
                kernel_stack,
                trap_cx_va: Mutex::new(trap_cx_va),
                task_cx: Mutex::new(TaskContext::goto_trap_return(kernel_stack_top)),
            },
            file: Arc::new(Mutex::new(File {
                fd_table: BTreeMap::new(),
                next_fd: 3,
            })),
            sched: Mutex::new(Sched {
                last_runtime: 0,
                nice: 0,
                vruntime: 0,
            }),
            signal_state: Mutex::new(SignalState::new()),
            exit_code: Mutex::new(0),
            cwd: Mutex::new("/".to_string()), // 新进程默认工作目录为根目录
            last_cpu: AtomicUsize::new(0),
            credentials: Mutex::new(Credentials { uid: 0, euid: 0 }),
            wake_time_ns: AtomicU64::new(0),
            prev_status_before_stop: Mutex::new(None),
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

    /// 获取当前线程TrapContext虚拟地址
    pub fn trap_context_va(&self) -> usize {
        *self.mm.trap_cx_va.lock()
    }

    /// execve_replace - Linux标准的execve实现
    ///
    /// 完全替换当前进程的内存映像，按照POSIX标准：
    /// - 保留PID、PPID、会话ID、进程组ID
    /// - 关闭标记了O_CLOEXEC的文件描述符  
    /// - 重置信号处理器为默认状态
    /// - 重置地址空间和程序状态
    /// - 成功时不返回（进程被新程序替换）
    pub fn execve_replace(
        self: &Arc<Self>,
        program_name: &str,
        elf_data: &[u8],
        args: &[String],
        envs: &[String],
    ) -> Result<(), crate::memory::mm::MemoryError> {
        // 步骤1: 创建新的内存空间 - 在完全提交之前先准备好
        let classify_load_error = |error: Box<dyn Error>| {
            error
                .downcast_ref::<crate::memory::mm::MemoryError>()
                .copied()
                .unwrap_or(crate::memory::mm::MemoryError::InvalidRange)
        };
        let (new_memory_set, user_sp, entry_point) = if args.is_empty() && envs.is_empty() {
            MemorySet::from_elf(elf_data).map_err(classify_load_error)?
        } else {
            MemorySet::from_elf_with_args(elf_data, args, envs).map_err(classify_load_error)?
        };

        // 步骤2: 关闭标记了O_CLOEXEC的文件描述符
        // 这必须在更换内存空间之前完成，以确保能够正确访问文件描述符表
        {
            let mut file_table = self.file.lock();
            file_table.close_cloexec_fds();
        }

        // 步骤3: 重置信号处理器为默认状态
        {
            let mut signal_state = self.signal_state.lock();
            signal_state.reset_to_default();
        }

        // 步骤4: 替换内存管理结构
        // 这是关键步骤 - 完全替换当前进程的地址空间
        let kernel_stack_top = self.mm.kernel_stack.get_top();

        // 单次赋值提交新地址空间；旧 MemorySet 在 guard 内被完整替换，不暴露 stale PTE 窗口。
        *self.mm.memory_set.lock() = new_memory_set;
        *self.mm.trap_cx_va.lock() = TRAP_CONTEXT;

        // 步骤5: 更新任务状态；参数与环境只存在于新初始栈中。
        *self.name.lock() = program_name.to_string();

        // 步骤6: 设置新程序的陷阱上下文
        self.mm.set_trap_context(TrapContext::app_init_context(
            entry_point,
            user_sp,
            KERNEL_SPACE.wait().lock().token(),
            kernel_stack_top,
            trap_handler as usize,
        ));

        // 地址空间由统一的 trap 返回路径激活；在这里切换会让后续内核代码运行在用户页表上。
        Ok(())
    }

    pub fn name(&self) -> String {
        self.name.lock().clone()
    }

    pub fn is_zombie(&self) -> bool {
        *self.task_status.lock() == TaskStatus::Zombie
    }

    /// 设置用户ID (需要root权限)
    pub fn set_uid(&self, uid: u32) -> Result<(), i32> {
        let mut credentials = self.credentials.lock();
        // 只有root用户可以设置任意UID
        if credentials.euid != 0 && credentials.euid != uid {
            return Err(-1); // EPERM
        }
        credentials.uid = uid;
        credentials.euid = uid;
        Ok(())
    }

    pub fn pid(&self) -> usize {
        self.pid.0
    }

    pub fn set_exit_code(&self, exit_code: i32) {
        *self.exit_code.lock() = exit_code;
    }

    pub fn exit_code(&self) -> i32 {
        *self.exit_code.lock()
    }

    pub fn wakeup(self: &Arc<Self>) {
        let current_status = *self.task_status.lock();
        if current_status == TaskStatus::Sleeping || current_status == TaskStatus::Stopped {
            set_task_status(self, TaskStatus::Ready);
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
                exit_code: {},
                task_status: {:?}
            }}"#,
            self.pid(),
            self.name(),
            self.exit_code(),
            self.task_status.lock()
        )
    }
}
