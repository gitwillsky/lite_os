#![no_std]
#![no_main]

mod aclint;
mod clint;
#[macro_use]
mod console;
mod constants;
mod dbcn;
mod device_tree;
mod fast_trap;
mod hart;
mod hart_csr_utils;
mod hsm_cell;
mod qemu_test;
mod rfence;
mod riscv_spec;
mod trap_stack;
mod trap_vec;
mod uart16550;

use constants::KERNEL_ENTRY;
use core::{
    arch::{asm, naked_asm},
    sync::atomic::{AtomicUsize, Ordering},
};
use device_tree::BoardInfo;
use fast_trap::{FastContext, FastResult};
use hart::hart_id;
use riscv_spec::{mepc, mie, mstatus};
use rustsbi::{RustSBI, SbiRet};
use spin::Once;
use trap_stack::{local_hsm, local_remote_hsm, remote_hsm};

const GLOBAL_PENDING: usize = 0x474c_4250;
const GLOBAL_INITIALIZING: usize = 0x474c_4249;
const GLOBAL_READY: usize = 0x474c_4252;
const DELEGATED_S_INTERRUPTS: usize = (1 << 1) | (1 << 5) | (1 << 9);
const DELEGATED_U_EXCEPTIONS: usize = (1 << 0)
    | (1 << 1)
    | (1 << 2)
    | (1 << 3)
    | (1 << 4)
    | (1 << 5)
    | (1 << 6)
    | (1 << 7)
    | (1 << 8)
    | (1 << 12)
    | (1 << 13)
    | (1 << 15);
const COUNTER_TIME: usize = 1 << 1;

// 非零初值保证状态位于 .data；其他 hart 读取 GLOBAL_READY(Acquire) 前不得访问正被清零的 BSS。
static GLOBAL_STATE: AtomicUsize = AtomicUsize::new(GLOBAL_PENDING);
static READY_HARTS: AtomicUsize = AtomicUsize::new(0);
static BOARD_INFO: Once<BoardInfo> = Once::new();
static SBI: Once<FixedRustSBI<'static>> = Once::new();

#[unsafe(naked)]
#[unsafe(no_mangle)]
#[unsafe(link_section = ".text.entry")]
unsafe extern "C" fn _start() -> ! {
    naked_asm!(
        "  call {locate_stack}
           call {rust_main}
           j {trap}
        ",
        locate_stack = sym trap_stack::locate,
        rust_main = sym rust_main,
        trap = sym trap_vec::trap_vec,
    )
}

