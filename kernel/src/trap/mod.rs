pub mod context;

use core::{
    arch::{asm, global_asm},
    panic,
};

pub use context::TrapContext;
use riscv::{
    ExceptionNumber, InterruptNumber,
    interrupt::{Exception, Interrupt, Trap},
    register::{
        self, sip, stval,
        stvec::{self, TrapMode},
    },
};

use crate::{
    memory::{TRAMPOLINE, TRAP_CONTEXT},
    syscall, timer,
};

global_asm!(include_str!("trap.S"));

unsafe extern "C" {
    unsafe fn __all_traps();
    unsafe fn __restore();
}

pub fn init() {
    set_kernel_trap_entry();

    unsafe {
        // 启用时钟中断
        register::sie::set_stimer();
    }
}

#[unsafe(no_mangle)]
pub fn trap_handler() -> ! {
    set_kernel_trap_entry();
    let scause_val = register::scause::read();
    let interrupt_type = scause_val.cause();
    // 在发生缺页异常时，保存导致问题的虚拟地址
    let stval = stval::read();

    if let Trap::Interrupt(code) = interrupt_type {
        if let Ok(interrupt) = Interrupt::from_number(code) {
            match interrupt {
                Interrupt::SupervisorTimer => {
                    timer::handle_supervisor_timer_interrupt();
                }
                Interrupt::SupervisorSoft => unsafe {
                    sip::clear_ssoft();
                },
                Interrupt::SupervisorExternal => {
                    println!("Supervisor external interrupt");
                }
                _ => {
                    panic!("Unknown interrupt: {:?}", interrupt);
                }
            }
        } else {
            panic!("Invalid interrupt code: {:?}", code);
        }
        return ctx;
    }

    let original_sepc = ctx.sepc;
    if let Trap::Exception(code) = interrupt_type {
        if let Ok(exception) = Exception::from_number(code) {
            match exception {
                Exception::InstructionMisaligned => {
                    panic!("Instruction misaligned");
                }
                Exception::InstructionFault => {
                    panic!("Instruction fault");
                }
                Exception::IllegalInstruction => {
                    panic!("Illegal instruction");
                }
                Exception::Breakpoint => {
                    // ebreak 指令，如果是标准的 ebreak (opcode 00100000000000000000000001110011), 它是 32-bit (4 bytes) 的。
                    // 如果是压缩指令集中的 c.ebreak (opcode 1001000000000010), 它是 16-bit (2 bytes) 的。
                    // 一个简单（但不完全鲁棒）的判断方法是检查指令的低两位：如果指令的低两位是 11，它是一个 32-bit 或更长的指令。
                    // 如果不是 11 (即 00, 01, 10)，它是一个 16-bit 压缩指令。
                    // 所以，对于 ebreak 或非法指令，如果需要跳过它，sepc 应该增加 2 或 4。
                    ctx.sepc += if (original_sepc & 0b11) != 0b11 { 2 } else { 4 };
                }
                Exception::LoadMisaligned => {
                    panic!("Load misaligned");
                }
                Exception::UserEnvCall => {
                    ctx.sepc += if (original_sepc & 0b11) != 0b11 { 2 } else { 4 };
                    let ret = syscall::syscall(ctx.x[17], [ctx.x[10], ctx.x[11], ctx.x[12]]);
                    ctx.x[10] = ret as usize;
                }
                Exception::StoreFault => {
                    panic!("Store fault");
                }
                Exception::InstructionPageFault => {
                    // 当 CPU 的取指单元 (Instruction Fetch Unit) 试图从一个虚拟地址获取下一条要执行的指令时，
                    // 如果该虚拟地址的转换失败或权限不足，就会发生指令缺页异常
                    panic!("Instruction Page Fault, VA:{:#x}", stval);
                }
                Exception::LoadPageFault => {
                    panic!("Load Page Fault, VA:{:#x}", stval)
                }
                Exception::StorePageFault => {
                    panic!("Store page fault, VA:{:#x}", stval)
                }
                _ => {
                    panic!("Trap exception: {:?} Not implemented", exception);
                }
            }
        } else {
            panic!("Invalid exception code: {:?}", code);
        }
        return ctx;
    }

    panic!("Can not handle trap: scause: {:?}", scause_val);
}

fn set_kernel_trap_entry() {
    let mut val = stvec::Stvec::from_bits(0);
    val.set_address(trap_from_kernel as usize);
    val.set_trap_mode(TrapMode::Direct);
    unsafe {
        stvec::write(val);
    }
}

fn set_user_trap_entry() {
    let mut val = stvec::Stvec::from_bits(0);
    val.set_address(trap_handler as usize);
    val.set_trap_mode(TrapMode::Direct);
    unsafe {
        stvec::write(val);
    }
}

#[unsafe(no_mangle)]
pub fn trap_return() -> ! {
    set_user_trap_entry();

    let trap_cx_ptr = TRAP_CONTEXT;
    let user_satp = current_user_token();
    let restore_va = __restore as usize - __all_traps as usize + TRAMPOLINE;
    unsafe {
        asm!(
            "fence.i",
            "jr {restore_va}",
            restore_va = in(reg) restore_va,
            in("x10") trap_cx_ptr,
            in("x11") user_satp,
            options(noreturn)
        )
    }
}

#[unsafe(no_mangle)]
pub fn trap_from_kernel() -> ! {
    panic!("a trap from kernel")
}
