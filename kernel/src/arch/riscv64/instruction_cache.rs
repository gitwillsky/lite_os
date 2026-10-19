/// 发布当前 CPU 在此之前写入的可执行指令字节。
///
/// data fence 先让写入对其他 hart 可观察，`fence.i` 再同步本 hart 的 instruction fetch。
/// remote hart 由 platform RFENCE 同步；缺失前一条 fence 时，远端可能在数据写尚未可见时
/// 提前完成自己的 `fence.i`。
pub(crate) fn publish_local() {
    // SAFETY: both fences affect only architectural ordering/cache state and do not access memory.
    unsafe { core::arch::asm!("fence rw, rw", "fence.i", options(nostack)) };
}

/// 发布给定物理范围内的新 executable view。
///
/// RISC-V `fence.i` 不编码地址范围；参数只维持 generic transaction 的静态 seam，生产指令
/// 序列仍严格复用原有 `fence rw, rw; fence.i`。
pub(crate) fn publish_range(_physical_start: usize, _size: usize) {
    publish_local();
}

/// 在 CPU 上线前丢弃 firmware/boot 阶段可能保留的 instruction-cache state。
pub(crate) fn initialize_local() {
    // SAFETY: startup owns this hart and has not exposed it to the scheduler.
    unsafe { core::arch::asm!("fence.i", options(nostack)) };
}