extern "C" fn rust_main(hart_id: usize, opaque: usize) {
    let is_cold_boot = GLOBAL_STATE
        .compare_exchange(
            GLOBAL_PENDING,
            GLOBAL_INITIALIZING,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .is_ok();

    if is_cold_boot {
        clear_bss();
        // 解析设备树
        let board_info = BOARD_INFO.call_once(|| device_tree::parse(opaque));
        // 初始化外设
        uart16550::init(board_info.uart.start);
        console::init_console(&Console);
        console::set_log_level(option_env!("LOG"));
        clint::init(board_info.clint.start);
        qemu_test::init(board_info.test.start);
        dbcn::init(KERNEL_ENTRY..board_info.mem.end);
        validate_board_info(board_info, hart_id);

        // print startup information
        print!(
            "\
[rustsbi] RustSBI version {ver_sbi}, adapting to RISC-V SBI v2.0.0
[rustsbi] Implementation     : RustSBI-QEMU Version {ver_impl}
[rustsbi] Platform Name      : {model}
[rustsbi] Platform SMP       : {smp}
[rustsbi] Platform HART Mask : {hart_mask:#x}
[rustsbi] Platform Memory    : {mem_addr:#x?},{mem_size}MiB
[rustsbi] Boot HART          : {hart_id}
[rustsbi] Device Tree Region : {dtb:#x?}
[rustsbi] Firmware Address   : {firmware:#x}
[rustsbi] Supervisor Address : {KERNEL_ENTRY:#x}
",
            ver_sbi = rustsbi::VERSION,
            ver_impl = env!("CARGO_PKG_VERSION"),
            model = board_info.model,
            smp = board_info.smp,
            hart_mask = board_info.hart_mask,
            mem_addr = board_info.mem,
            mem_size = (board_info.mem.end - board_info.mem.start) / (1024 * 1024),
            dtb = board_info.dtb,
            firmware = _start as usize,
        );

        // 初始化 SBI
        SBI.call_once(|| FixedRustSBI {
            rfence: rfence::Rfence,
            clint: &clint::Clint,
            hsm: Hsm,
            reset: qemu_test::get(),
            dbcn: dbcn::get(),
        });
        // Release 发布 BSS 清零、BoardInfo、设备指针和 RustSBI；secondary 的 Acquire
        // 在访问任一全局对象前消费这些写。
        GLOBAL_STATE.store(GLOBAL_READY, Ordering::Release);
    } else {
        while GLOBAL_STATE.load(Ordering::Acquire) != GLOBAL_READY {
            core::hint::spin_loop();
        }
    }

    let board_info = BOARD_INFO.wait();
    assert!(
        board_info.hart_mask & (1usize << hart_id) != 0,
        "hart {} is absent from DTB hart mask {:#x}",
        hart_id,
        board_info.hart_mask
    );
    set_pmp(board_info);
    if is_cold_boot {
        hart_csr_utils::print_pmps();
    }
    trap_stack::prepare_for_trap();

    // Release 发布当前 hart 的 HSM cell/trap stack 初始化；cold-boot hart 在写入
    // remote HSM start payload 前以 Acquire 等待完整 mask，避免 payload 被迟到的 init 覆盖。
    READY_HARTS.fetch_or(1usize << hart_id, Ordering::Release);
    if is_cold_boot {
        while READY_HARTS.load(Ordering::Acquire) & board_info.hart_mask != board_info.hart_mask {
            core::hint::spin_loop();
        }
        assert!(
            local_remote_hsm().start(Supervisor {
                start_addr: KERNEL_ENTRY,
                opaque,
            }),
            "cold-boot hart HSM was not stopped"
        );
        start_all_cores(board_info, opaque);
    }

    // 清理 clint
    clint::clear();
    // 准备启动调度
    unsafe {
        // 只委托 S-mode 实际消费的中断与来自 U-mode 的异常；S-mode ecall 必须留在 M-mode 作为 SBI。
        asm!("csrw mideleg,    {}", in(reg) DELEGATED_S_INTERRUPTS);
        asm!("csrw medeleg,    {}", in(reg) DELEGATED_U_EXCEPTIONS);
        // kernel 单调时钟只需要 `time` CSR，不向 S-mode 暴露未使用的硬件计数器。
        asm!("csrw mcounteren, {}", in(reg) COUNTER_TIME);
        use riscv::register::mtvec;
        mtvec::write(trap_vec::trap_vec as _, mtvec::TrapMode::Vectored);
    }
}

fn clear_bss() {
    unsafe extern "C" {
        unsafe static mut sbss: u64;
        unsafe static mut ebss: u64;
    }
    unsafe {
        let mut ptr = sbss as *mut u64;
        let end = ebss as *mut u64;
        while ptr < end {
            ptr.write_volatile(0);
            ptr = ptr.add(1);
        }
    }
}

fn validate_board_info(board_info: &BoardInfo, cold_boot_hart: usize) {
    assert!(board_info.smp != 0, "DTB contains no enabled hart");
    assert!(
        board_info.invalid_hart_id.is_none(),
        "DTB hart ID {} exceeds firmware limit {}",
        board_info.invalid_hart_id.unwrap_or(usize::MAX),
        constants::MAX_HART_NUM
    );
    assert_eq!(
        board_info.hart_mask.count_ones() as usize,
        board_info.smp,
        "DTB CPU count and unique hart mask disagree"
    );
    assert!(
        board_info.hart_mask & (1usize << cold_boot_hart) != 0,
        "cold-boot hart {} is absent from DTB",
        cold_boot_hart
    );
}

/// 启动所有其他核心
fn start_all_cores(board_info: &BoardInfo, opaque: usize) {
    use crate::hart::hart_id;

    let current_hart = hart_id();
    println!(
        "[rustsbi] Starting {} cores (current: {})",
        board_info.smp, current_hart
    );

    // 只按 DTB 的实际 hart mask 启动，不能把 CPU 数量误当作连续 hart ID 上界。
    for hart_id in 0..constants::MAX_HART_NUM {
        if board_info.hart_mask & (1usize << hart_id) == 0 {
            continue;
        }
        if hart_id != current_hart {
            match remote_hsm(hart_id) {
                Some(remote) => {
                    if remote.start(Supervisor {
                        start_addr: KERNEL_ENTRY,
                        opaque,
                    }) {
                        clint::set_msip(hart_id);
                        println!("[rustsbi] Successfully started core {}", hart_id);
                    } else {
                        panic!("ready hart {} HSM was not stopped", hart_id);
                    }
                }
                None => {
                    panic!("validated DTB hart {} has no HSM slot", hart_id);
                }
            }
        }
    }
}

/// 设置 PMP，物理内存保护
fn set_pmp(board_info: &BoardInfo) {
    use riscv::register::*;
    let mem = &board_info.mem;
    unsafe {
        pmpcfg0::set_pmp(0, Range::OFF, Permission::NONE, false);
        pmpaddr0::write(0);
        // 外设
        pmpcfg0::set_pmp(1, Range::TOR, Permission::RW, false);
        pmpaddr1::write(mem.start >> 2);
        // SBI
        pmpcfg0::set_pmp(2, Range::TOR, Permission::NONE, false);
        pmpaddr2::write(KERNEL_ENTRY >> 2);
        // 主存
        pmpcfg0::set_pmp(3, Range::TOR, Permission::RWX, false);
        pmpaddr3::write(mem.end >> 2);
        // 其他
        pmpcfg0::set_pmp(4, Range::TOR, Permission::RW, false);
        pmpaddr4::write(1 << (usize::BITS - 1));
    }
}

extern "C" fn fast_handler(
    mut ctx: FastContext,
    a1: usize,
    a2: usize,
    a3: usize,
    a4: usize,
    a5: usize,
    a6: usize,
    a7: usize,
) -> FastResult {
    use riscv::register::{
        mcause::{self, Exception as E, Trap as T},
        mtval, satp, sstatus,
    };

    #[inline]
    fn boot(mut ctx: FastContext, start_addr: usize, opaque: usize) -> FastResult {
        unsafe {
            sstatus::clear_sie();
            satp::write(0);
        }
        ctx.regs().a[0] = hart_id();
        ctx.regs().a[1] = opaque;
        ctx.regs().pc = start_addr;
        ctx.call(2)
    }
    loop {
        match local_hsm().start() {
            Ok(supervisor) => {
                mstatus::update(|bits| {
                    // 清除前一执行环境的 MPRV/MIE 与 MPP，防止 S-mode 以内存访问特权继承 M-mode 状态。
                    *bits &= !(mstatus::MPP | mstatus::MPRV | mstatus::MIE);
                    *bits |= mstatus::MPIE | mstatus::MPP_SUPERVISOR;
                });
                mie::write(mie::MSIE | mie::MTIE);
                break boot(ctx, supervisor.start_addr, supervisor.opaque);
            }
            Err(rustsbi::spec::hsm::HART_STOP) => {
                mie::write(mie::MSIE);
                unsafe { riscv::asm::wfi() };
                clint::clear_msip();
            }
            _ => match mcause::read().cause() {
                // SBI call
                T::Exception(E::SupervisorEnvCall) => {
                    use sbi_spec::hsm;
                    let ret = SBI
                        .wait()
                        .handle_ecall(a7, a6, [ctx.a0(), a1, a2, a3, a4, a5]);
                    if ret.is_ok() {
                        match (a7, a6) {
                            // 关闭
                            (hsm::EID_HSM, hsm::HART_STOP) => continue,
                            // 不可恢复挂起
                            (hsm::EID_HSM, hsm::HART_SUSPEND)
                                if matches!(ctx.a0() as u32, hsm::suspend_type::NON_RETENTIVE) =>
                            {
                                break boot(ctx, a1, a2);
                            }
                            _ => {}
                        }
                    }
                    ctx.regs().a = [ret.error, ret.value, a2, a3, a4, a5, a6, a7];
                    mepc::next();
                    break ctx.restore();
                }
                // 其他陷入
                trap => {
                    println!(
                        "
-----------------------------
> trap:    {trap:?}
> mstatus: {:#018x}
> mepc:    {:#018x}
> mtval:   {:#018x}
-----------------------------
            ",
                        mstatus::read(),
                        mepc::read(),
                        mtval::read()
                    );
                    panic!("stopped with unsupported trap")
                }
            },
        }
    }
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    use rustsbi::{
        Reset,
        spec::srst::{RESET_REASON_SYSTEM_FAILURE, RESET_TYPE_SHUTDOWN},
    };
    // 输出的信息大概是“[rustsbi-panic] hart 0 panicked at ...”
    println!("[rustsbi-panic] hart {} {info}", hart::raw_hart_id());
    println!("[rustsbi-panic] system shutdown scheduled due to RustSBI panic");
    qemu_test::get().system_reset(RESET_TYPE_SHUTDOWN, RESET_REASON_SYSTEM_FAILURE);
    unreachable!()
}

/// 特权软件信息。
#[derive(Debug)]
struct Supervisor {
    start_addr: usize,
    opaque: usize,
}

struct Console;

impl console::Console for Console {
    #[inline]
    fn put_char(&self, c: u8) {
        let uart = uart16550::UART.lock();
        while uart.get().write(&[c]) == 0 {
            core::hint::spin_loop();
        }
    }

    #[inline]
    fn put_str(&self, s: &str) {
        let uart = uart16550::UART.lock();
        let mut bytes = s.as_bytes();
        while !bytes.is_empty() {
            let count = uart.get().write(bytes);
            bytes = &bytes[count..];
        }
    }
}

#[derive(RustSBI)]
struct FixedRustSBI<'a> {
    #[rustsbi(fence)]
    rfence: rfence::Rfence,
    #[rustsbi(ipi, timer)]
    clint: &'a clint::Clint,
    hsm: Hsm,
    reset: &'a qemu_test::QemuTest,
    dbcn: &'a dbcn::DBCN,
}

struct Hsm;

impl rustsbi::Hsm for Hsm {
    fn hart_start(&self, hartid: usize, start_addr: usize, opaque: usize) -> SbiRet {
        match remote_hsm(hartid) {
            Some(remote) => {
                if remote.start(Supervisor { start_addr, opaque }) {
                    clint::set_msip(hartid);
                    SbiRet::success(0)
                } else {
                    SbiRet::already_started()
                }
            }
            None => SbiRet::invalid_param(),
        }
    }

    #[inline]
    fn hart_stop(&self) -> SbiRet {
        local_hsm().stop();
        SbiRet::success(0)
    }

    #[inline]
    fn hart_get_status(&self, hartid: usize) -> SbiRet {
        match remote_hsm(hartid) {
            Some(remote) => SbiRet::success(remote.sbi_get_status()),
            None => SbiRet::invalid_param(),
        }
    }

    fn hart_suspend(&self, suspend_type: u32, _resume_addr: usize, _opaque: usize) -> SbiRet {
        use rustsbi::spec::hsm::suspend_type::{NON_RETENTIVE, RETENTIVE};
        if matches!(suspend_type, NON_RETENTIVE | RETENTIVE) {
            local_hsm().suspend();
            unsafe { riscv::asm::wfi() };
            local_hsm().resume();
            SbiRet::success(0)
        } else {
            SbiRet::not_supported()
        }
    }
}
