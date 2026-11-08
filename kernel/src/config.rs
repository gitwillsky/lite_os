//! 被 `arch` 与 `memory` 共同消费、且必须位于二者之下的无依赖常量。

/// Boot/secondary 的栈大小及 task 的栈槽位上限。
///
/// task 槽位底页不映射作 guard：32 KiB 槽实际映射 28 KiB；AArch64 顶页另存上下文，可用调用栈为 24 KiB。
/// 16 KiB 槽会只剩 AArch64 8 KiB、RISC-V 12 KiB，必须结合非零染色与编译器栈帧评估，不能引用旧零扫描推算余量。
/// 当前保留 32 KiB，完整负载评估由执行域契约记录；溢出在 guard page fail-stop，不能以压低统计值替代安全边界。
pub(crate) const KERNEL_STACK_SIZE: usize = 32 * 1024;
