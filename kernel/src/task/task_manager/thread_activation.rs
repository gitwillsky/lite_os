/// best-effort TID stores 尚未完成时 scheduler state 对 activation 的语义投影。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PreActivationState {
    /// 尚未被 job-control 消费的原始 New。
    New,
    /// group stop 消费了 New，但 SIGCONT 仍必须恢复为 New 而非提前 Ready。
    StoppedNew,
    /// 已存在正常 scheduler membership；activation 不得重复发布。
    Activated,
}

/// 最终 activation 对 scheduler state 的唯一 mutation。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ActivationTransition {
    None,
    StopNew,
    ReadyNew,
    ResumeStoppedNew,
}

/// ProcessGraph consequence 与 scheduler mutation 的组合决策。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ActivationDecision {
    pub(super) inherit_group_exit: bool,
    pub(super) transition: ActivationTransition,
}

/// @description 从同一次 ProcessGraph/scheduler 快照选择新 Thread activation consequence。
/// @param state 当前 scheduler state 对 pre-activation protocol 的投影。
/// @param group_exiting Thread group 是否已提交唯一退出状态。
/// @param job_control_running Process 是否处于正常 job-control 运行态。
/// @return group-exit 对所有 state 都发布终止 consequence；只有 New/StoppedNew 可被最终激活。
pub(super) const fn new_thread_activation(
    state: PreActivationState,
    group_exiting: bool,
    job_control_running: bool,
) -> ActivationDecision {
    let transition = match (state, group_exiting, job_control_running) {
        (PreActivationState::New, true, _) | (PreActivationState::New, false, true) => {
            ActivationTransition::ReadyNew
        }
        (PreActivationState::StoppedNew, true, _)
        | (PreActivationState::StoppedNew, false, true) => ActivationTransition::ResumeStoppedNew,
        (PreActivationState::New, false, false) => ActivationTransition::StopNew,
        (PreActivationState::StoppedNew | PreActivationState::Activated, false, false)
        | (PreActivationState::Activated, _, _) => ActivationTransition::None,
    };
    ActivationDecision {
        inherit_group_exit: group_exiting,
        transition,
    }
}
