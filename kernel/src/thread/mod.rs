//! 线程：调度实体、内核栈、上下文切换、阻塞与唤醒、kill 与 kick。
//!
//! # 用户线程
//!
//! 线程拥有用户寄存器上下文、地址空间与一个 [`ThreadOwner`]。用户态 trap、返回用户态前的
//! 处理与线程退出都只经 owner 分派，本领域不解释 syscall 或异常的语义。
//!
//! # kill 与 kick
//!
//! - kill：线程必须退出。可中断等待立即以 `Interrupted` 结束，返回用户态前 owner 观察到后退出。
//! - kick：要求线程尽快经过一次用户态返回点，不打断阻塞等待；owner 用它实现 restricted mode 退出。
//!
//! # 阅读顺序
//!
//! [`Thread::create_user`] → [`Thread::start`] → `wait.rs`（阻塞协议）→ `context_switch.rs` →
//! [`exit_current`] → [`run`]（每 CPU 调度循环）。

use alloc::sync::Arc;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicU32, Ordering};

use spin::{Mutex, Once};

mod context_switch;
mod processor;
mod scheduler;
pub(crate) use processor::{DeadlineError, IsolationError, replace_isolation};
pub(crate) use scheduler::deadline::Parameters as DeadlineParameters;
mod scheduling;
mod timer_queue;
mod user_context;
mod wait;

pub(crate) use context_switch::{yield_current, yield_current_period};
pub(crate) use processor::{cpu_busy_snapshot, take_reschedule};
use scheduling::SchedulingEntity;
pub(crate) use scheduling::{DEFAULT_PRIORITY, PRIORITY_LEVELS};
use user_context::ContextSlot;

use crate::arch::context::{KernelContext, KernelResume, UserContext};
use crate::arch::trap::UserTrapEntry;
use crate::execution::deferred::{self, DeferredWork};
use crate::execution::sync::LocalIrqGuard;
use crate::execution::timer::get_time_ns;
use crate::vm::{AddressSpace, KERNEL_SPACE, KernelStack};

const KILLED: u32 = 1 << 0;
const KICKED: u32 = 1 << 1;

/// 线程（或进程内全部存活线程之和）的 CPU 时间与上下文切换计数。
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Runtime {
    pub(crate) cpu_ns: u64,
    pub(crate) restricted_ns: u64,
    pub(crate) voluntary_switches: u64,
    pub(crate) involuntary_switches: u64,
    pub(crate) restricted_faults: u64,
    pub(crate) restricted_pager_waits: u64,
}

impl Runtime {
    pub(crate) fn add(&mut self, other: Self) {
        self.cpu_ns = self.cpu_ns.saturating_add(other.cpu_ns);
        self.restricted_ns = self.restricted_ns.saturating_add(other.restricted_ns);
        self.voluntary_switches = self
            .voluntary_switches
            .saturating_add(other.voluntary_switches);
        self.involuntary_switches = self
            .involuntary_switches
            .saturating_add(other.involuntary_switches);
        self.restricted_faults = self
            .restricted_faults
            .saturating_add(other.restricted_faults);
        self.restricted_pager_waits = self
            .restricted_pager_waits
            .saturating_add(other.restricted_pager_waits);
    }
}

/// 用户线程返回用户态所经的 trap handler 与首次调度入口；`main` 在启动用户态前安装一次。
#[derive(Clone, Copy)]
pub(crate) struct UserEntryPoints {
    pub(crate) trap_handler: UserTrapEntry,
    pub(crate) trap_return: KernelResume,
}

// OWNER: 启动期唯一写入；缺失时创建用户线程 fail-stop，而不是带着无效 trap 入口进入用户态。
static USER_ENTRY: Once<UserEntryPoints> = Once::new();

/// 安装用户线程的 trap 入口。
///
/// # Panics
/// 重复安装时 fail-stop。
pub(crate) fn install_user_entry(entry: UserEntryPoints) {
    assert!(USER_ENTRY.get().is_none(), "user entry installed twice");
    USER_ENTRY.call_once(|| entry);
}

