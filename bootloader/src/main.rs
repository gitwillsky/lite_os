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
mod riscv_spec;
mod trap_stack;
mod trap_vec;
mod uart16550;

use constants::KERNEL_ENTRY;
use core::{
    arch::{asm, naked_asm},
    mem::MaybeUninit,
    sync::atomic::{AtomicBool, Ordering},
};
use device_tree::BoardInfo;
use fast_trap::{FastContext, FastResult};
use hart::hart_id;
use riscv_spec::{mepc, mie, mstatus};
use rustsbi::{RustSBI, SbiRet};
use spin::Once;
use trap_stack::{local_hsm, local_remote_hsm, remote_hsm};

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
    static GENESIS: AtomicBool = AtomicBool::new(true);
    static BOARD_INFO: Once<BoardInfo> = Once::new();
    // 全局初始化过程
    if GENESIS.swap(false, Ordering::Acquire) {
        unsafe extern "C" {
            unsafe static mut sbss: u64;
            unsafe static mut ebss: u64;
        }
        unsafe {
            let mut ptr = sbss as *mut u64;
            let end = ebss as *mut u64;
            while ptr < end {
                ptr.write_volatile(0);
                ptr = ptr.offset(1);
            }
        }

        // 解析设备树
        let board_info = BOARD_INFO.call_once(|| device_tree::parse(opaque));
        // 初始化外设
        uart16550::init(board_info.uart.start);
        console::init_console(&Console);
        console::set_log_level(option_env!("LOG"));
        clint::init(board_info.clint.start);
        qemu_test::init(board_info.test.start);
        dbcn::init(KERNEL_ENTRY..board_info.mem.end);

        // print startup information
        print!(
            "\
[rustsbi] RustSBI version {ver_sbi}, adapting to RISC-V SBI v2.0.0
[rustsbi] Implementation     : RustSBI-QEMU Version {ver_impl}
[rustsbi] Platform Name      : {model}
[rustsbi] Platform SMP       : {smp}
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
            mem_addr = board_info.mem,
            mem_size = (board_info.mem.end - board_info.mem.start) / (1024 * 1024),
            dtb = board_info.dtb,
            firmware = _start as usize,
        );

        // 初始化 SBI
        unsafe {
            SBI = MaybeUninit::new(FixedRustSBI {
                clint: &clint::Clint,
                hsm: Hsm,
                reset: qemu_test::get(),
                dbcn: dbcn::get(),
            });
        }
        // 设置并打印 pmp
        set_pmp(board_info);
        hart_csr_utils::print_pmps();
        // 设置陷入栈
        trap_stack::prepare_for_trap();
        // 设置内核入口
        local_remote_hsm().start(Supervisor {
            start_addr: KERNEL_ENTRY,
            opaque,
        });
    } else {
        // 设置 pmp
        set_pmp(BOARD_INFO.wait());
        // 设置陷入栈
        trap_stack::prepare_for_trap();
    }
    // 清理 clint
    clint::clear();
    // 准备启动调度
    unsafe {
        asm!("csrw mideleg,    {}", in(reg) !0);
        asm!("csrw medeleg,    {}", in(reg) !0);
        asm!("csrw mcounteren, {}", in(reg) !0);
        use riscv::register::{medeleg, mtvec};
        medeleg::clear_supervisor_env_call();
        medeleg::clear_machine_env_call();
        mtvec::write(trap_vec::trap_vec as _, mtvec::TrapMode::Vectored);
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
        mcause::{self, Trap as T},
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
                    *bits &= !mstatus::MPP;
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
                T::Exception(_supervisor_env_call) => {
                    use sbi_spec::{base, hsm, legacy};
                    let mut ret = unsafe { (*core::ptr::addr_of_mut!(SBI)).assume_init_mut() }
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
                            // legacy console 探测
                            (base::EID_BASE, base::PROBE_EXTENSION)
                                if matches!(
                                    ctx.a0(),
                                    legacy::LEGACY_CONSOLE_PUTCHAR | legacy::LEGACY_CONSOLE_GETCHAR
                                ) =>
                            {
                                ret.value = 1;
                            }
                            _ => {}
                        }
                    } else {
                        match a7 {
                            legacy::LEGACY_CONSOLE_PUTCHAR => {
                                print!("{}", ctx.a0() as u8 as char);
                                ret.error = 0;
                                ret.value = a1;
                            }
                            legacy::LEGACY_CONSOLE_GETCHAR => {
                                let mut c = 0u8;
                                let uart = uart16550::UART.lock();
                                loop {
                                    if uart.get().read(core::slice::from_mut(&mut c)) == 1 {
                                        ret.error = c as _;
                                        ret.value = a1;
                                        break;
                                    }
                                }
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
    println!("[rustsbi-panic] hart {} {info}", hart_id());
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

static mut SBI: MaybeUninit<FixedRustSBI> = MaybeUninit::uninit();

#[derive(RustSBI)]
struct FixedRustSBI<'a> {
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
