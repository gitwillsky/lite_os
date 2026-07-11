use core::{error::Error, sync::atomic::AtomicUsize};

use alloc::{
    boxed::Box,
    string::{String, ToString},
};
use spin::Mutex;

use crate::{
    memory::{
        KERNEL_SPACE, TRAP_CONTEXT,
        address::VirtualAddress,
        kernel_stack::KernelStack,
        mm::{self, MemorySet, UserAccessError},
    },
    sync::IrqMutex,
    task::{context::TaskContext, pid::ProcessId},
    trap::{TrapContext, trap_handler},
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
    memory_set: Mutex<mm::MemorySet>,
}

impl AddressSpace {
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
}

#[derive(Debug)]
struct ThreadContext {
    tid: usize,
    kernel_stack: KernelStack,
    trap_cx_va: Mutex<usize>,
    task_cx: Mutex<TaskContext>,
}

#[derive(Debug)]
pub(crate) struct Sched {
    /// 本次运行开始的 monotonic 时间，只在 sched mutex 内访问。
    pub last_runtime: u64,
    /// nice值 (-20到19, 影响动态优先级计算)
    pub nice: i32,
    /// 累计运行时间 (用于CFS调度算法)
    pub vruntime: u64,
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

struct Credentials {
    uid: u32,
    euid: u32,
}

/// @description Process 级资源 owner；当前恰好由一个 Task/Thread 引用。
struct Process {
    name: Mutex<String>,
    tgid: ProcessId,
    address_space: AddressSpace,
    cwd: Mutex<String>,
    credentials: Mutex<Credentials>,
}

/// @description 当前单进程单线程模型的 Process、Thread 与 SchedulingEntity 组合边界。
pub struct TaskControlBlock {
    process: Process,
    thread: ThreadContext,
    pub(crate) scheduling: SchedulingEntity,
}

impl TaskControlBlock {
    pub(super) fn new_with_pid(
        name: &str,
        elf_data: &[u8],
        pid: ProcessId,
    ) -> Result<Self, Box<dyn Error>> {
        let (memory_set, user_sp, entry_point) = MemorySet::from_elf(elf_data)?;
        let kernel_stack = KernelStack::new();
        let kernel_stack_top = kernel_stack.get_top();
        let trap_cx_va = TRAP_CONTEXT;
        let tid = pid.0;
        let process = Process {
            name: Mutex::new(name.to_string()),
            tgid: pid,
            address_space: AddressSpace {
                memory_set: Mutex::new(memory_set),
            },
            cwd: Mutex::new("/".to_string()),
            credentials: Mutex::new(Credentials { uid: 0, euid: 0 }),
        };
        let tcb = Self {
            process,
            thread: ThreadContext {
                tid,
                kernel_stack,
                trap_cx_va: Mutex::new(trap_cx_va),
                task_cx: Mutex::new(TaskContext::goto_trap_return(kernel_stack_top)),
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
            trap_handler as usize,
        ));
        Ok(tcb)
    }

    /// 获取当前线程TrapContext虚拟地址
    pub fn trap_context_va(&self) -> usize {
        *self.thread.trap_cx_va.lock()
    }