/// 用户态同步异常的语义事件。
#[derive(Debug, Clone, Copy)]
pub(crate) enum UserException {
    /// 缺页。
    PageFault {
        address: usize,
        access: crate::vm::FaultAccess,
        syndrome: u64,
    },
    /// 权限或总线访问错误。
    AccessFault { address: usize, syndrome: u64 },
    /// 未对齐访问。
    Alignment { address: usize, syndrome: u64 },
    /// 非法指令；`pc` 为指令地址。
    IllegalInstruction { pc: usize },
    /// 断点指令。
    Breakpoint { pc: usize },
    /// 架构未解码的异常。
    Unsupported { pc: usize },
}

/// owner 处理一次 trap 后线程的去向。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Disposition {
    /// 继续执行。
    Continue,
    /// 线程必须退出；trap 路径释放全部引用后调用 [`exit_current`]。
    Exit,
}

/// owner 完成可阻塞的返回前处理后，移交一次硬件视图选择；不复制持久执行模式。
pub(crate) enum UserReturn {
    Enter(crate::vm::ExecutionView),
    Exit,
}

/// 用户线程的 trap 语义；由进程层实现。
///
/// 方法返回 [`Disposition`] 或 [`UserReturn`]，不自行退出：退出不返回，若在 owner 内部直接退出，
/// 调用栈上持有的线程与 owner 引用将永久泄漏。
pub(crate) trait ThreadOwner: Send + Sync {
    /// 处理一次 syscall 指令；寄存器在 `thread` 的用户上下文中。
    fn syscall(self: Arc<Self>, thread: &Arc<Thread>) -> Disposition;
    /// 处理一次同步异常。
    fn exception(self: Arc<Self>, thread: &Arc<Thread>, exception: UserException) -> Disposition;
    /// 返回用户态前的最后处理（kill、kick、模式切换）。
    fn before_user_return(self: Arc<Self>, thread: &Arc<Thread>) -> UserReturn;
    /// 线程执行体已退休、仍为 current 时调用一次。
    fn exited(self: Arc<Self>, thread: &Arc<Thread>);
}

struct UserState {
    // OWNER: 必须先于 `Thread::kernel_stack` 退休：AArch64 上下文位于内核栈保留页。
    context: ContextSlot<UserContext>,
    // OWNER: 线程运行所在地址空间的一个 user；退出时在仍为 current 的上下文中 take，
    // 使最后一次地址空间退休（全 CPU fence）不发生在关中断的 idle reap 中。
    space: Mutex<Option<Arc<AddressSpace>>>,
    // OWNER: RISC-V 上下文页在地址空间中的地址；退出时释放。
    context_page: Option<usize>,
    owner: Arc<dyn ThreadOwner>,
}

/// 用户执行体与内核工作函数互斥；内核线程不伪造用户空间或 ThreadOwner。
enum ThreadBody {
    User(UserState),
    Kernel(fn()),
}

/// 线程。
pub(crate) struct Thread {
    id: u64,
    body: ThreadBody,
    // OWNER: 只为保活内核栈映射；上下文切换经 `kernel_context` 中保存的栈指针使用它。
    _kernel_stack: KernelStack,
    // OWNER: 从创建到最后一个队列 token 归还期间保有可调度容量。
    _scheduler_slot: processor::ThreadSlot,
    kernel_context: Mutex<KernelContext>,
    scheduling: SchedulingEntity,
    // FLAG: KILLED/KICKED，见模块文档；Release 发布、Acquire 观察。
    flags: AtomicU32,
}

/// 用户线程首次进入用户态的寄存器。
#[derive(Debug, Clone, Copy)]
pub(crate) struct UserStart {
    pub(crate) entry: usize,
    pub(crate) stack: usize,
    pub(crate) arguments: [usize; 2],
}

impl Thread {
    /// 线程全局唯一编号。
    pub(crate) fn id(&self) -> u64 {
        self.id
    }

