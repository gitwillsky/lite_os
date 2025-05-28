pub mod context;

use core::arch::global_asm;

pub use context::TrapContext;
use riscv::{
    ExceptionNumber, InterruptNumber,
    interrupt::{Exception, Interrupt, Trap},
    register::{
        self, sip,
        stvec::{self, TrapMode},
    },
};

use crate::{syscall, timer};

global_asm!(include_str!("trap.S"));

pub fn init() {
    unsafe extern "C" {
        fn __alltraps();
    }
    unsafe {
        let mut val = stvec::Stvec::from_bits(0);
        val.set_address(__alltraps as usize);
        val.set_trap_mode(TrapMode::Direct);
        stvec::write(val);

        // 初始化 sscratch 寄存器为当前栈指针
        // 这样当中断发生时，sscratch 包含有效的内核栈地址
        let current_sp: usize;
        core::arch::asm!("mv {}, sp", out(reg) current_sp);
        register::sscratch::write(current_sp);

        // 使能中断
        register::sstatus::set_sie();
    }
    println!("Trap module initialized");
}

#[unsafe(no_mangle)]
pub fn trap_handler(ctx: &mut TrapContext) -> &mut TrapContext {
    let scause_val = register::scause::read();
    let interrupt_type = scause_val.cause();

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
