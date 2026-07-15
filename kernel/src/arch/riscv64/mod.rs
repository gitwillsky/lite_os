use core::arch::global_asm;

pub(crate) mod dtb;
#[macro_use]
pub(crate) mod console;
pub(crate) mod hart;
mod io;
pub(crate) mod sbi;
mod start;

pub(crate) use io::before_mmio_write;
pub(crate) use start::entry_address as hart_start_entry;

global_asm!(include_str!("trap.S"));
global_asm!(include_str!("switch.S"));