    /// 创建尚未启动的用户线程。
    ///
    /// # Parameters
    /// - `space`: 线程运行的地址空间。
    /// - `owner`: 用户态 trap 语义。
    /// - `start`: 首次进入用户态的寄存器。
    ///
    /// # Errors
    /// 调度存储、内核栈、上下文页或对象分配失败返回 `Err(())`；不留下已分配的上下文页。
    pub(crate) fn create_user(
        space: Arc<AddressSpace>,
        owner: Arc<dyn ThreadOwner>,
        start: UserStart,
    ) -> Result<Arc<Self>, ()> {
        let entry = *USER_ENTRY.get().expect("user entry not installed");
        let kernel_stack = KernelStack::try_new()?;
        let scheduler_slot = processor::ThreadSlot::try_new()?;
        let top = kernel_stack.get_top();
        let (address, pointer, context_page) = match kernel_stack.user_context_address() {
            Some(address) => (address, address as *mut UserContext, None),
            None => {
                let (address, pointer) = space.allocate_context().map_err(|_| ())?;
                (address, pointer.cast::<UserContext>(), Some(address))
            }
        };
        let pointer = NonNull::new(pointer).expect("user context address is null");
        // SAFETY: AArch64 指针位于本线程独占的内核栈保留页；RISC-V 指针是刚为本线程分配、
        // 由地址空间 `contexts` 保活的页。两者都对齐且在 retire 前不会被其他代码访问。
        let context = unsafe { ContextSlot::bind(address, pointer) };
        let mut user_context = UserContext::app_init_context(
            start.entry,
            start.stack,
            KERNEL_SPACE.wait().lock().kernel_trap_token(),
            top,
            entry.trap_handler,
        );
        user_context.set_entry_arguments(start.arguments[0], start.arguments[1]);
        // SAFETY: 上下文存储尚未被任何人使用；整体写入初始值后才通过 slot 访问。
        unsafe { pointer.as_ptr().write(user_context) };
        let thread = Arc::try_new(Self {
            id: scheduler_slot.id(),
            body: ThreadBody::User(UserState {
                context,
                space: Mutex::new(Some(space.clone())),
                context_page,
                owner,
            }),
            _kernel_stack: kernel_stack,
            _scheduler_slot: scheduler_slot,
            kernel_context: Mutex::new(KernelContext::with_resume(top, run_user_thread)),
            scheduling: SchedulingEntity::new(
                DEFAULT_PRIORITY,
                0,
                crate::execution::cpu::current_id().index(),
            ),
            flags: AtomicU32::new(0),
        });
        match thread {
            Ok(thread) => {
                processor::register_thread(&thread);
                Ok(thread)
            }
            Err(_) => {
                if let Some(address) = context_page {
                    space.release_context(address);
                }
                Err(())
            }
        }
    }

    /// 把 New 线程交给调度器。
    ///
    /// # Panics
    /// 线程不处于 New 时 fail-stop。
    pub(crate) fn start(self: &Arc<Self>) {
        processor::enqueue_new(self.clone());
    }

    fn user(&self) -> &UserState {
        match &self.body {
            ThreadBody::User(user) => user,
            ThreadBody::Kernel(_) => panic!("kernel thread has no user state"),
        }
    }

    /// 用户线程的 owner。
    pub(crate) fn owner(&self) -> &Arc<dyn ThreadOwner> {
        &self.user().owner
    }

    /// 用户线程运行所在的地址空间；线程退出后为 `None`。
    pub(crate) fn address_space(&self) -> Option<Arc<AddressSpace>> {
        match &self.body {
            ThreadBody::User(user) => user.space.lock().clone(),
            ThreadBody::Kernel(_) => None,
        }
    }

    /// 在唯一 transaction 内访问用户寄存器上下文。
    pub(crate) fn with_user_context<R>(&self, operation: impl FnOnce(&mut UserContext) -> R) -> R {
        self.user().context.with(operation)
    }

    /// 返回用户态前发布本 CPU 信息并取得 trampoline 使用的上下文地址。
    pub(crate) fn prepare_user_return(&self, logical_cpu: usize) -> usize {
        let user = self.user();
        user.context
            .with(|context| context.prepare_kernel_return(logical_cpu));
        user.context.address()
    }

    /// 要求线程退出；可中断等待立即结束。
    pub(crate) fn kill(self: &Arc<Self>) {
        self.flags.fetch_or(KILLED, Ordering::Release);
        // 终止必须能运行清理，不能等一个任意长的 CBS 补充周期；撤销也归还带宽凭据。
        let _ = processor::replace_deadline(self, None);
        wait::interrupt(self);
        processor::request_thread_reschedule(self);
    }

    /// 是否已被要求退出。
    pub(crate) fn is_killed(&self) -> bool {
        self.flags.load(Ordering::Acquire) & KILLED != 0
    }

