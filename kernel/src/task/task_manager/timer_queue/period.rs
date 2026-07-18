/// @description 从既有 expiration 按固定 interval 跳过全部 missed periods。
/// @return 下一次 deadline；interval 为零/算术耗尽时 disarm，并返回至少一次 elapsed。
pub(super) fn next_period(expiration: u64, interval_ns: u64, now_ns: u64) -> (Option<u64>, u64) {
    let Some(elapsed_periods) = now_ns.saturating_sub(expiration).checked_div(interval_ns) else {
        return (None, 1);
    };
    let elapsed = elapsed_periods.saturating_add(1);
    (
        expiration.checked_add(elapsed.saturating_mul(interval_ns)),
        elapsed,
    )
}
