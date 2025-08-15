use core::sync::atomic::{AtomicBool, Ordering};

/// 统一的任务管理器
///
/// 这个模块是系统中所有进程管理的中心，提供统一的抽象接口。
/// 它隐藏了进程在不同状态下的存储细节（调度器队列、睡眠队列、当前运行等），
/// 对外只暴露简洁的进程管理API。
use alloc::{collections::BTreeMap, string::ToString, sync::Arc, vec::Vec};
use lazy_static::lazy_static;
use spin::{RwLock, Mutex};

// Per-CPU 调度标志，指示是否需要重新调度
static NEED_RESCHED: [AtomicBool; MAX_CORES] = [
    AtomicBool::new(false), AtomicBool::new(false), AtomicBool::new(false), AtomicBool::new(false),
    AtomicBool::new(false), AtomicBool::new(false), AtomicBool::new(false), AtomicBool::new(false),
];

use crate::{
    arch::hart::{hart_id, MAX_CORES},
    task::{self, context::TaskContext, current_processor, processor::CORE_MANAGER, TaskControlBlock, TaskStatus},
    timer::{get_time_ns, get_time_us},
};
// 引入 GUI 所有权释放函数，用于进程退出时自动释放显示控制权
use crate::syscall::graphics::sys_gui_release_owner_for_tgid;

/// 调度策略
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SchedulingPolicy {
    FIFO,
    RoundRobin,
    Priority,
    CFS,
}

/// 进程状态统计
#[derive(Debug, Clone, Copy)]
pub struct ProcessStats {
    pub total: u32,
    pub running: u32,
    pub ready: u32,
    pub sleeping: u32,
    pub stopped: u32,
    pub zombie: u32,
}

/// 统一的任务管理器
///
/// 这是系统中唯一的进程状态权威源，所有其他组件都通过这个管理器来操作进程。
/// 睡眠管理现在直接基于进程的wake_time_ns字段，不需要单独存储。
pub struct TaskManager {
    /// 全局进程表：PID -> TaskControlBlock
    /// 这里存储系统中所有进程，无论其状态如何
    tasks: RwLock<BTreeMap<usize, Arc<TaskControlBlock>>>,

    /// init 进程的引用，用于特殊处理
    init_task: RwLock<Option<Arc<TaskControlBlock>>>,

    /// 当前的调度策略
    scheduling_policy: RwLock<SchedulingPolicy>,
}

impl TaskManager {
    pub fn new() -> Self {
        Self {
            tasks: RwLock::new(BTreeMap::new()),
            init_task: RwLock::new(None),
            scheduling_policy: RwLock::new(SchedulingPolicy::CFS),
        }
    }

    /// 添加新进程到系统
    /// 这是创建进程的统一入口点
    pub fn add_task(&self, task: Arc<TaskControlBlock>) {
        let pid = task.pid();

        // 添加到全局进程表
        {
            let mut tasks = self.tasks.write();
            tasks.insert(pid, task.clone());
        }

        // 如果是 init 进程，特别记录
        if pid == crate::task::pid::INIT_PID {
            *self.init_task.write() = Some(task.clone());
        }

        // 添加到多核调度器（根据当前状态）
        let status = *task.task_status.lock();
        match status {
            TaskStatus::Ready => {
                CORE_MANAGER.add_task(task);
            }
            TaskStatus::Sleeping => {
                // 睡眠任务通过wake_time_ns字段管理，无需额外处理
            }
            TaskStatus::Stopped => {
                // 被信号停止的任务不参与调度，直到收到SIGCONT信号
            }
            TaskStatus::Running => {
                // 运行中的任务已经在某个核心上，不需要添加到调度器
            }
            TaskStatus::Zombie => {
                // 僵尸进程不需要调度
            }
        }
    }

    /// 从系统中移除进程
    /// 这是进程回收的统一入口点
    pub fn remove_task(&self, pid: usize) -> Option<Arc<TaskControlBlock>> {
        let mut processes = self.tasks.write();
        if let Some(task) = processes.remove(&pid) {
            Some(task)
        } else {
            None
        }
    }

    /// 根据 PID 查找进程
    /// 这是查找进程的统一接口，性能优化的O(log n)查找
    pub fn find_task_by_pid(&self, pid: usize) -> Option<Arc<TaskControlBlock>> {
        let processes = self.tasks.read();
        processes.get(&pid).cloned()
    }

