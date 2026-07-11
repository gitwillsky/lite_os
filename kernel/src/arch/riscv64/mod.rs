use core::arch::global_asm;

pub(crate) mod dtb;
#[macro_use]
pub(crate) mod console;
pub(crate) mod hart;
pub(crate) mod sbi;
mod start;

pub(crate) use start::entry_address as hart_start_entry;

global_asm!(include_str!("trap.S"));
global_asm!(include_str!("switch.S"));
