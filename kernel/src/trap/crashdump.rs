use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use alloc::format;

use riscv::register::{scause, sepc, sstatus, stval};

use crate::{
    arch::hart::{hart_id, MAX_CORES},
    arch::sbi,
    console,
    memory::{KERNEL_SPACE, address::VirtualAddress, page_table::PTEFlags},
};

/// 标记是否已有 CPU 进入 panic 领导流程
static PANIC_IN_PROGRESS: AtomicBool = AtomicBool::new(false);

/// 已冻结 CPU 的位图（bit i 表示 hart i 已冻结并完成快照）
static FROZEN_MASK: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone, Copy)]
pub struct CpuCrashSnapshot {
    pub valid: bool,
    pub hart: usize,
    pub sepc: usize,
    pub scause_bits: usize,
    pub stval: usize,
    pub sstatus_bits: usize,
    pub sp: usize,
    pub fp: usize,
    pub ra: usize,
}

impl CpuCrashSnapshot {
    pub const fn empty() -> Self {
        Self {
            valid: false,
            hart: 0,
            sepc: 0,
            scause_bits: 0,
            stval: 0,
            sstatus_bits: 0,
            sp: 0,
            fp: 0,
            ra: 0,
        }
    }
}

// 每核快照存储（由各自 CPU 只写自己的槽位，领导核在输出时只读）
static mut SNAPSHOTS: [CpuCrashSnapshot; MAX_CORES] = [CpuCrashSnapshot::empty(); MAX_CORES];

#[inline]
pub fn panic_freeze_active() -> bool {
    PANIC_IN_PROGRESS.load(Ordering::Acquire)
}

#[inline]
fn set_frozen(hart: usize) {
    let bit = 1usize << hart;
    FROZEN_MASK.fetch_or(bit, Ordering::AcqRel);
}

#[inline]
fn is_all_frozen_except(except_hart: usize) -> bool {
    let active = active_hart_mask() & !(1usize << except_hart);
    let cur = FROZEN_MASK.load(Ordering::Acquire);
    (cur & active) == active
}

#[inline(always)]
fn read_sp_fp_ra() -> (usize, usize, usize) {
    let mut sp: usize;
    let mut fp: usize;
    let mut ra: usize;
    unsafe {
        core::arch::asm!(
            "mv {0}, sp\n\
             mv {1}, s0\n\
             mv {2}, ra",
            out(reg) sp,
            out(reg) fp,
            out(reg) ra,
        );
    }
    (sp, fp, ra)
}

fn capture_current_cpu_state() {
    let h = hart_id();
    let (sp, fp, ra) = read_sp_fp_ra();
    let snap = CpuCrashSnapshot {
        valid: true,
        hart: h,
        sepc: sepc::read(),
        scause_bits: scause::read().bits(),
        stval: stval::read(),
        sstatus_bits: sstatus::read().bits(),
        sp,
        fp,
        ra,
    };
    unsafe {
        SNAPSHOTS[h] = snap;
    }
}

fn broadcast_ipi_freeze(except_hart: usize) {
    let mask = active_hart_mask() & !(1usize << except_hart);
    if mask != 0 {
        let _ = sbi::sbi_send_ipi(mask, 0);
    }
}

#[inline]
fn active_hart_mask() -> usize {
    // 通过 HSM 查询处于 STARTED 的 hart
    let mut mask: usize = 0;
    for i in 0..MAX_CORES {
        if let Ok(status) = sbi::hart_get_status(i) {
            // SBI 规范：STARTED = 1
            if status == 1 {
                mask |= 1usize << i;
            }
        }
    }
    mask
}

fn print_snapshot(s: &CpuCrashSnapshot) {
    if !s.valid {
        return;
    }
    // 解码 scause
    let trap_cause = match s.scause_bits & ((1usize << (core::mem::size_of::<usize>() * 8 - 1)) - 1) {
        0 => "InstructionAddressMisaligned",
        1 => "InstructionAccessFault",
        2 => "IllegalInstruction",
        3 => "Breakpoint",
        4 => "LoadAddressMisaligned",
        5 => "LoadAccessFault",
        6 => "StoreAMOAddressMisaligned",
        7 => "StoreAMOAccessFault",
        8 => "EnvironmentCallFromU",
        9 => "EnvironmentCallFromS",
        12 => "InstructionPageFault",
        13 => "LoadPageFault",
        15 => "StoreAMOPageFault",
        _ => "Unknown",
    };
    let is_interrupt = (s.scause_bits >> (core::mem::size_of::<usize>() * 8 - 1)) != 0;

    // 查询内核页表中的映射与权限
    let (pa, pte_flags) = {
        let ks = KERNEL_SPACE.wait().lock();
        let va = VirtualAddress::from(s.sepc);
        let pa = ks.translate_va(va);
        let pte = ks.get_page_table().translate(va.floor());
        let flags = pte.map(|p| p.flags());
        (pa, flags)
    };
    let flags_str = if let Some(f) = pte_flags { alloc::format!("{:?}", f) } else { "<unmapped>".into() };
    let exec_ok = pte_flags.map(|f| f.contains(PTEFlags::X)).unwrap_or(false);

    console::panic_println_fmt(format_args!(
        "[PANIC] CPU{}: sepc={:#x} ({}) scause={:#x}{} stval={:#x} sstatus={:#x}",
        s.hart,
        s.sepc,
        if let Some(pa) = pa { alloc::format!("PA={:?}", pa) } else { "PA=None".into() },
        s.scause_bits,
        if is_interrupt { " (Interrupt)" } else { "" },
        s.stval,
        s.sstatus_bits
    ));
    console::panic_println_fmt(format_args!(
        "         sp={:#x} fp={:#x} ra={:#x} pte={}{}",
        s.sp,
        s.fp,
        s.ra,
        flags_str,
        if !exec_ok { " [non-exec]" } else { "" }
    ));
    // 结合 fp 做简易回溯
    print_backtrace_from_fp(s.fp);
}