    /// 要求线程尽快经过一次用户态返回点；不打断阻塞等待。
    pub(crate) fn kick(self: &Arc<Self>) {
        self.flags.fetch_or(KICKED, Ordering::Release);
        processor::request_thread_reschedule(self);
    }

    /// 消费一次 kick 请求。
    pub(crate) fn take_kick(&self) -> bool {
        self.flags.fetch_and(!KICKED, Ordering::AcqRel) & KICKED != 0
    }

    /// 把线程限制在 `cpus` 上运行。
    ///
    /// # Errors
    /// `cpus` 与 possible CPU 不相交返回 `Err(())`。
    pub(crate) fn set_affinity(
        self: &Arc<Self>,
        cpus: crate::execution::cpu::CpuSet,
    ) -> Result<(), ()> {
        let affinity = scheduling::CpuAffinity::restricted_to(cpus).ok_or(())?;
        processor::replace_affinity(self, affinity)?;
        Ok(())
    }

    /// 设置或撤销已验证的 deadline 配置；准入失败不改变原策略。
    pub(crate) fn set_deadline(
        self: &Arc<Self>,
        parameters: Option<scheduler::deadline::Parameters>,
    ) -> Result<(), DeadlineError> {
        processor::replace_deadline(self, parameters)
    }

    /// CPU 时间与上下文切换计数。
    pub(crate) fn runtime(&self) -> Runtime {
        let (cpu_ns, restricted_ns) = {
            let policy = self.scheduling.policy.lock();
            policy.runtime_at(get_time_ns())
        };
        let state = self.scheduling.state.lock();
        Runtime {
            cpu_ns,
            restricted_ns,
            voluntary_switches: state.voluntary_switches,
            involuntary_switches: state.involuntary_switches,
            restricted_faults: state.restricted_faults,
            restricted_pager_waits: state.restricted_pager_waits,
        }
    }

    /// 记录一次已成功解决的 restricted 缺页；失败的解析与 normal mode 的服务缺页不计入。
    pub(crate) fn record_restricted_fault(&self, source: crate::vm::FaultSource) {
        let mut state = self.scheduling.state.lock();
        state.restricted_faults += 1;
        if source == crate::vm::FaultSource::Pager {
            state.restricted_pager_waits += 1;
        }
    }

    /// 当前线程进入或离开 restricted mode 时切换计时桶。
    pub(crate) fn set_restricted_accounting(&self, restricted: bool) {
        self.scheduling
            .policy
            .lock()
            .set_restricted(crate::execution::timer::get_time_ns, restricted);
    }

    /// 设置权重等级（`0..PRIORITY_LEVELS`，数值越小权重越大）。
    ///
    /// # Errors
    /// 越界返回 `Err(())`。
    pub(crate) fn set_priority(&self, priority: u8) -> Result<(), ()> {
        if priority >= PRIORITY_LEVELS {
            return Err(());
        }
        self.scheduling.policy.lock().set_priority(priority);
        Ok(())
    }
}

impl Drop for Thread {
    fn drop(&mut self) {
        // 从未启动的用户线程也必须先退休上下文，再释放内核栈或上下文页。
        if let ThreadBody::User(user) = &self.body
            && user.context.is_live()
        {
            user.context.retire();
            if let (Some(address), Some(space)) = (user.context_page, user.space.lock().take()) {
                space.release_context(address);
            }
        }
    }
}

/// 当前 CPU 正在运行的线程。
pub(crate) fn current() -> Option<Arc<Thread>> {
    processor::with_current_processor(|processor| processor.current.clone())
}

/// 用户线程首次调度的 continuation：完成 handoff 后进入 trap return。
fn run_user_thread() -> ! {
    context_switch::complete_pending_handoff();
    let entry = USER_ENTRY.get().expect("user entry not installed");
    (entry.trap_return)()
}

