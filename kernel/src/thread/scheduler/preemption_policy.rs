/// 判定新发表且 generation 有效的 CFS Ready entity 是否应抢占 current。
///
/// # Parameters
///
/// - `current_vruntime`: 当前 Running entity 的 vruntime；idle 时为 None。
/// - `ready_vruntime`: 本次真实唤醒的 entity（或新接管的有效批次中最早者）的 vruntime。
///
/// # Returns
///
/// 新的 Ready entity 严格早于 current 时返回 true；缺少任一 owner 或相等时返回 false。
#[inline(always)]
pub(crate) fn local_ready_preempts(
    current_vruntime: Option<u64>,
    ready_vruntime: Option<u64>,
) -> bool {
    current_vruntime
        .zip(ready_vruntime)
        .is_some_and(|(current, ready)| ready < current)
}
