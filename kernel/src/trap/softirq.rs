use core::sync::atomic::{AtomicU32, Ordering};
use alloc::vec::Vec;
use lazy_static::lazy_static;

use crate::{
    arch::hart::{hart_id, MAX_CORES},
    task,
    timer,
    watchdog,
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
    pub fn as_index(&self) -> usize { *self as usize }
}

// 每核挂起的软中断位图（驻留于堆内已映射内存，避免直接操作未映射物理地址）
lazy_static! {
    static ref PENDING: Vec<AtomicU32> = {
        let mut v = Vec::with_capacity(MAX_CORES);
        for _ in 0..MAX_CORES { v.push(AtomicU32::new(0)); }
        v
    };
}

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
    let cpu = hart_id();
    PENDING[cpu].fetch_or(bit, Ordering::AcqRel);
    set_ssip();
}

#[inline(always)]
fn take_pending_for(cpu: usize) -> u32 {
    PENDING[cpu].swap(0, Ordering::AcqRel)
}

#[inline(always)]
pub fn dispatch_current_cpu() {
    let cpu = hart_id();
    let mask = take_pending_for(cpu);
    if (mask & (1u32 << SoftIrq::Timer.as_index())) != 0 {
        handle_timer_softirq();
    }
}

#[inline(always)]
fn handle_timer_softirq() {
    // 看门狗、睡眠唤醒、信号检查与调度
    watchdog::check();
    task::check_and_wakeup_sleeping_tasks(timer::get_time_ns());
    let cx = task::current_trap_context();
    // 退回公开接口：软中断仅做唤醒，不在此处退出进程，避免访问私有函数
    let _ = cx; // 保留现有语义：调度点触发调度
    super::task::suspend_current_and_run_next();
}


