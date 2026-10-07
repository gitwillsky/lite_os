//! `open(2)` / `fcntl(F_SETFL)` 的 status flag 位（Linux AArch64/RISC-V generic ABI）。
//!
//! 属于 VFS 的打开语义；OFD 层与设备/FIFO 打开都从这里取位定义。

pub(crate) const O_ACCMODE: u32 = 3;
pub(crate) const O_RDONLY: u32 = 0;
pub(crate) const O_WRONLY: u32 = 1;
pub(crate) const O_RDWR: u32 = 2;
pub(crate) const O_APPEND: u32 = 0x400;
pub(crate) const O_NONBLOCK: u32 = 0x800;
pub(crate) const O_CLOEXEC: u32 = 0x80000;