    /// 获取所有进程
    /// 这是获取进程列表的统一接口
    pub fn tasks(&self) -> Vec<Arc<TaskControlBlock>> {
        let processes = self.tasks.read();
        processes.values().cloned().collect()
    }

    /// 获取所有进程的 PID 列表
    pub fn pids(&self) -> Vec<usize> {
        let processes = self.tasks.read();
        processes.keys().cloned().collect()
    }

    /// 获取进程总数
    pub fn task_count(&self) -> usize {
        let processes = self.tasks.read();
        processes.len()
    }

    /// 获取 init 进程
    pub fn init_task(&self) -> Option<Arc<TaskControlBlock>> {
        let init_proc = self.init_task.read();
        init_proc.clone()
    }

    /// 获取进程统计信息
    /// 统一计算各种状态的进程数量
    pub fn task_stats(&self) -> ProcessStats {
        let processes = self.tasks.read();

        let mut running = 0u32;
        let mut ready = 0u32;
        let mut sleeping = 0u32;
        let mut stopped = 0u32;
        let mut zombie = 0u32;

        for task in processes.values() {
            let status = *task.task_status.lock();
            match status {
                TaskStatus::Running => running += 1,
                TaskStatus::Ready => ready += 1,
                TaskStatus::Sleeping => sleeping += 1,
                TaskStatus::Stopped => stopped += 1,
                TaskStatus::Zombie => zombie += 1,
            }
        }

        ProcessStats {
            total: processes.len() as u32,
            running,
            ready,
            sleeping,
            stopped,
            zombie,
        }
    }

    /// 获取特定状态的进程
    pub fn get_tasks_by_status(&self, status: TaskStatus) -> Vec<Arc<TaskControlBlock>> {
        let processes = self.tasks.read();
        processes
            .values()
            .filter(|task| *task.task_status.lock() == status)
            .cloned()
            .collect()
    }

    /// 获取在特定核心上运行的进程
    pub fn task_on_core(&self, core_id: usize) -> Option<Arc<TaskControlBlock>> {
        if let Some(processor) = CORE_MANAGER.get_processor(core_id) {
            let proc = processor.lock();
            proc.current.clone()
        } else {
            None
        }
    }

    /// 设置调度策略
    pub fn set_scheduling_policy(&self, policy: SchedulingPolicy) {
        *self.scheduling_policy.write() = policy;
    }

    /// 获取当前调度策略
    pub fn scheduling_policy(&self) -> SchedulingPolicy {
        *self.scheduling_policy.read()
    }

    /// 更新进程状态
    /// 当进程状态发生变化时，需要调用此函数来维护一致性
    pub fn update_task_status(&self, pid: usize, old_status: TaskStatus, new_status: TaskStatus) {
        if let Some(task) = self.find_task_by_pid(pid) {
            // 根据状态变化进行相应的调度器操作
            match (old_status, new_status) {
                (TaskStatus::Ready, TaskStatus::Running) => {
                    // 从调度器队列移动到某个核心的current，由调度器处理
                }
                (TaskStatus::Ready, TaskStatus::Stopped) => {
                    // 从调度器队列移动到停止状态，不参与调度
                    // 任务已经从调度器中移除，无需额外操作
                }
                (TaskStatus::Running, TaskStatus::Ready) => {
                    // 从某个核心的current移动到调度器队列
                    CORE_MANAGER.add_task(task);
                }
                (TaskStatus::Running, TaskStatus::Sleeping) => {
                    // 从某个核心的current移动到睡眠队列，由 timer 模块处理
                }
                (TaskStatus::Running, TaskStatus::Stopped) => {
                    // 从某个核心的current移动到停止状态，不参与调度
                }
                (TaskStatus::Sleeping, TaskStatus::Ready) => {
                    // 从睡眠队列移动到调度器队列
                    CORE_MANAGER.add_task(task);
                }
                (TaskStatus::Sleeping, TaskStatus::Stopped) => {
                    // 从睡眠状态移动到停止状态，不参与调度
                }
                (TaskStatus::Stopped, TaskStatus::Ready) => {
                    // 从停止状态恢复到调度器队列
                    CORE_MANAGER.add_task(task);
                }
                (_, TaskStatus::Zombie) => {
                    // 进程退出，不需要调度
                }
                _ => {
                    // 其他状态转换
                }
            }
        }
    }

