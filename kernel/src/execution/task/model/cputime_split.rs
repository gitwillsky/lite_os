//! 把精确总 CPU runtime 按 tick 采样比例拆成 user/system（Linux `cputime_adjust`）。

/// 按 tick 比例估计 system 时间。
///
/// # Parameters
///
/// - `runtime_us`: 精确总 CPU runtime。
/// - `user_ticks` / `system_ticks`: 被打断的特权级为用户态 / 内核态的 timer tick 数。
///
/// # Returns
///
/// `[0, runtime_us]` 内的 system 微秒；没有任何 tick 样本时为 0（全部算 user）。
pub(crate) fn estimate_system_us(runtime_us: u64, user_ticks: u64, system_ticks: u64) -> u64 {
    let ticks = u128::from(user_ticks) + u128::from(system_ticks);
    if ticks == 0 {
        return 0;
    }
    (u128::from(runtime_us) * u128::from(system_ticks) / ticks) as u64
}

/// 钳制估计值，使反复读取得到的 system 时间不回退、也不超过总 runtime。
///
/// 比例按样本更新，user 增长更快时按比例得到的 system 可能小于上次报告值；对用户而言时间不能倒退。
///
/// # Parameters
///
/// - `estimate_us`: [`estimate_system_us`] 的结果。
/// - `previously_reported_us`: 此前报告过的最大 system 时间。
/// - `runtime_us`: 精确总 CPU runtime。
pub(crate) fn clamp_system_us(
    estimate_us: u64,
    previously_reported_us: u64,
    runtime_us: u64,
) -> u64 {
    estimate_us.max(previously_reported_us).min(runtime_us)
}
