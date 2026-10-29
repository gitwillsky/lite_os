//! CPU 执行、调度、trap 与同步；各子领域分别拥有状态与依赖契约。
pub(crate) mod cpu;
pub(crate) mod deferred;
pub(crate) mod entry;
pub(crate) mod interrupt;
pub(crate) mod sync;
pub(crate) mod task;
pub(crate) mod timer;
pub(crate) mod trap;
