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
            // 尝试获取任务，但在状态检查前不修改计数器
            if let Some(task) = self.scheduler.fetch_task() {
                // 首先检查任务是否可窃取，避免借用冲突
                let is_ready = {
                    let status_guard = task.task_status.lock();
                    *status_guard == TaskStatus::Ready
                };
                
                if !is_ready {
                    // 任务状态不对，放回调度器
                    self.scheduler.add_task(task.clone());
                    return None;
                }
                
                // 验证任务不是僵尸状态（额外的安全检查）
                if task.is_zombie() {
                    warn!("Attempted to steal zombie task {}", task.pid());
                    // 僵尸任务不应该回到调度器
                    return None;
                }
                
                // 状态检查通过，现在才减少计数器
                self.task_count.fetch_sub(1, Ordering::Release);
                
                debug!(
                    "CPU {} stole task {} (remaining tasks: {})",
                    self.hart_id,
                    task.pid(),
                    self.task_count()
                );
                
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
    
    // hart_id()已经在arch/hart.rs中做了边界检查，但为了安全再次验证
    if hart >= MAX_CORES {
        panic!("Invalid hart ID {} >= MAX_CORES {}", hart, MAX_CORES);
    }
    
    unsafe {
        if PER_CPU_PROCESSORS[hart].is_none() {
            PER_CPU_PROCESSORS[hart] = Some(Processor::new(hart));
        }
        PER_CPU_PROCESSORS[hart].as_mut().unwrap()
    }
}

pub fn add_task_to_cpu(cpu_id: usize, task: Arc<TaskControlBlock>) {
    let me = hart_id();
    
    // 强化边界检查并提供调试信息
    if cpu_id >= MAX_CORES {
        error!("Invalid CPU ID {} >= MAX_CORES {} in add_task_to_cpu", cpu_id, MAX_CORES);
        // 降级到当前CPU而不是panic，以提高系统鲁棒性
        let fallback_cpu = me;
        warn!("Falling back to current CPU {} for task {}", fallback_cpu, task.pid());
        add_task_to_cpu(fallback_cpu, task);
        return;
    }
    
    unsafe {
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
    let current_cpu = hart_id();
    
    // 检查边界
    if current_cpu >= MAX_CORES {
        error!("Invalid CPU ID {} in try_global_steal", current_cpu);
        return None;
    }
    
    // 实现工作窃取：从其他CPU偷取任务
    // 使用轮询方式检查其他CPU，避免总是从同一个CPU窃取
    let start_cpu = (current_cpu + 1) % MAX_CORES;
    
    unsafe {
        for i in 0..MAX_CORES {
            let target_cpu = (start_cpu + i) % MAX_CORES;
            
            // 跳过自己的CPU
            if target_cpu == current_cpu {
                continue;
            }
            
            // 检查目标CPU是否有处理器实例
            if let Some(target_processor) = &mut PER_CPU_PROCESSORS[target_cpu] {
                // 检查目标CPU是否活跃且有足够的任务
                if target_processor.active.load(Ordering::Acquire) {
                    let target_task_count = target_processor.task_count();
                    
                    // 只有当目标CPU有多个任务时才窃取（保持负载均衡）
                    if target_task_count > 1 {
                        if let Some(stolen_task) = target_processor.steal_task() {
                            debug!(
                                "CPU {} successfully stole task {} from CPU {}",
                                current_cpu,
                                stolen_task.pid(),
                                target_cpu
                            );
                            return Some(stolen_task);
                        }
                    }
                }
            }
        }
    }
    
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