/// 终止当前线程，不返回。
///
/// 1. 仍为 current 时退休用户上下文、上下文页与地址空间，并通知 owner；这些步骤可能阻塞，
///    必须保留 wait target。
/// 2. 关中断后撤销 current，把最后 owner 交给 deferred reap，切到 idle。
pub(crate) fn exit_current() -> ! {
    let thread = current().expect("exiting thread lost current ownership");
    if let ThreadBody::User(user) = &thread.body {
        user.context.retire();
        let space = user.space.lock().take();
        if let (Some(address), Some(space)) = (user.context_page, &space) {
            space.release_context(address);
        }
        drop(space);
        user.owner.clone().exited(&thread);
    }
    drop(thread);
    let _masked = crate::arch::interrupt::disable_local();
    let (save, idle) = {
        let thread = processor::with_current_processor(processor::Processor::take_current)
            .expect("exiting thread lost current ownership");
        {
            let mut policy = thread.scheduling.policy.lock();
            policy.finish(get_time_ns);
            policy.retire_deadline();
        }
        processor::exit_partition(&thread);
        let idle = processor::with_current_processor(processor::Processor::idle_context_ptr);
        let save = {
            let mut context = thread.kernel_context.lock();
            &mut *context as *mut KernelContext
        };
        processor::defer_reap(thread);
        (save, idle)
    };
    // SAFETY: 调度 owner 已移交 deferred reap；两个上下文由本 CPU 独占，本 frame 不再持有线程引用。
    unsafe { crate::arch::context::switch_kernel_context(save, idle) };
    panic!("exited thread context resumed")
}

/// 睡眠到 `deadline_ns`。
///
/// # Returns
/// 到期返回 `TimedOut`；被 kill 返回 `Interrupted`；定时节点分配失败返回 `OutOfMemory`。
pub(crate) fn sleep_until(deadline_ns: u64) -> crate::execution::sync::WaitResult {
    wait::sleep_until(deadline_ns)
}

/// 处理当前 CPU 已发布的 deferred work。只在关中断的调度安全点或用户态返回汇合点调用。
pub(crate) fn dispatch_deferred() {
    // 关中断的协作式等待也必须推进远端 rendezvous；只靠 IPI trap 会让回收者等待
    // 正在反复 yield、却从未返回用户态开中断的 CPU，导致全核 TLB/内存屏障互等。
    crate::platform::complete_pending_ipi();
    crate::execution::barrier::complete_pending();
    let work = deferred::take();
    if work.is_empty() {
        return;
    }
    if work.contains(DeferredWork::TIMER) {
        if let Some(thread) = current() {
            thread.scheduling.policy.lock().checkpoint(get_time_ns);
        }
        crate::crash::note_deferred_progress();
        timer_queue::expire(get_time_ns());
        processor::request_slice_reschedule();
    } else if work.contains(DeferredWork::TIMER_BACKLOG) {
        // 空闲时没有调度 tick：boot CPU 的 housekeeping 与到期唤醒也必须推进看门狗的 deferred 进度，
        // 否则全空闲的系统会被误判为 deferred 停滞。
        crate::crash::note_deferred_progress();
        timer_queue::expire(get_time_ns());
    }
    if work.contains(DeferredWork::RUNQUEUE) {
        processor::dispatch_inbound_ready();
    }
    if work.contains(DeferredWork::CONSOLE) {
        crate::serial::input_ready();
    }
    work.run_registered(get_time_ns());
}

/// 初始化调度拓扑与等待协议适配器。必须在任何线程创建前调用。
pub(crate) fn initialize() {
    processor::init_topology();
    timer_queue::initialize();
    wait::install();
}

/// 每 CPU 调度循环；不返回。
pub(crate) fn run() -> ! {
    processor::with_current_processor(|_| {
        // Release 发布本地 Processor 初始化；缺失时远端可能向尚未开始 drain 的 CPU 投递。
        crate::execution::cpu::mark_active();
    });
    loop {
        // 1. 关中断覆盖 deferred、mailbox drain 与选择，保证空闲决策看到一致状态。
        let idle_irq = LocalIrqGuard::disable();
        dispatch_deferred();
        processor::with_current_processor(processor::Processor::drain_inbound);
        if let Some(thread) = processor::with_current_processor(processor::Processor::select) {
            context_switch::switch_from_idle(thread);
            drop(idle_irq);
            continue;
        }
        // 空闲：没有时间片截止，硬件 deadline 只剩本 CPU 的截止时间等待者（boot CPU 另有 housekeeping）。
        processor::enter_idle();
        // 2. 固定的 WFI/resume PC 使 trap 入口能跳过已消费 edge 对应的 WFI，关闭 enable-to-WFI 窗口。
        crate::arch::interrupt::wait_with_local_irq_masked();
        drop(idle_irq);
    }
}
