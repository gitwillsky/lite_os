/// 判定新发表且 generation 有效的 Ready 排序键是否早于 current。
///
/// # Parameters
///
/// - `current`: 当前 Running 的键；idle 时为 None，生产调用者把预算可用性和类内顺序组合为全序。
/// - `ready`: 本次真实唤醒的 entity（或新接管的有效批次中最早者）的键。
///
/// # Returns
///
/// 新的 Ready entity 严格早于 current 时返回 true；缺少任一 owner 或相等时返回 false。
#[inline(always)]
pub(crate) fn local_ready_preempts<T: Ord>(current: Option<T>, ready: Option<T>) -> bool {
    current
        .zip(ready)
        .is_some_and(|(current, ready)| ready < current)
}
