//! CPU 执行：逻辑 CPU、deferred work、启动/trap 入口编解码、中断契约、同步原语、时钟与 trap 分派。
pub(crate) mod cpu;
pub(crate) mod deferred;
pub(crate) mod entry;
pub(crate) mod interrupt;
pub(crate) mod sync;
pub(crate) mod timer;
pub(crate) mod trap;
