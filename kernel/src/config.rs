//! 被 `arch` 与 `memory` 共同消费、且必须位于二者之下的无依赖常量。

/// Boot、secondary 与 task kernel stack 的统一大小（不含 guard page）。
///
/// 每个原生线程（包括每个 Linux 线程的承载线程）在创建时提交整个栈，因此它直接决定每线程的固定内存。
/// 文件系统与网络协议栈已移出内核，内核路径变浅：在 musl 门禁（6.5 万次 fork/exit、exec、信号与地址空间切换）
/// 中实测峰值深度 18648 字节（含 AArch64 顶部 4 KiB 的上下文保留），取约 1.7 倍余量的 32 KiB
/// （Linux arm64 为 16 KiB）。溢出落在 guard page，立即以缺页 fail-stop，不会静默破坏相邻对象。
pub(crate) const KERNEL_STACK_SIZE: usize = 32 * 1024;