    /// 同步所有进程状态
    /// 用于确保进程表与实际状态的一致性
    pub fn sync_all_task_states(&self) {
        let processes = self.tasks.read();
        for task in processes.values() {
            let pid = task.pid();
            let current_status = *task.task_status.lock();

            // 这里可以添加状态一致性检查的逻辑
            // 例如检查声称在运行的进程是否真的在某个核心上
            if current_status == TaskStatus::Running {
                let mut found_on_core = false;
                for i in 0..MAX_CORES {
                    if let Some(running_task) = self.task_on_core(i) {
                        if running_task.pid() == pid {
                            found_on_core = true;
                            break;
                        }
                    }
                }
                if !found_on_core {
                    warn!(
                        "Process PID {} claims to be running but not found on any core",
                        pid
                    );
                }
            }
        }
    }

    /// 添加任务到睡眠状态
    pub fn add_sleeping_task(&self, task: Arc<TaskControlBlock>, wake_time_ns: u64) {
        // 直接在任务的wake_time_ns字段设置唤醒时间
        task.wake_time_ns
            .store(wake_time_ns, core::sync::atomic::Ordering::Relaxed);
    }

    /// 获取所有睡眠任务
    pub fn sleeping_tasks(&self) -> Vec<Arc<TaskControlBlock>> {
        let processes = self.tasks.read();
        processes
            .values()
            .filter(|task| {
                *task.task_status.lock() == TaskStatus::Sleeping
                    && task
                        .wake_time_ns
                        .load(core::sync::atomic::Ordering::Relaxed)
                        > 0
            })
            .cloned()
            .collect()
    }

    /// 检查并唤醒到期的睡眠任务
    /// 返回被唤醒的任务列表
    pub fn check_and_wakeup_sleeping_tasks(
        &self,
        current_time_ns: u64,
    ) -> Vec<Arc<TaskControlBlock>> {
        let processes = self.tasks.read();
        let mut awakened_tasks = Vec::new();

        // 遍历所有进程，检查睡眠状态的进程是否到期
        for task in processes.values() {
            if *task.task_status.lock() == TaskStatus::Sleeping {
                let wake_time = task
                    .wake_time_ns
                    .load(core::sync::atomic::Ordering::Relaxed);
                if wake_time > 0 && wake_time <= current_time_ns {
                    // 清零唤醒时间，表示不再睡眠
                    task.wake_time_ns
                        .store(0, core::sync::atomic::Ordering::Relaxed);
                    awakened_tasks.push(task.clone());
                }
            }
        }
        awakened_tasks
    }

    /// 从睡眠状态中移除指定任务（用于提前唤醒）
    pub fn remove_sleeping_task(&self, task_pid: usize) -> bool {
        if let Some(task) = self.find_task_by_pid(task_pid) {
            if *task.task_status.lock() == TaskStatus::Sleeping {
                // 清零唤醒时间，表示不再睡眠
                task.wake_time_ns
                    .store(0, core::sync::atomic::Ordering::Relaxed);
                return true;
            }
        }
        false
    }

    /// 获取睡眠任务数量
    pub fn sleeping_task_count(&self) -> usize {
        let processes = self.tasks.read();
        processes
            .values()
            .filter(|task| {
                *task.task_status.lock() == TaskStatus::Sleeping
                    && task
                        .wake_time_ns
                        .load(core::sync::atomic::Ordering::Relaxed)
                        > 0
            })
            .count()
    }
}

// 全局统一任务管理器实例
lazy_static! {
    pub static ref TASK_MANAGER: TaskManager = TaskManager::new();
    static ref THREAD_JOIN_MANAGER: Mutex<alloc::collections::BTreeMap<usize, alloc::vec::Vec<alloc::sync::Arc<crate::task::TaskControlBlock>>>> = Mutex::new(alloc::collections::BTreeMap::new());
}

/// 添加任务到系统
pub fn add_task(task: Arc<TaskControlBlock>) {
    TASK_MANAGER.add_task(task);
}

/// 根据PID查找任务
pub fn find_task_by_pid(pid: usize) -> Option<Arc<TaskControlBlock>> {
    TASK_MANAGER.find_task_by_pid(pid)
}

