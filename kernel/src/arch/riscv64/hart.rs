use core::sync::atomic::{AtomicUsize, Ordering};

/// @description kernel 可索引的 hart 容量上限，不表示 DTB 实际启用核数。
pub const MAX_SUPPORTED_HARTS: usize = 8;

static ONLINE_HARTS: AtomicUsize = AtomicUsize::new(0);

/// @description 读取未经验证的当前 hart ID，仅供入口检查和 panic 诊断使用。
///
/// @return `tp` 中由内核入口安装的原始 hart ID。
#[inline(always)]
pub(crate) fn raw_hart_id() -> usize {
    let value: usize;
    unsafe {
        core::arch::asm!("mv {}, tp", out(reg) value, options(nomem, nostack));
    }
    value
}

/// @description 获取已经过内核入口验证的当前 hart ID。
///
/// @return 小于 [`MAX_SUPPORTED_HARTS`] 的 hart ID。
/// @errors `tp` 越界表示入口或 trap 上下文已被破坏，将触发内核 panic；不得映射到 CPU0。
#[inline(always)]
pub fn hart_id() -> usize {
    let hart = raw_hart_id();
    assert!(
        hart < MAX_SUPPORTED_HARTS,
        "hart invariant violated: tp={} >= MAX_SUPPORTED_HARTS={}",
        hart,
        MAX_SUPPORTED_HARTS
    );
    hart
}

/// @description 发布当前 hart 已完成页表、timer 和中断初始化。
///
/// @return 无返回值。
pub(crate) fn mark_online() {
    let hart = hart_id();
    // Release 发布本 hart 在此之前完成的 CSR 和 per-hart 状态写入；缺失时，
    // RFENCE 发送方可能把请求发给尚不能接收 supervisor software interrupt 的 hart。
    ONLINE_HARTS.fetch_or(1usize << hart, Ordering::Release);
}

/// @description 获取已完成 S-mode 初始化的 hart 集合。
///
/// @return bit N 表示 hart N 已可接收 IPI/RFENCE 后续工作。
pub(crate) fn online_hart_mask() -> usize {
    // Acquire 与 mark_online 的 Release 配对，消费每个目标 hart 的初始化写入。
    ONLINE_HARTS.load(Ordering::Acquire)
}
