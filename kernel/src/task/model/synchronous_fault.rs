/// 强制同步 fault 投递前需要原子发布的 disposition 与 signal mask 变化。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct SynchronousFaultPolicy {
    /// blocked 或 ignored fault 必须恢复默认 disposition，避免返回同一 faulting PC 无限 trap。
    pub(super) reset_to_default: bool,
    /// 当前 Thread 必须解除 fault signal 屏蔽，确保默认动作或 caught handler 可立即执行。
    pub(super) signal_mask: u64,
}

/// 计算 Linux `force_sig_info_to_task(HANDLER_CURRENT)` 的同步 fault 规范化策略。
///
/// @param signal `1..=64` 的 Linux signal number。
/// @param handler 当前 disposition；0 为默认，1 为忽略，其余为 caught handler。
/// @param signal_mask 当前 Thread blocked mask。
/// @return ignored 或 blocked 时恢复默认 disposition，并始终解除当前 signal 屏蔽。
pub(super) fn force_synchronous_fault(
    signal: usize,
    handler: usize,
    signal_mask: u64,
) -> SynchronousFaultPolicy {
    assert!((1..=64).contains(&signal), "invalid synchronous signal");
    let bit = 1u64 << (signal - 1);
    SynchronousFaultPolicy {
        reset_to_default: handler == 1 || signal_mask & bit != 0,
        signal_mask: signal_mask & !bit,
    }
}

/// 合并 standard signal coalescing 中不可丢失的 forced consequence。
///
/// @param existing 当前首个可见 siginfo 携带的 forced 标记。
/// @param incoming 同号后续 generation 的 forced 标记。
/// @return 无返回值；除 forced 标记外的首个 siginfo 由 caller 原样保留。
pub(super) fn merge_forced(existing: &mut bool, incoming: bool) {
    *existing |= incoming;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caught_unblocked_fault_preserves_handler_delivery() {
        let policy = force_synchronous_fault(4, 0x1234, 0);
        assert_eq!(
            policy,
            SynchronousFaultPolicy {
                reset_to_default: false,
                signal_mask: 0,
            }
        );
    }

    #[test]
    fn blocked_or_ignored_fault_forces_unblocked_default_action() {
        let signal_bit = 1u64 << (4 - 1);
        for (handler, mask) in [(0x1234, signal_bit), (1, 0), (1, signal_bit)] {
            let policy = force_synchronous_fault(4, handler, mask);
            assert!(policy.reset_to_default);
            assert_eq!(policy.signal_mask & signal_bit, 0);
        }
    }

    #[test]
    fn default_fault_remains_an_unblocked_default_action() {
        let policy = force_synchronous_fault(4, 0, 0);
        assert!(!policy.reset_to_default);
        assert_eq!(policy.signal_mask, 0);
    }

    #[test]
    fn later_forced_generation_preserves_first_visible_siginfo() {
        let mut first = (-6i32, 42i32, false);
        let incoming = (1i32, 0i32, true);
        merge_forced(&mut first.2, incoming.2);
        assert_eq!((first.0, first.1), (-6, 42));
        assert!(first.2);
    }
}
