use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// @description kernel 可索引的 hart 容量上限，不表示 DTB 实际启用核数。
pub const MAX_SUPPORTED_HARTS: usize = 8;

static ONLINE_HARTS: AtomicUsize = AtomicUsize::new(0);
static POSSIBLE_HARTS: AtomicUsize = AtomicUsize::new(0);
static BOOT_HART: AtomicUsize = AtomicUsize::new(usize::MAX);
static TOPOLOGY_READY: AtomicBool = AtomicBool::new(false);

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
/// @return 已存在于 DTB possible mask 的 hart ID。
/// @errors `tp` 越界或不在 DTB mask 中表示入口或 trap 上下文已被破坏，将触发内核 panic。
#[inline(always)]
pub fn hart_id() -> usize {
    let hart = raw_hart_id();
    assert!(
        is_possible_hart(hart),
        "hart invariant violated: tp={} not in possible mask {:#x}",
        hart,
        possible_hart_mask()
    );
    hart
}

/// @description 发布 DTB 解析出的 hart 拓扑。
///
/// @param board_info kernel DTB 解析结果。
/// @param boot_hart 首个进入 kernel 的 hart ID。
/// @return 无返回值；DTB 拓扑非法时触发 fail-stop。
pub(crate) fn init_topology(board_info: &crate::arch::dtb::BoardInfo, boot_hart: usize) {
    assert!(board_info.smp != 0, "DTB contains no enabled hart");
    assert!(
        board_info.invalid_hart_id.is_none(),
        "DTB hart ID {} exceeds kernel capacity {}",
        board_info.invalid_hart_id.unwrap_or(usize::MAX),
        MAX_SUPPORTED_HARTS
    );
    assert_eq!(
        board_info.hart_mask.count_ones() as usize,
        board_info.smp,
        "DTB CPU count and unique hart mask disagree"
    );
    assert!(
        board_info.hart_mask & (1usize << boot_hart) != 0,
        "boot hart {} is absent from DTB hart mask {:#x}",
        boot_hart,
        board_info.hart_mask
    );
    BOOT_HART.store(boot_hart, Ordering::Relaxed);
    // Release 发布 possible mask；缺失时其他 hart 可能在拓扑未完整发布时通过 hart_id() 校验。
    POSSIBLE_HARTS.store(board_info.hart_mask, Ordering::Release);
    // Release 与 topology_ready/possible_hart_mask 的 Acquire 配对，作为 DTB 拓扑可用标志。
    TOPOLOGY_READY.store(true, Ordering::Release);
}

/// @description 判断 DTB hart 拓扑是否已经发布。
///
/// @return `true` 表示 possible mask 和 boot hart 已可被消费。
pub(crate) fn topology_ready() -> bool {
    TOPOLOGY_READY.load(Ordering::Acquire)
}

/// @description 获取 DTB 描述的 possible hart mask。
///
/// @return bit N 表示 hart N 存在于 DTB CPU 节点。
/// @errors 拓扑未发布时触发 fail-stop，防止 early code 误用运行期 API。
pub(crate) fn possible_hart_mask() -> usize {
    assert!(topology_ready(), "hart topology used before DTB init");
    POSSIBLE_HARTS.load(Ordering::Acquire)
}

/// @description 判断 hart ID 是否存在于 DTB possible mask。
///
/// @param hart 待检查 hart ID。
/// @return 存在且小于 kernel 容量上限时返回 `true`。
pub(crate) fn is_possible_hart(hart: usize) -> bool {
    hart < MAX_SUPPORTED_HARTS && (possible_hart_mask() & (1usize << hart)) != 0
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
