use core::sync::atomic::AtomicUsize;

use alloc::{
    string::{String, ToString},
    vec::Vec,
};
use spin::Mutex;

use crate::{
    fs::{Console, FileDescriptorTable, OpenFileDescription},
    memory::{
        ElfLoadError, KERNEL_SPACE, KernelStack, MemoryError, MemorySet, TRAP_CONTEXT,
        UserAccessError, VirtualAddress,
    },
    sync::IrqMutex,
    task::{TrapContext, context::TaskContext, pid::ProcessId},
};

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum RunState {
    New,
    Ready { cpu: usize, generation: u64 },
    Running { cpu: usize },
    Blocking { cpu: usize },
    Blocked,
    WakePending { cpu: usize },
    Exited,
}

#[derive(Debug)]
struct AddressSpace {
    memory_set: Mutex<MemorySet>,
}

impl AddressSpace {
    /// @description 从用户地址空间复制字节到 kernel 缓冲区，地址空间锁覆盖整个复制。
    ///
    /// @param user_address 用户源地址。
    /// @param destination kernel 目标缓冲区。
    /// @return 完整成功返回 `Ok(())`；fault、权限错误或 overflow 返回 `UserAccessError`。
    pub(crate) fn copy_from_user(
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
    pub(crate) fn copy_to_user(
        &self,
        user_address: usize,
        source: &[u8],
    ) -> Result<(), UserAccessError> {
        self.memory_set.lock().copy_to_user(user_address, source)
    }

    /// @description 从用户空间复制有上限的 NUL 结尾字节串。
    ///
    /// @param user_address 用户字符串首地址。
    /// @param max_len 包含终止 NUL 的最大总字节数。
    /// @return 成功返回不含 NUL 的 owned bytes；fault、未终止或内存不足返回明确错误。
    pub(crate) fn copy_user_c_string(
        &self,
        user_address: usize,
        max_len: usize,
    ) -> Result<alloc::vec::Vec<u8>, UserAccessError> {
        self.memory_set
            .lock()
            .copy_user_c_string(user_address, max_len)
    }
}

#[derive(Debug)]
struct ThreadContext {
    tid: usize,
    kernel_stack: KernelStack,
    trap_cx_va: Mutex<usize>,
    task_cx: Mutex<TaskContext>,
    kernel_trap_handler: usize,
}

#[derive(Debug)]
pub(crate) struct Sched {
    /// 本次运行开始的 monotonic 时间，只在 sched mutex 内访问。
    pub(crate) last_runtime: u64,
    /// nice值 (-20到19, 影响动态优先级计算)
    pub(crate) nice: i32,
    /// 累计运行时间 (用于CFS调度算法)
    pub(crate) vruntime: u64,
}

/// @description 调度器唯一拥有和解释的 Thread 运行状态。
pub(crate) struct SchedulingEntity {
    // state/generation/wait_key 必须在一个 IRQ-safe 临界区内转换；拆锁会允许重复 enqueue。
    pub(crate) state: IrqMutex<SchedulingState>,
    pub(crate) policy: Mutex<Sched>,
    /// 只作为下次 CPU 选择的亲和性 hint，不发布 task 状态。
    pub(crate) last_cpu: AtomicUsize,
}

/// @description run state、enqueue generation 与 wait membership 的唯一权威。
#[derive(Debug)]
pub(crate) struct SchedulingState {
    pub(crate) run_state: RunState,
    pub(crate) next_generation: u64,
    pub(crate) deadline_wait: Option<(u64, u64)>,
}

impl SchedulingState {
    /// @description 创建新的唯一 Ready generation，并使此前所有 queue entry 失效。
    ///
    /// @param cpu 新 membership 的 owner CPU。
    /// @return 必须随 RunQueueEntry 一起保存的 generation。
    pub(crate) fn transition_to_ready(&mut self, cpu: usize) -> u64 {
        self.next_generation = self.next_generation.wrapping_add(1);
        assert_ne!(self.next_generation, 0, "runqueue generation wrapped");
        let generation = self.next_generation;
        self.run_state = RunState::Ready { cpu, generation };
        generation
    }
}

impl Sched {
    /// 计算动态优先级 (基于nice值)
    pub(crate) fn get_dynamic_priority(&self) -> i32 {
        // Linux-like priority calculation: priority = 20 + nice
        // 范围: 0-39 (nice: -20到19)
        (20 + self.nice).clamp(0, 39)
    }

