use core::sync::atomic::{AtomicU32, Ordering};

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

// 每核挂起的软中断位图
static mut PENDING: *const AtomicU32 = core::ptr::null();

#[inline(always)]
fn ensure_init() -> &'static [AtomicU32] {
    use core::mem::{align_of, size_of};
    use crate::memory::{frame_allocator, PAGE_SIZE};
    unsafe {
        if PENDING.is_null() {
            // 分配一页以上的内存用于保存 MAX_CORES 个 AtomicU32
            let bytes = ((MAX_CORES * size_of::<AtomicU32>() + PAGE_SIZE - 1) / PAGE_SIZE) as usize;
            let frame = frame_allocator::alloc_contiguous(bytes).expect("alloc softirq pending");
            let base = (frame.ppn.as_usize() * PAGE_SIZE) as *mut u8;
            // 零初始化
            for i in 0..(bytes * PAGE_SIZE) { base.add(i).write_volatile(0); }
            PENDING = base as *const AtomicU32;
        }
        core::slice::from_raw_parts(PENDING, MAX_CORES)
    }
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
    let pending = ensure_init();
    pending[cpu].fetch_or(bit, Ordering::AcqRel);
    set_ssip();
}

#[inline(always)]
fn take_pending_for(cpu: usize) -> u32 {
    let pending = ensure_init();
    pending[cpu].swap(0, Ordering::AcqRel)
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


