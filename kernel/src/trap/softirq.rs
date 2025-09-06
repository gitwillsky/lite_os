use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use alloc::sync::Arc;

use crate::{
    arch::hart::{MAX_CORES, hart_id},
    task::{self, TaskStatus},
    timer, watchdog,
};

#[derive(Debug, Clone, Copy)]
pub enum SoftIrq {
    Timer = 0,
    Network = 1,
    Block = 2,
    Tasklet = 3,
    Sched = 4,
    Hrtimer = 5,
    Rcu = 6,
}

impl SoftIrq {
    #[inline(always)]
    pub fn as_index(&self) -> usize {
        *self as usize
    }
}

// 每核挂起的软中断位图 - 使用固定大小数组避免Vec的潜在问题
static PENDING: [AtomicU32; MAX_CORES] = [
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
];

// 软中断处理状态，防止重入
static SOFTIRQ_ACTIVE: [AtomicBool; MAX_CORES] = [
    AtomicBool::new(false),
    AtomicBool::new(false),
    AtomicBool::new(false),
    AtomicBool::new(false),
    AtomicBool::new(false),
    AtomicBool::new(false),
    AtomicBool::new(false),
    AtomicBool::new(false),
];

#[inline(always)]
fn set_ssip() {
    // 置位 SSIP (Supervisor Software Interrupt)
    unsafe {
        let mut val: usize;
        core::arch::asm!("csrr {0}, sip", out(reg) val);
        val |= 1 << 1; // SSIP
        core::arch::asm!("csrw sip, {0}", in(reg) val);
    }
}

#[inline(always)]
pub fn raise(irq: SoftIrq) {
    let bit = 1u32 << irq.as_index();
    let cpu = hart_id(); // hart_id()现在已经有边界检查
    
    // 额外的防御性边界检查
    if cpu >= MAX_CORES {
        error!("Invalid CPU ID {} >= MAX_CORES {} in softirq::raise", cpu, MAX_CORES);
        return;
    }

    PENDING[cpu].fetch_or(bit, Ordering::AcqRel);
    // 内存屏障确保位图更新在中断触发前完成
    core::sync::atomic::fence(Ordering::Release);
    set_ssip();
}

#[inline(always)]
fn take_pending_for(cpu: usize) -> u32 {
    // 额外的防御性边界检查
    if cpu >= MAX_CORES {
        error!("Invalid CPU ID {} >= MAX_CORES {} in take_pending_for", cpu, MAX_CORES);
        return 0;
    }
    
    PENDING[cpu].swap(0, Ordering::AcqRel)
}

#[inline(always)]
pub fn dispatch_current_cpu() {
    let cpu = hart_id(); // hart_id()现在已经有边界检查
    
    // 额外的防御性边界检查
    if cpu >= MAX_CORES {
        error!("Invalid CPU ID {} >= MAX_CORES {} in dispatch_current_cpu", cpu, MAX_CORES);
        return;
    }

    // 防止软中断重入 - 但允许在不同CPU上并发处理
    if SOFTIRQ_ACTIVE[cpu]
        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        // 已经在处理软中断，避免重入
        return;
    }

    let mask = take_pending_for(cpu);

    // 立即释放软中断锁，允许后续软中断
    // 这很重要，因为handle_timer_softirq可能会切换任务，而输入处理需要及时响应
    SOFTIRQ_ACTIVE[cpu].store(false, Ordering::Release);

    // 然后处理软中断
    if (mask & (1u32 << SoftIrq::Timer.as_index())) != 0 {
        handle_timer_softirq();
    }
    if (mask & (1u32 << SoftIrq::Tasklet.as_index())) != 0 {
        crate::drivers::virtio_input::drain_all_input_devices();
    }
}

#[inline(always)]
fn handle_timer_softirq() {
    // 看门狗、睡眠唤醒
    watchdog::check();
    task::check_and_wakeup_sleeping_tasks(timer::get_time_ns());

    // 在软中断上下文中，我们需要谨慎处理任务切换
    // 只有在有当前任务且不在内核关键路径时才触发调度
    // 避免在idle循环或其他特殊上下文中调度
    if let Some(task) = task::current_task() {
        if !task.is_zombie() && *task.task_status.lock() == crate::task::TaskStatus::Running {
            let now = timer::get_time_ns();
            let slice_us = task.sched.lock().time_slice;
            let slice_ns = slice_us.saturating_mul(1000);
            let last = task
                .last_runtime
                .load(core::sync::atomic::Ordering::Relaxed) as u64
                * 1000;
            let ran_ns = now.saturating_sub(last);
            let ready_exists = crate::task::current_processor().task_count() > 0;
            if ready_exists && ran_ns >= slice_ns / 2 {
                // 在软中断上下文中标记需要重新调度，但不立即切换
                task::mark_need_resched();
                debug!(
                    "Timer softirq: marked task {} for preemption (runtime exceeded)",
                    task.pid()
                );
            }
        }
    }
}