fn print_all_snapshots(leader: usize) {
    // 先打印领导核
    unsafe { print_snapshot(&SNAPSHOTS[leader]); }
    // 再打印其它核
    for i in 0..MAX_CORES {
        if i == leader {
            continue;
        }
        let frozen = (FROZEN_MASK.load(Ordering::Acquire) >> i) & 1 == 1;
        let active = ((active_hart_mask() >> i) & 1) == 1;
        if frozen {
            unsafe { print_snapshot(&SNAPSHOTS[i]); }
        } else {
            if active {
                console::panic_println_fmt(format_args!("[PANIC] CPU{}: no response (not frozen)", i));
            } else {
                console::panic_println_fmt(format_args!("[PANIC] CPU{}: inactive (not started)", i));
            }
        }
    }
}

fn print_backtrace_from_fp(mut fp: usize) {
    // 基于常见栈帧布局：prologue 保存 ra/s0，s0 作为帧指针
    // 尽量避免越界访问：检查对齐和简单的地址范围特征
    const MAX_DEPTH: usize = 32;
    for depth in 0..MAX_DEPTH {
        if fp == 0 || (fp & 0x7) != 0 {
            break;
        }
        // 简单的高半区判定：许多内核栈地址在 0xffff... 范围
        // 但也允许低地址，以便在帧跨区时依然尝试打印
        unsafe {
            let prev_ra_ptr = (fp as *const usize).wrapping_sub(1);
            let prev_fp_ptr = (fp as *const usize).wrapping_sub(2);
            let prev_ra = core::ptr::read(prev_ra_ptr);
            let prev_fp = core::ptr::read(prev_fp_ptr);
            if prev_ra == 0 || prev_fp == 0 {
                break;
            }
            console::panic_println_fmt(format_args!("  bt#{:02}: ra={:#x} -> fp={:#x}", depth, prev_ra, prev_fp));
            if prev_fp == fp { break; }
            fp = prev_fp;
        }
    }
}

/// IPI 软中断下的冻结入口：保存寄存器并停机
pub fn ipi_freeze_entry() -> ! {
    unsafe { sstatus::clear_sie(); }
    capture_current_cpu_state();
    let h = hart_id();
    set_frozen(h);
    loop {
        riscv::asm::wfi();
    }
}

/// 由 panic CPU 调用：抢占领导权、广播 IPI、等待各核冻结并打印
pub fn leader_freeze_and_collect() -> ! {
    unsafe { sstatus::clear_sie(); }
    let me = hart_id();
    let i_am_leader = PANIC_IN_PROGRESS
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_ok();

    if !i_am_leader {
        // 其他 CPU 进入 panic，直接保存并停机
        ipi_freeze_entry();
    }

    // 领导核：先保存自身
    capture_current_cpu_state();
    set_frozen(me);

    console::panic_println_fmt(format_args!("============= PANIC FREEZE (leader CPU{}) =============", me));

    // 广播 IPI 让其它核保存并停机
    broadcast_ipi_freeze(me);

    // 等待其它核响应，周期性重发 IPI，设置有限循环避免死等
    let mut spins: usize = 0;
    while !is_all_frozen_except(me) && spins < 10_000_000 {
        if (spins & 0x7FFFF) == 0 { // 每 ~524k 次重发一次
            broadcast_ipi_freeze(me);
        }
        spins += 1;
        core::hint::spin_loop();
    }

    // 打印所有已收集的快照
    print_all_snapshots(me);

    console::panic_println_fmt(format_args!("============= PANIC HALT ============="));

    let _ = sbi::shutdown();
    loop {
        riscv::asm::wfi();
    }
}


