//! Linux `open(2)` / `fcntl(2)` 的 native flag 编码。
//!
//! AArch64 的 directory/nofollow/direct/largefile 位来自 arm64 UAPI，不能使用
//! asm-generic 编码：否则 musl 隐式加入的 O_LARGEFILE 会被误判成 O_NOFOLLOW。
//! 此模块仅在编译期选择 ABI；VFS/OFD 消费同一编码，不复制或翻译 status 状态。

pub const O_ACCMODE: u32 = 3;
pub const O_RDONLY: u32 = 0;
pub const O_WRONLY: u32 = 1;
pub const O_RDWR: u32 = 2;
pub const O_CREAT: u32 = 0x40;
pub const O_EXCL: u32 = 0x80;
/// 阻止打开 TTY 时自动取得 controlling terminal。
pub const O_NOCTTY: u32 = 0x100;
pub const O_TRUNC: u32 = 0x200;
pub const O_APPEND: u32 = 0x400;
pub const O_NONBLOCK: u32 = 0x800;
pub const O_CLOEXEC: u32 = 0x80000;
/// 只持有 path；不打开 backend 或取得内容访问权。
pub const O_PATH: u32 = 0x200000;

/// 要求末项为目录，编码属于目标架构的 Linux UAPI。
#[cfg(target_arch = "aarch64")]
pub const O_DIRECTORY: u32 = 0o040000;
#[cfg(not(target_arch = "aarch64"))]
pub const O_DIRECTORY: u32 = 0o200000;
/// 禁止跟随末项 symlink；不能与 native O_LARGEFILE 混淆。
#[cfg(target_arch = "aarch64")]
pub const O_NOFOLLOW: u32 = 0o100000;
#[cfg(not(target_arch = "aarch64"))]
pub const O_NOFOLLOW: u32 = 0o400000;
/// Linux 的 direct-I/O 请求位，不改变普通打开的 directory 约束。
#[cfg(target_arch = "aarch64")]
pub const O_DIRECT: u32 = 0o200000;
#[cfg(not(target_arch = "aarch64"))]
pub const O_DIRECT: u32 = 0o040000;
/// 64-bit 文件偏移标志；不能改变末项链接跟随行为。
#[cfg(target_arch = "aarch64")]
pub const O_LARGEFILE: u32 = 0o400000;
#[cfg(not(target_arch = "aarch64"))]
pub const O_LARGEFILE: u32 = 0o100000;