    /// 更新虚拟运行时间 (CFS算法核心)
    pub(crate) fn update_vruntime(&mut self, runtime_us: u64) {
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

/// @description Process 级资源 owner；当前恰好由一个 Task/Thread 引用。
struct Process {
    tgid: ProcessId,
    address_space: AddressSpace,
    cwd: Mutex<String>,
    files: Mutex<FileDescriptorTable>,
}

/// @description 当前单进程单线程模型的 Process、Thread 与 SchedulingEntity 组合边界。
pub(crate) struct TaskControlBlock {
    process: Process,
    thread: ThreadContext,
    pub(crate) scheduling: SchedulingEntity,
}

impl TaskControlBlock {
    pub(super) fn new_with_pid(
        name: &[u8],
        elf_data: &[u8],
        pid: ProcessId,
        kernel_trap_handler: usize,
        kernel_trap_return: usize,
        console: alloc::sync::Arc<dyn Console>,
    ) -> Result<Self, ElfLoadError> {
        let mut argv0 = Vec::new();
        argv0
            .try_reserve_exact(name.len())
            .map_err(|_| ElfLoadError::OutOfMemory)?;
        argv0.extend_from_slice(name);
        let initial_args = [argv0];
        let (memory_set, user_sp, entry_point) = MemorySet::from_elf(elf_data, &initial_args, &[])?;
        let kernel_stack = KernelStack::new();
        let kernel_stack_top = kernel_stack.get_top();
        let trap_cx_va = TRAP_CONTEXT;
        let tid = pid.0;
        let process = Process {
            tgid: pid,
            address_space: AddressSpace {
                memory_set: Mutex::new(memory_set),
            },
            cwd: Mutex::new("/".to_string()),
            files: Mutex::new(FileDescriptorTable::with_console(console)),
        };
        let tcb = Self {
            process,
            thread: ThreadContext {
                tid,
                kernel_stack,
                trap_cx_va: Mutex::new(trap_cx_va),
                task_cx: Mutex::new(TaskContext::goto_trap_return(
                    kernel_stack_top,
                    kernel_trap_return,
                )),
                kernel_trap_handler,
            },
            scheduling: SchedulingEntity {
                state: IrqMutex::new(SchedulingState {
                    run_state: RunState::New,
                    next_generation: 0,
                    deadline_wait: None,
                }),
                policy: Mutex::new(Sched {
                    last_runtime: 0,
                    nice: 0,
                    vruntime: 0,
                }),
                last_cpu: AtomicUsize::new(0),
            },
        };

        // prepare TrapContext in user space
        tcb.set_trap_context(TrapContext::app_init_context(
            entry_point,
            user_sp,
            KERNEL_SPACE.wait().lock().token(),
            kernel_stack_top,
            kernel_trap_handler,
        ));
        Ok(tcb)
    }

    /// 获取当前线程TrapContext虚拟地址
    pub(crate) fn trap_context_va(&self) -> usize {
        *self.thread.trap_cx_va.lock()
    }

    /// @description 覆盖当前 Thread 的 supervisor-only trap context。
    ///
    /// @param trap_context 待写入的完整上下文值。
    /// @return 无返回值；映射缺失表示 kernel 不变量损坏并 panic。
    pub(crate) fn set_trap_context(&self, trap_context: TrapContext) {
        let va = self.trap_context_va();
        let memory_set = self.process.address_space.memory_set.lock();
        let ppn = memory_set.trap_context_ppn(va);
        let offset = VirtualAddress::from(va).page_offset();
        assert!(offset + core::mem::size_of::<TrapContext>() <= crate::memory::PAGE_SIZE);
        // SAFETY: validated page offset keeps pointer arithmetic inside the live trap-context
        // frame retained by the address-space guard.
        let ptr = unsafe { ppn.as_page_mut_ptr().add(offset).cast::<TrapContext>() };
        assert!(
            ptr.is_aligned(),
            "TrapContext physical address is not aligned"
        );
        // SAFETY: address-space guard 保证映射存活；当前 Thread 是该 trap context 的唯一写者。
        unsafe { ptr.write(trap_context) };
    }

    /// @description 复制当前 Thread trap context，不让底层映射引用逃逸地址空间锁。
    ///
    /// @return owned TrapContext clone；映射缺失表示 kernel 不变量损坏并 panic。
    pub(crate) fn load_trap_context(&self) -> TrapContext {
        let va = self.trap_context_va();
        let memory_set = self.process.address_space.memory_set.lock();
        let ppn = memory_set.trap_context_ppn(va);
        let offset = VirtualAddress::from(va).page_offset();
        assert!(offset + core::mem::size_of::<TrapContext>() <= crate::memory::PAGE_SIZE);
        // SAFETY: validated page offset keeps pointer arithmetic inside the live trap-context
        // frame retained by the address-space guard.
        let ptr = unsafe { ppn.as_page_ptr().add(offset).cast::<TrapContext>() };
        assert!(
            ptr.is_aligned(),
            "TrapContext physical address is not aligned"
        );
        // SAFETY: guard 保证 frame 存活；只读引用仅用于本行 clone 且不会逃逸。
        unsafe { (&*ptr).clone() }
    }

    pub(crate) fn copy_from_user(
        &self,
        user_address: usize,
        destination: &mut [u8],
    ) -> Result<(), UserAccessError> {
        self.process
            .address_space
            .copy_from_user(user_address, destination)
    }

    pub(crate) fn copy_to_user(
        &self,
        user_address: usize,
        source: &[u8],
    ) -> Result<(), UserAccessError> {
        self.process
            .address_space
            .copy_to_user(user_address, source)
    }

    pub(crate) fn copy_user_c_string(
        &self,
        user_address: usize,
        max_len: usize,
    ) -> Result<alloc::vec::Vec<u8>, UserAccessError> {
        self.process
            .address_space
            .copy_user_c_string(user_address, max_len)
    }

    pub(crate) fn user_token(&self) -> usize {
        self.process.address_space.memory_set.lock().token()
    }

    pub(crate) fn set_program_break(&self, new_break: usize) -> Result<usize, MemoryError> {
        self.process
            .address_space
            .memory_set
            .lock()
            .set_program_break(new_break)
    }

    /// @description 取得当前 Thread 的 context-switch 保存区锁。
    ///
    /// @return TaskContext mutex；raw pointer 仅可在 TCB Arc 保活期间使用。
    pub(crate) fn task_context(&self) -> &Mutex<TaskContext> {
        &self.thread.task_cx
    }

    /// @description 复制当前 Process 的工作目录。
    ///
    /// @return owned cwd path。
    pub(crate) fn cwd(&self) -> String {
        self.process.cwd.lock().clone()
    }

    pub(crate) fn fd_get(&self, fd: usize) -> Option<alloc::sync::Arc<OpenFileDescription>> {
        self.process.files.lock().get(fd)
    }

    pub(crate) fn fd_allocate(
        &self,
        ofd: alloc::sync::Arc<OpenFileDescription>,
        cloexec: bool,
    ) -> Result<usize, ()> {
        self.process.files.lock().allocate(ofd, 0, cloexec)
    }

    pub(crate) fn fd_close(&self, fd: usize) -> Result<(), ()> {
        self.process.files.lock().close(fd)
    }

    pub(crate) fn fd_duplicate(
        &self,
        old: usize,
        minimum: usize,
        cloexec: bool,
    ) -> Result<usize, ()> {
        self.process.files.lock().duplicate(old, minimum, cloexec)
    }

    pub(crate) fn fd_duplicate_to(
        &self,
        old: usize,
        new: usize,
        cloexec: bool,
    ) -> Result<usize, ()> {
        self.process.files.lock().duplicate_to(old, new, cloexec)
    }

    pub(crate) fn fd_flags(&self, fd: usize) -> Result<u32, ()> {
        self.process.files.lock().descriptor_flags(fd)
    }

    pub(crate) fn fd_set_flags(&self, fd: usize, flags: u32) -> Result<(), ()> {
        self.process.files.lock().set_descriptor_flags(fd, flags)
    }

    /// @description 原子准备并提交当前单线程 Process 的新 ELF 映像。
    ///
    /// @param elf_data 已完整读入 kernel 的 ELF bytes。
    /// @param args 写入新用户栈的参数。
    /// @param envs 写入新用户栈的环境。
    /// @return 准备或提交成功返回 `Ok(())`；ELF/内存错误在修改 Process 前返回。
    /// @errors 不支持的 ELF 与内存不足分别映射为 `ElfLoadError`。
    pub(crate) fn execve_replace(
        &self,
        elf_data: &[u8],
        args: &[Vec<u8>],
        envs: &[Vec<u8>],
    ) -> Result<(), ElfLoadError> {
        // 步骤1: 在不修改当前 Process 的前提下，完整准备新映像和初始栈。
        let (new_memory_set, user_sp, entry_point) = MemorySet::from_elf(elf_data, args, envs)?;

        // 步骤2: 替换内存管理结构
        // 这是关键步骤 - 完全替换当前进程的地址空间
        let kernel_stack_top = self.thread.kernel_stack.get_top();

        // 单次赋值提交新地址空间；旧 MemorySet 在 guard 内被完整替换，不暴露 stale PTE 窗口。
        *self.process.address_space.memory_set.lock() = new_memory_set;
        *self.thread.trap_cx_va.lock() = TRAP_CONTEXT;
        self.process.files.lock().close_cloexec();

        // 步骤3: 设置新程序的陷阱上下文。参数与环境只存在于新初始栈中。
        self.set_trap_context(TrapContext::app_init_context(
            entry_point,
            user_sp,
            KERNEL_SPACE.wait().lock().token(),
            kernel_stack_top,
            self.thread.kernel_trap_handler,
        ));

        // 地址空间由统一的 trap 返回路径激活；在这里切换会让后续内核代码运行在用户页表上。
        Ok(())
    }

    /// @description 返回当前 Process/thread group ID。
    ///
    /// @return TGID；Linux getpid 与 process-directed lookup 使用该值。
    pub(crate) fn tgid(&self) -> usize {
        self.process.tgid.0
    }

    /// @description 返回当前 Thread ID。
    ///
    /// @return 当前单线程模型中与 TGID 数值相同、但语义独立的 TID。
    pub(crate) fn tid(&self) -> usize {
        self.thread.tid
    }
}

impl core::fmt::Debug for TaskControlBlock {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            r#"
            TaskControlBlock {{
                tgid: {},
                tid: {},
                task_status: {:?}
            }}"#,
            self.tgid(),
            self.tid(),
            self.scheduling.state.lock().run_state
        )
    }
}
