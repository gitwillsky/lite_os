/// timer replacement 在当前 owner snapshot 下真正缺少的 node storage。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct TimerReplacementNeeds {
    pub(super) record: bool,
    pub(super) deadline: bool,
}

/// POSIX timer ID final recheck 对唯一 prepared record node 的动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PosixCreateAction {
    Commit,
    RetargetPreparedNode,
}

/// @description 决定 candidate ID 冲突时提交或复用同一 prepared node。
pub(super) const fn posix_create_action(id_occupied: bool) -> PosixCreateAction {
    if id_occupied {
        PosixCreateAction::RetargetPreparedNode
    } else {
        PosixCreateAction::Commit
    }
}

/// @description 计算 ITIMER_REAL replacement 真正缺少的 record/deadline node。
/// @param record_exists 当前是否已有 timer record。
/// @param deadline_exists 当前 record 是否已有 active deadline node。
/// @param replacement_record 新 setting 是否需要保留 record。
/// @param replacement_deadline 新 setting 是否 armed。
/// @return 只有无法复用当前 node 时才要求锁外 allocation。
pub(super) const fn real_replacement_needs(
    record_exists: bool,
    deadline_exists: bool,
    replacement_record: bool,
    replacement_deadline: bool,
) -> TimerReplacementNeeds {
    TimerReplacementNeeds {
        record: replacement_record && !record_exists,
        deadline: replacement_deadline && !deadline_exists,
    }
}

/// @description 计算已有 POSIX timer replacement 是否真正缺少 deadline node。
pub(super) const fn posix_deadline_needed(
    deadline_exists: bool,
    replacement_deadline: bool,
) -> bool {
    replacement_deadline && !deadline_exists
}