    /// @description 覆盖当前 Thread 的 supervisor-only trap context。
    ///
    /// @param trap_context 待写入的完整上下文值。
    /// @return 无返回值；映射缺失表示 kernel 不变量损坏并 panic。
    pub fn set_trap_context(&self, trap_context: TrapContext) {
        let va = self.trap_context_va();
        let memory_set = self.process.address_space.memory_set.lock();
        let ppn = memory_set.trap_context_ppn(va);
        let offset = VirtualAddress::from(va).page_offset();
        assert!(offset + core::mem::size_of::<TrapContext>() <= crate::memory::PAGE_SIZE);
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
    pub fn load_trap_context(&self) -> TrapContext {
        let va = self.trap_context_va();
        let memory_set = self.process.address_space.memory_set.lock();
        let ppn = memory_set.trap_context_ppn(va);
        let offset = VirtualAddress::from(va).page_offset();
        assert!(offset + core::mem::size_of::<TrapContext>() <= crate::memory::PAGE_SIZE);
        let ptr = unsafe { ppn.as_page_ptr().add(offset).cast::<TrapContext>() };
        assert!(
            ptr.is_aligned(),
            "TrapContext physical address is not aligned"
        );
        // SAFETY: guard 保证 frame 存活；只读引用仅用于本行 clone 且不会逃逸。
        unsafe { (&*ptr).clone() }
    }

    pub fn copy_from_user(
        &self,
        user_address: usize,
        destination: &mut [u8],
    ) -> Result<(), UserAccessError> {
        self.process
            .address_space
            .copy_from_user(user_address, destination)
    }

    pub fn copy_to_user(&self, user_address: usize, source: &[u8]) -> Result<(), UserAccessError> {
        self.process
            .address_space
            .copy_to_user(user_address, source)
    }

    pub fn copy_user_string(
        &self,
        user_address: usize,
        max_len: usize,
    ) -> Result<String, UserAccessError> {
        self.process
            .address_space
            .copy_user_string(user_address, max_len)
    }

    pub fn user_token(&self) -> usize {
        self.process.address_space.memory_set.lock().token()
    }

    pub fn set_program_break(&self, new_break: usize) -> Result<usize, mm::MemoryError> {
        self.process
            .address_space
            .memory_set
            .lock()
            .set_program_break(new_break)
    }

    /// @description 取得当前 Thread 的 context-switch 保存区锁。
    ///
    /// @return TaskContext mutex；raw pointer 仅可在 TCB Arc 保活期间使用。
    pub fn task_context(&self) -> &Mutex<TaskContext> {
        &self.thread.task_cx
    }

    /// @description 复制当前 Process 的工作目录。
    ///
    /// @return owned cwd path。
    pub fn cwd(&self) -> String {
        self.process.cwd.lock().clone()
    }

    /// @description 原子准备并提交当前单线程 Process 的新 ELF 映像。
    ///
    /// @param program_name 新进程映像名称。
    /// @param elf_data 已完整读入 kernel 的 ELF bytes。
    /// @param args 写入新用户栈的参数。
    /// @param envs 写入新用户栈的环境。
    /// @return 准备或提交成功返回 `Ok(())`；ELF/内存错误在修改 Process 前返回。
    /// @errors 不支持的 ELF、范围错误与内存不足映射为 `MemoryError`。
    pub fn execve_replace(
        &self,
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

        // 步骤2: 替换内存管理结构
        // 这是关键步骤 - 完全替换当前进程的地址空间
        let kernel_stack_top = self.thread.kernel_stack.get_top();

        // 单次赋值提交新地址空间；旧 MemorySet 在 guard 内被完整替换，不暴露 stale PTE 窗口。
        *self.process.address_space.memory_set.lock() = new_memory_set;
        *self.thread.trap_cx_va.lock() = TRAP_CONTEXT;

        // 步骤3: 更新任务状态；参数与环境只存在于新初始栈中。
        *self.process.name.lock() = program_name.to_string();

        // 步骤4: 设置新程序的陷阱上下文
        self.set_trap_context(TrapContext::app_init_context(
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
        self.process.name.lock().clone()
    }

    /// 设置用户ID (需要root权限)
    pub fn set_uid(&self, uid: u32) -> Result<(), i32> {
        let mut credentials = self.process.credentials.lock();
        // 只有root用户可以设置任意UID
        if credentials.euid != 0 && credentials.euid != uid {
            return Err(-1); // EPERM
        }
        credentials.uid = uid;
        credentials.euid = uid;
        Ok(())
    }

    /// @description 返回当前 Process/thread group ID。
    ///
    /// @return TGID；Linux getpid 与 process-directed lookup 使用该值。
    pub fn tgid(&self) -> usize {
        self.process.tgid.0
    }

    /// @description 返回当前 Thread ID。
    ///
    /// @return 当前单线程模型中与 TGID 数值相同、但语义独立的 TID。
    pub fn tid(&self) -> usize {
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
                name: {},
                task_status: {:?}
            }}"#,
            self.tgid(),
            self.tid(),
            self.name(),
            self.scheduling.state.lock().run_state
        )
    }
}