/// 获取所有任务
pub fn get_all_tasks() -> Vec<Arc<TaskControlBlock>> {
    TASK_MANAGER.tasks()
}

/// 获取所有PID
pub fn get_all_pids() -> Vec<usize> {
    TASK_MANAGER.pids()
}

/// 获取任务数量
pub fn get_task_count() -> usize {
    TASK_MANAGER.task_count()
}

/// 获取init进程
pub fn init_proc() -> Option<Arc<TaskControlBlock>> {
    TASK_MANAGER.init_task()
}

/// 获取进程统计信息
pub fn get_process_statistics() -> ProcessStats {
    TASK_MANAGER.task_stats()
}

/// 设置调度策略
pub fn set_scheduling_policy(policy: SchedulingPolicy) {
    TASK_MANAGER.set_scheduling_policy(policy);
}

/// 获取调度策略
pub fn get_scheduling_policy() -> SchedulingPolicy {
    TASK_MANAGER.scheduling_policy()
}

/// 移除任务（用于进程回收）
pub fn remove_task(pid: usize) -> Option<Arc<TaskControlBlock>> {
    TASK_MANAGER.remove_task(pid)
}

/// 同步所有任务状态
pub fn sync_all_task_states() {
    TASK_MANAGER.sync_all_task_states();
}

/// 获取在特定核心上运行的任务
pub fn get_task_on_core(core_id: usize) -> Option<Arc<TaskControlBlock>> {
    TASK_MANAGER.task_on_core(core_id)
}

/// 添加任务到睡眠队列
pub fn add_sleeping_task(task: Arc<TaskControlBlock>, wake_time_ns: u64) {
    TASK_MANAGER.add_sleeping_task(task, wake_time_ns);
}

/// 获取所有睡眠任务
pub fn get_sleeping_tasks() -> Vec<Arc<TaskControlBlock>> {
    TASK_MANAGER.sleeping_tasks()
}

/// 从睡眠队列中移除指定任务
pub fn remove_sleeping_task(task_pid: usize) -> bool {
    TASK_MANAGER.remove_sleeping_task(task_pid)
}

/// 获取睡眠任务数量
pub fn get_sleeping_task_count() -> usize {
    TASK_MANAGER.sleeping_task_count()
}

/// 安全的状态更新函数
pub fn set_task_status(task: &Arc<TaskControlBlock>, new_status: TaskStatus) {
    let old_status = {
        let mut status_guard = task.task_status.lock();
        let old = *status_guard;
        *status_guard = new_status;
        old
    };
    // 通知统一任务管理器状态已改变
    if old_status != new_status {
        TASK_MANAGER.update_task_status(task.pid(), old_status, new_status);
    }
}

/// 检查并唤醒到期的睡眠任务
pub fn check_and_wakeup_sleeping_tasks(current_time_ns: u64) -> Vec<Arc<TaskControlBlock>> {
    let awakened_tasks = TASK_MANAGER.check_and_wakeup_sleeping_tasks(current_time_ns);

    // 将唤醒的任务状态设置为Ready（set_task_status会自动处理调度器添加）
    for task in &awakened_tasks {
        set_task_status(task, TaskStatus::Ready);
    }

    awakened_tasks
}

/// 统一的任务退出清理函数
///
/// # 参数
/// - task: 要清理的任务
/// - exit_code: 退出码
/// - from_signal: 是否来自信号终止（影响父子关系处理）
pub fn perform_task_exit_cleanup(task: &Arc<TaskControlBlock>, exit_code: i32, from_signal: bool) {
    let pid = task.pid();

    // 设置退出状态
    task.set_exit_code(exit_code);
    set_task_status(task, TaskStatus::Zombie);

    // 关闭所有文件描述符并清理文件锁
    task.file.lock().close_all_fds_and_cleanup_locks(pid);

    // 若该进程组持有 GUI 上下文所有权，释放之
    sys_gui_release_owner_for_tgid(task.tgid());

    // 重新父化子进程到init进程
    reparent_children_to_init(task);

    // 处理父子关系
    handle_parent_child_relationship(task, from_signal);
}
/// 线程退出清理：仅结束当前线程，不销毁进程共享资源
pub fn perform_thread_exit_cleanup(task: &Arc<TaskControlBlock>, exit_code: i32) {
    let pid = task.pid();

    // 设置退出状态
    task.set_exit_code(exit_code);
    set_task_status(task, TaskStatus::Zombie);

    // 释放线程槽位
    let slot = task.thread_slot.load(Ordering::Relaxed);
    {
        let mut slots = task.thread_slots.lock();
        if slot < slots.len() { slots[slot] = false; }
    }

    // 通知等待该线程的 join 者
    let mut join_map = THREAD_JOIN_MANAGER.lock();
    if let Some(waiters) = join_map.remove(&pid) {
        for waiter in waiters {
            if *waiter.task_status.lock() == TaskStatus::Sleeping {
                waiter.wakeup();
            }
        }
    }
}

