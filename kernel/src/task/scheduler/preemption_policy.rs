/// @description 判定本地 CFS Ready root 是否应抢占 current。
/// @param current_vruntime 当前 Running entity 的 vruntime；idle 时为 None。
/// @param ready_vruntime 清除 stale generation 后的 Ready heap 最小 vruntime。
/// @return Ready root 严格早于 current 时返回 true；缺少任一 owner 或相等时返回 false。
#[inline(always)]
pub(crate) fn local_ready_preempts(
    current_vruntime: Option<u64>,
    ready_vruntime: Option<u64>,
) -> bool {
    current_vruntime
        .zip(ready_vruntime)
        .is_some_and(|(current, ready)| ready < current)
}
