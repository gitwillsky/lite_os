use core::sync::atomic::{AtomicU32, Ordering};

use crate::{
    arch::hart::{MAX_CORES, hart_id},
    task::{self},
    timer,
};

#[derive(Debug, Clone, Copy)]
pub enum SoftIrq {
    Timer = 0,
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

#[inline(always)]
fn set_ssip() {
    unsafe { riscv::register::sip::set_ssoft() }
}

#[inline(always)]
pub fn raise(irq: SoftIrq) {
    let bit = 1u32 << irq.as_index();
    let cpu = hart_id();

    // Release 在置 SSIP 前发布 pending bit；consumer 的 AcqRel swap 获取该位。
    // 额外 fence 不会发布新的写，反而会掩盖真正的 request/consume 配对。
    PENDING[cpu].fetch_or(bit, Ordering::Release);
    set_ssip();
}

#[inline(always)]
fn take_pending_for(cpu: usize) -> u32 {
    assert!(cpu < MAX_CORES, "softirq CPU index out of range");
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
    // 唤醒到期的睡眠任务。
    task::wake_expired_tasks(timer::get_time_ns());
}