/// 注册 join 等待者
pub fn register_thread_join_waiter(target_tid: usize, waiter: Arc<TaskControlBlock>) {
    let mut join_map = THREAD_JOIN_MANAGER.lock();
    join_map.entry(target_tid).or_default().push(waiter);
}


/// 将进程的子进程重新父化给init进程
fn reparent_children_to_init(task: &Arc<TaskControlBlock>) {
    let pid = task.pid();

    if let Some(init_proc) = TASK_MANAGER.init_task() {
        if pid == init_proc.pid() {
            error!("init process exit with exit_code {}", task.exit_code());
            return;
        }

        let children_to_reparent: Vec<_> = task
            .children
            .lock()
            .iter()
            .filter(|child| child.pid() != pid)
            .cloned()
            .collect();

        if !children_to_reparent.is_empty() {
            // 设置子进程的新父进程
            for child in &children_to_reparent {
                child.set_parent(Arc::downgrade(&init_proc));
            }

            // 将子进程添加到init进程的子进程列表
            let mut init_children = init_proc.children.lock();
            for child in children_to_reparent {
                init_children.push(child);
            }
        }
    }
}

/// 处理进程退出时的父子关系
fn handle_parent_child_relationship(task: &Arc<TaskControlBlock>, _from_signal: bool) {
    // 注意：无论进程因何退出（正常或被信号终止），都不应当把僵尸进程从父进程的
    // 子进程列表中移除并转交给 init。父进程需要能够 wait()/waitpid() 收尸。
    // 这里只唤醒可能正在等待的父进程。
    if let Some(parent) = task.parent() {
        wake_waiting_parent(&parent);
    }
}

/// 从父进程的子进程列表中移除指定任务
fn remove_from_parent_children(
    parent: &Arc<TaskControlBlock>,
    task: &Arc<TaskControlBlock>,
) -> bool {
    let mut parent_children = parent.children.lock();
    if let Some(pos) = parent_children
        .iter()
        .position(|child| Arc::ptr_eq(child, task))
    {
        parent_children.remove(pos);
        debug!(
            "Removed zombie process {} from parent {} children list",
            task.pid(),
            parent.pid()
        );
        true
    } else {
        false
    }
}

/// 如果需要，将任务转移给init进程
fn transfer_to_init_if_needed(task: &Arc<TaskControlBlock>, parent: &Arc<TaskControlBlock>) {
    let Some(init_proc) = TASK_MANAGER.init_task() else {
        return;
    };

    let pid = task.pid();

    // 只有当父进程不是init进程时才转移
    if pid != init_proc.pid() && parent.pid() != init_proc.pid() {
        init_proc.children.lock().push(task.clone());
        task.set_parent(Arc::downgrade(&init_proc));
        debug!("Transferred zombie process {} to init process", pid);
    }
}

/// 唤醒等待的父进程
fn wake_waiting_parent(parent: &Arc<TaskControlBlock>) {
    if *parent.task_status.lock() == TaskStatus::Sleeping {
        parent.wakeup();
    }
}

/// 暂停任务并重新加入调度队列（如果需要）
/// 这是 processor.rs 中 suspend_current_and_run_next 的统一接口
pub fn suspend_task_and_reschedule_if_needed(task: Arc<TaskControlBlock>) {
    let should_readd = *task.task_status.lock() == TaskStatus::Running;
    if should_readd {
        set_task_status(&task, TaskStatus::Ready);
    }
}

