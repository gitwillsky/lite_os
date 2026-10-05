//! 被 `arch` 与 `memory` 共同消费、且必须位于二者之下的无依赖常量。

/// Boot、secondary 与 task kernel stack 的统一大小。
pub(crate) const KERNEL_STACK_SIZE: usize = 8192 * 16;
