/// @description 沿固定相位计算严格晚于当前时刻的下一次 timer deadline。
///
/// @param previous 上一次 deadline；零表示尚未 arm。
/// @param now 当前 monotonic counter。
/// @param interval 非零 tick 间隔。
/// @return 保持原相位并跳过已错过周期后的下一 deadline。
/// @errors interval 为零或 counter 可表达范围耗尽时返回 `None`。
pub(crate) fn next(previous: u64, now: u64, interval: u64) -> Option<u64> {
    if interval == 0 {
        return None;
    }
    match previous {
        0 => now.checked_add(interval),
        deadline if deadline > now => Some(deadline),
        deadline => (now - deadline)
            .checked_div(interval)
            .and_then(|elapsed| elapsed.checked_add(1))
            .and_then(|periods| periods.checked_mul(interval))
            .and_then(|advance| deadline.checked_add(advance)),
    }
}