/// 准备任务以便挂起（统一处理运行时间统计）
/// 返回任务上下文指针
pub fn prepare_task_for_suspend(
    task: &Arc<TaskControlBlock>,
) -> *mut crate::task::context::TaskContext {
    let end_time = get_time_us();
    let runtime = end_time.saturating_sub(task.last_runtime.load(Ordering::Relaxed));

     // 更新调度器的虚拟运行时间
     task.sched.lock().update_vruntime(runtime);

    &mut *task.mm.task_cx.lock() as *mut _
}

/// 标记进程进入内核态
pub fn mark_kernel_entry() {
    if let Some(task) = current_task() {
        let current_time = get_time_us();
        let mut in_kernel = task.in_kernel_mode.lock();

        // 如果之前在用户态，计算用户态时间
        if !*in_kernel {
            let last_runtime = task.last_runtime.load(Ordering::Relaxed);
            if current_time > last_runtime {
                let user_time = current_time - last_runtime;
                task.user_cpu_time.fetch_add(user_time, Ordering::Relaxed);
                task.total_cpu_time.fetch_add(user_time, Ordering::Relaxed);
            }

            // 记录进入内核态的时间
            task.kernel_enter_time
                .store(current_time, Ordering::Relaxed);
            *in_kernel = true;
        }
    }
}

/// 标记进程退出内核态
pub fn mark_kernel_exit() {
    if let Some(task) = current_task() {
        let current_time = get_time_us();
        let mut in_kernel = task.in_kernel_mode.lock();

        // 如果之前在内核态，计算内核态时间
        if *in_kernel {
            let kernel_enter_time = task.kernel_enter_time.load(Ordering::Relaxed);
            if current_time > kernel_enter_time {
                let kernel_time = current_time - kernel_enter_time;
                task.kernel_cpu_time
                    .fetch_add(kernel_time, Ordering::Relaxed);
                task.total_cpu_time
                    .fetch_add(kernel_time, Ordering::Relaxed);
            }

            // 更新最后运行时间为退出内核态的时间
            task.last_runtime.store(current_time, Ordering::Relaxed);
            *in_kernel = false;
        }
    }
}

// nanosleep 实现
pub fn nanosleep(nanoseconds: u64) -> isize {
    if nanoseconds == 0 {
        return 0;
    }
    let start_time = get_time_ns();
    // 无论时间长短，都使用睡眠队列来保证准确性
    if let Some(current_task) = current_task() {
        let wake_time = start_time + nanoseconds;
        let pid = current_task.pid();

        set_task_status(&current_task, TaskStatus::Sleeping);
        add_sleeping_task(current_task, wake_time);
        // 让出CPU，等待被唤醒（此时任务状态为Sleeping，不会被重新加入就绪队列）
        block_current_and_run_next();

        // 醒来后检查实际时间
        let end_time = get_time_ns();
        let actual_sleep = end_time - start_time;

        // 检查是否睡眠时间足够
        if actual_sleep < nanoseconds {
            // 睡眠被提前中断，检查是否还需要继续睡眠
            let remaining_ns = nanoseconds - actual_sleep;
            if let Some(current_task) = task::current_task() {
                // 检查唤醒时间是否被清零（表示被信号中断）
                let wake_time = current_task
                    .wake_time_ns
                    .load(core::sync::atomic::Ordering::Relaxed);
                if wake_time == 0 {
                    // 唤醒时间被清零，说明被信号中断，返回 EINTR
                    return -4; // EINTR
                } else {
                    // 继续睡眠剩余时间
                    return nanosleep(remaining_ns);
                }
            }
        }
    } else {
        // 如果没有当前任务，使用忙等待（不推荐，但作为备用方案）
        let start_time = get_time_ns();
        while get_time_ns() - start_time < nanoseconds {
            // 忙等待
        }
    }

    0
}

/// 常量定义
pub const IDLE_PID: usize = 0;

/// 标记当前CPU需要重新调度
pub fn mark_need_resched() {
    let cpu = hart_id();
    if cpu < MAX_CORES {
        NEED_RESCHED[cpu].store(true, Ordering::Release);
    }
}

/// 检查并清除调度标志
pub fn check_and_clear_resched() -> bool {
    let cpu = hart_id();
    if cpu < MAX_CORES {
        NEED_RESCHED[cpu].swap(false, Ordering::AcqRel)
    } else {
        false
    }
}

/// 获取并移除当前任务
pub fn take_current_task() -> Option<Arc<TaskControlBlock>> {
    current_processor().lock().current.take()
}

