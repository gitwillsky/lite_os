use alloc::{boxed::Box, collections::VecDeque, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use lazy_static::lazy_static;
use spin::{Mutex, RwLock};

use crate::{
    arch::hart::{MAX_CORES, hart_id, is_valid_hart_id},
    arch::sbi,
    task::{
        TaskControlBlock, TaskStatus,
        context::TaskContext,
        scheduler::{Scheduler, cfs_scheduler::CFScheduler},
    },
    timer::get_time_us,
};

/// idle返回函数 - 当从任务切换回idle时执行
/// 这个函数永远不应该被调用，因为idle循环不会切换回来
/// 但我们需要一个有效的返回地址以避免异常
#[unsafe(no_mangle)]
pub extern "C" fn idle_return() -> ! {
    // 如果执行到这里，说明出现了严重错误
    panic!("idle_return should never be called!");
}

pub struct Processor {
    pub hart_id: usize,
    pub current: Option<Arc<TaskControlBlock>>,
    pub idle_context: TaskContext,
    pub scheduler: Box<dyn Scheduler>,
    pub task_count: AtomicUsize,
    pub active: AtomicBool,
    pub inbound: Mutex<VecDeque<Arc<TaskControlBlock>>>,
}

impl Processor {
    pub fn new(hart_id: usize) -> Self {
        // 创建一个有效的idle上下文
        // 重要：idle上下文的ra必须指向一个有效的返回地址
        let mut idle_ctx = TaskContext::zero_init();
        // 设置返回地址为idle_return函数
        idle_ctx.set_ra(idle_return as usize);

        Self {
            hart_id,
            current: None,
            idle_context: idle_ctx,
            scheduler: Box::new(CFScheduler::new()),
            task_count: AtomicUsize::new(0),
            active: AtomicBool::new(false),
            inbound: Mutex::new(VecDeque::new()),
        }
    }

    pub fn idle_context_ptr(&mut self) -> *mut TaskContext {
        &mut self.idle_context
    }

    pub fn add_task(&mut self, task: Arc<TaskControlBlock>) {
        self.scheduler.add_task(task);
        self.task_count.fetch_add(1, Ordering::Relaxed);
    }

    pub fn fetch_task(&mut self) -> Option<Arc<TaskControlBlock>> {
        if let Some(task) = self.scheduler.fetch_task() {
            self.task_count.fetch_sub(1, Ordering::Relaxed);
            Some(task)
        } else {
            None
        }
    }

    pub fn task_count(&self) -> usize {
        self.task_count.load(Ordering::Relaxed)
    }

    pub fn steal_task(&mut self) -> Option<Arc<TaskControlBlock>> {
        let count = self.task_count.load(Ordering::Acquire);
        if count > 1 {
            if let Some(task) = self.scheduler.fetch_task() {
                self.task_count.fetch_sub(1, Ordering::Release);
                let status = task.task_status.lock();
                if *status != TaskStatus::Ready {
                    drop(status);
                    self.scheduler.add_task(task.clone());
                    self.task_count.fetch_add(1, Ordering::Release);
                    return None;
                }
                drop(status);
                Some(task)
            } else {
                None
            }
        } else {
            None
        }
    }

    pub fn drain_inbound_to_local(&mut self) {
        let mut q = self.inbound.lock();
        let mut tmp = VecDeque::new();
        core::mem::swap(&mut *q, &mut tmp);
        drop(q);
        for t in tmp {
            self.add_task(t);
        }
    }
}

// Per-CPU 变量，每个核心完全独立
static mut PER_CPU_PROCESSORS: [Option<Processor>; MAX_CORES] = [const { None }; MAX_CORES];
static ACTIVE_CORES: AtomicUsize = AtomicUsize::new(0);
static NEXT_CPU: AtomicUsize = AtomicUsize::new(0);

/// 无锁访问当前 CPU 的处理器
pub fn current_processor() -> &'static mut Processor {
    let hart = hart_id();
    unsafe {
        if PER_CPU_PROCESSORS[hart].is_none() {
            PER_CPU_PROCESSORS[hart] = Some(Processor::new(hart));
        }
        PER_CPU_PROCESSORS[hart].as_mut().unwrap()
    }
}

pub fn add_task_to_cpu(cpu_id: usize, task: Arc<TaskControlBlock>) {
    let me = hart_id();
    unsafe {
        if cpu_id < MAX_CORES {
            if PER_CPU_PROCESSORS[cpu_id].is_none() {
                PER_CPU_PROCESSORS[cpu_id] = Some(Processor::new(cpu_id));
            }
            if let Some(p) = &mut PER_CPU_PROCESSORS[cpu_id] {
                if cpu_id == me {
                    p.add_task(task);
                } else {
                    {
                        let mut q = p.inbound.lock();
                        q.push_back(task);
                    }
                    core::sync::atomic::fence(Ordering::Release);
                    let _ = sbi::sbi_send_ipi(1usize << cpu_id, 0);
                }
            }
        }
    }
}

pub fn add_task_to_best_cpu(task: Arc<TaskControlBlock>) -> usize {
    let start = NEXT_CPU.fetch_add(1, Ordering::Relaxed) % MAX_CORES;
    let mut best_cpu = hart_id();
    let mut best_load = usize::MAX;
    let last = task.last_cpu.load(Ordering::Relaxed);
    let mut last_active = false;
    let mut last_load = usize::MAX;
    unsafe {
        for k in 0..MAX_CORES {
            let i = (start + k) % MAX_CORES;
            if let Some(p) = &PER_CPU_PROCESSORS[i] {
                if p.active.load(Ordering::Relaxed) {
                    let inbound_len = {
                        let q = p.inbound.lock();
                        q.len()
                    };
                    let load = p.task_count().saturating_add(inbound_len);
                    if load < best_load {
                        best_load = load;
                        best_cpu = i;
                    }
                    if i == last {
                        last_active = true;
                        last_load = load;
                    }
                }
            }
        }
    }
    let chosen = if last_active && last_load <= best_load.saturating_add(1) {
        last
    } else {
        best_cpu
    };
    add_task_to_cpu(chosen, task);
    chosen
}

pub fn try_global_steal() -> Option<Arc<TaskControlBlock>> {
    None
}

/// 获取活跃核心数量
pub fn active_core_count() -> usize {
    ACTIVE_CORES.load(Ordering::Relaxed)
}

/// 标记核心为活跃状态
pub fn mark_core_active() {
    ACTIVE_CORES.fetch_add(1, Ordering::Relaxed);
}
