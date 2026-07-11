use core::arch::global_asm;

pub mod dtb;
#[macro_use]
pub mod console;
pub(crate) mod sbi;
pub mod hart;
mod start;

global_asm!(include_str!("trap.S"));
global_asm!(include_str!("switch.S"));