/// 获取当前任务的引用
pub fn current_task() -> Option<Arc<TaskControlBlock>> {
    current_processor().lock().current.clone()
}

/// 获取当前任务的用户空间页表令牌
pub fn current_user_token() -> usize {
    current_task()
        .expect("No current task when getting user token")
        .mm
        .memory_set
        .lock()
        .token()
}

/// 获取当前任务的陷阱上下文
pub fn current_trap_context() -> &'static mut crate::trap::TrapContext {
    current_task()
        .expect("No current task when getting trap context")
        .mm
        .trap_context()
}

/// 获取当前工作目录
pub fn current_cwd() -> alloc::string::String {
    current_task()
        .map(|task| task.cwd.lock().clone())
        .unwrap_or_else(|| "/".to_string())
}

pub fn run_tasks() -> ! {
    let current_hart = hart_id();
    CORE_MANAGER.activate_core(current_hart);

    loop {
        if let Err(_) = crate::watchdog::feed() {
            // Watchdog 可能被禁用，这是正常的
        }

        // 1. 尝试从本地调度器获取任务
        let task = {
            let mut processor = current_processor().lock();
            processor.fetch_task()
        };

        if let Some(task) = task {
            if !task.is_zombie() {
                // 处理信号检查
                if task.signal_state.lock().has_deliverable_signals() {
                    if !handle_task_signals(&task) {
                        // 信号处理后任务不应该继续调度（可能被终止或停止）
                        continue;
                    }
                }

                // 检查任务是否处于睡眠状态
                if *task.task_status.lock() == TaskStatus::Sleeping {
                    // 睡眠状态的任务不应该被调度，跳过
                    continue;
                }

                // 切换到任务
                switch_to_task(task);
                continue;
            }
        }

        // 2. 尝试工作窃取
        if let Some(stolen_task) = CORE_MANAGER.steal_work(current_hart) {
            if !stolen_task.is_zombie() {
                // 检查被窃取的任务是否处于睡眠状态
                if *stolen_task.task_status.lock() == TaskStatus::Sleeping {
                    // 睡眠状态的任务不应该被调度，跳过
                    continue;
                }

                switch_to_task(stolen_task);
                continue;
            }
        }

        // 3. 没有任务，进入空闲状态
        use riscv::asm::wfi;
        wfi();
    }
}

/// 处理任务信号
/// 返回是否应该继续调度这个任务
fn handle_task_signals(task: &Arc<TaskControlBlock>) -> bool {
    use crate::signal::handle_signals;

    let (should_continue, exit_code) = handle_signals(task, None);

    if !should_continue {
        if let Some(code) = exit_code {
            debug!(
                "Task {} terminated by signal with exit code {}",
                task.pid(),
                code
            );
            // 执行任务清理，但不调度（因为我们在调度循环中）
            crate::task::task_manager::perform_task_exit_cleanup(task, code, true);
        }
        return false; // 不应该继续调度
    }

    // 检查任务是否被信号停止（例如 SIGTSTP/Ctrl+Z）
    if *task.task_status.lock() == TaskStatus::Stopped {
        debug!("Task {} was stopped by signal", task.pid());
        return false; // 被停止的任务不应该被调度
    }

    true // 可以继续调度
}

/// 挂起当前任务并运行下一个任务
pub fn suspend_current_and_run_next() {
    // 安全地获取当前任务
    let task = match take_current_task() {
        Some(t) => t,
        None => {
            // 没有当前任务，可能在idle循环中，直接返回
            return;
        }
    };
    
    // 验证任务有效性
    if task.is_zombie() {
        // Zombie任务不应该被调度
        return;
    }
    
    // 更新运行时间统计 - 使用安全的原子操作
    let end_time = get_time_us();
    let last_runtime = task.last_runtime.load(Ordering::Acquire);
    if last_runtime > 0 && end_time > last_runtime {
        let runtime = end_time - last_runtime;
        task.sched.lock().update_vruntime(runtime);
    }
    
    // 获取任务上下文指针
    // 重要：TaskContext存储在TaskControlBlock内部，由Arc保护
    let task_cx_ptr = {
        let task_cx = task.mm.task_cx.lock();
        let ptr = &*task_cx as *const TaskContext as *mut TaskContext;
        
        // 验证指针有效性
        if ptr.is_null() {
            panic!("Task context pointer is null for task {}", task.pid());
        }
        
        ptr
    };
    
    // 处理任务状态 - 保持Arc引用确保内存有效
    suspend_task_and_reschedule_if_needed(task.clone());
    
    // 内存屏障
    core::sync::atomic::fence(Ordering::SeqCst);
    
    // 切换到其他任务
    schedule(task_cx_ptr);
}

/// 阻塞当前任务并切换到下一个任务
pub fn block_current_and_run_next() {
    let task = take_current_task().expect("No current task to block");
    
    // 更新运行时间统计
    let end_time = get_time_us();
    let runtime = end_time.saturating_sub(task.last_runtime.load(Ordering::Relaxed));
    task.sched.lock().update_vruntime(runtime);
    
    // 对于阻塞场景，应当将任务状态设置为 Sleeping
    set_task_status(&task, TaskStatus::Sleeping);
    
    // 获取任务上下文指针
    let task_cx_ptr = {
        let task_cx = task.mm.task_cx.lock();
        &*task_cx as *const TaskContext as *mut TaskContext
    };
    
    // 内存屏障
    core::sync::atomic::fence(Ordering::SeqCst);
    
    // 切换到其他任务（task的Arc引用确保内存有效）
    schedule(task_cx_ptr);
}

/// 退出当前任务并运行下一个任务
pub fn exit_current_and_run_next(exit_code: i32) {
    let task = take_current_task().expect("No current task to exit");

    // 执行完整的任务清理
    perform_task_exit_cleanup(&task, exit_code, false);

    // 调度到下一个任务
    schedule(&mut *task.mm.task_cx.lock() as *mut _);
}

/// 切换到指定任务
fn switch_to_task(task: Arc<TaskControlBlock>) {
    // 获取处理器锁 - 必须在整个切换过程中持有
    let mut processor = current_processor().lock();
    
    // 检查是否已有当前任务
    if processor.current.is_some() {
        return;
    }
    
    // 检查并设置任务状态
    {
        let mut status = task.task_status.lock();
        if *status != TaskStatus::Ready {
            // 任务不在Ready状态，不能调度
            return;
        }
        *status = TaskStatus::Running;
    }
    
    // 记录任务开始运行的时间
    let start_time = get_time_us();
    task.last_runtime.store(start_time, Ordering::Relaxed);
    
    // 设置当前任务
    processor.current = Some(task.clone());
    
    // 获取idle上下文指针 - 在持有锁的情况下是安全的
    let idle_task_cx_ptr = processor.idle_context_ptr();
    
    // 获取任务上下文地址
    let next_task_cx_ptr = {
        let task_cx = task.mm.task_cx.lock();
        &*task_cx as *const TaskContext
    };
    
    // 验证指针
    if next_task_cx_ptr.is_null() {
        panic!("Invalid task context pointer");
    }
    
    // 在切换前释放processor锁
    // 这是安全的，因为：
    // 1. idle_context是processor的一部分，只要不重新分配processor就是有效的
    // 2. 任务上下文由Arc保护，是有效的
    drop(processor);
    
    // 内存屏障
    core::sync::atomic::fence(Ordering::SeqCst);
    
    // 执行上下文切换
    unsafe {
        crate::task::__switch(idle_task_cx_ptr, next_task_cx_ptr);
    }
}

/// 调度函数 - 切换到idle控制流
fn schedule(switched_task_cx_ptr: *mut TaskContext) {
    // 确保切换的上下文指针有效
    if switched_task_cx_ptr.is_null() {
        panic!("Invalid task context pointer in schedule");
    }
    
    let idle_task_cx_ptr = {
        let mut processor = current_processor().lock();
        let ptr = processor.idle_context_ptr();
        
        // 内存屏障，确保所有操作在切换前完成
        core::sync::atomic::fence(Ordering::SeqCst);
        ptr
    };

    unsafe {
        crate::task::__switch(switched_task_cx_ptr, idle_task_cx_ptr);
    }
}

/// 退出当前线程并切换到下一个任务（不销毁进程共享资源）
pub fn exit_current_thread_and_run_next(exit_code: i32) -> ! {
    let task = take_current_task().expect("No current task to exit (thread)");
    perform_thread_exit_cleanup(&task, exit_code);
    schedule(&mut *task.mm.task_cx.lock() as *mut _);
    unreachable!()
}
