//! 生产 Thread restart 记录；捕获 handler 与 ignored/stop 的决定不复制到 fixture。
#![allow(dead_code)]
#[path = "../../../kernel/src/task/model/syscall_restart.rs"]
mod syscall_restart;
use syscall_restart::{
    SyscallRestart, SyscallRestartBlock, SyscallRestartPolicy, SyscallRestartState,
};

fn record(policy: SyscallRestartPolicy) -> SyscallRestart {
    SyscallRestart {
        syscall_id: 188,
        args: [19, 0x2000, 7, 0, 0, 0],
        syscall_pc: 0x1002,
        policy,
    }
}
#[test]
fn no_handler_preserves_original_arguments_for_both_restart_classes() {
    for policy in [
        SyscallRestartPolicy::WithRestartHandler,
        SyscallRestartPolicy::WithoutHandler,
    ] {
        let restarted = record(policy).resolve(None).unwrap();
        assert_eq!(restarted.syscall_pc, 0x1002);
        assert_eq!(restarted.syscall_id, 188);
        assert_eq!(restarted.args, [19, 0x2000, 7, 0, 0, 0]);
    }
}
#[test]
fn caught_handler_keeps_message_wait_interrupted_even_with_sa_restart() {
    for flags in [0, 0x1000_0000, 0x1800_0000] {
        assert!(
            record(SyscallRestartPolicy::WithoutHandler)
                .resolve(Some(flags))
                .is_none()
        );
    }
    assert!(
        record(SyscallRestartPolicy::WithRestartHandler)
            .resolve(Some(0))
            .is_none()
    );
    assert!(
        record(SyscallRestartPolicy::WithRestartHandler)
            .resolve(Some(0x1000_0000))
            .is_some()
    );
}

#[test]
fn timed_block_replays_native_restart_syscall_and_consumes_the_frozen_input_once() {
    let mut state = SyscallRestartState::default();
    let block = SyscallRestartBlock::Futex {
        address: 0x2000,
        expected: 9,
        private: true,
        deadline: 123_000_000,
        bitset: 0x40,
    };
    state.set_block(block);
    state.arm(record(SyscallRestartPolicy::RestartBlock));
    let instruction = state.resolve(None).unwrap();
    assert_eq!(instruction.syscall_id, syscall_abi::SYSCALL_RESTART_SYSCALL);
    assert_eq!(instruction.syscall_pc, 0x1002);
    assert!(state.resolve(None).is_none());
    assert_eq!(state.take_block(), Some(block));
    assert_eq!(state.take_block(), None);
}

#[test]
fn restart_block_never_replays_for_caught_handler_and_sigreturn_retires_it() {
    for flags in [0, 0x1000_0000] {
        let mut state = SyscallRestartState::default();
        state.set_block(SyscallRestartBlock::Sleep {
            deadline: 789,
            remaining: 0x3000,
        });
        state.arm(record(SyscallRestartPolicy::RestartBlock));
        assert!(state.resolve(Some(flags)).is_none());
        // 原生 sigreturn 即使有 SA_RESTART 也清理实际 restart function。
        state.clear();
        assert_eq!(state.take_block(), None);
    }
}

#[test]
fn ignored_or_stop_signal_keeps_sleep_deadline_and_remaining_output_identity() {
    let mut state = SyscallRestartState::default();
    let block = SyscallRestartBlock::Sleep {
        deadline: 456,
        remaining: 0x3000,
    };
    state.set_block(block);
    state.arm(record(SyscallRestartPolicy::RestartBlock));
    assert_eq!(state.resolve(None).unwrap().syscall_id, 128);
    assert_eq!(state.take_block(), Some(block));
    state.set_block(block);
    state.clear();
    assert!(
        state.take_block().is_none(),
        "exec cannot retain an old image's operation"
    );
}

#[test]
fn stopping_does_not_resolve_before_a_handler_pending_at_continue() {
    let mut state = SyscallRestartState::default();
    state.set_block(SyscallRestartBlock::Futex {
        address: 0x3000,
        expected: 1,
        private: false,
        deadline: 100,
        bitset: u32::MAX,
    });
    state.arm(record(SyscallRestartPolicy::RestartBlock));
    // Stop/continue 都不调用 resolve；实际下一个捕获 handler 仍能抑制 block restart。
    assert!(state.resolve(Some(0x1000_0000)).is_none());
    state.clear();
    assert!(state.take_block().is_none());
}

#[test]
fn trace_stop_rewinds_once_and_keeps_handler_decision_until_continue() {
    use syscall_restart::RestartResolution;
    let mut state = SyscallRestartState::default();
    state.arm(record(SyscallRestartPolicy::WithRestartHandler));
    let shown = state.prepare_trace().unwrap();
    assert_eq!(
        shown.syscall_pc,
        record(SyscallRestartPolicy::WithRestartHandler).syscall_pc
    );
    assert!(
        state.prepare_trace().is_none(),
        "a second stop must preserve tracer's register changes"
    );
    match state.resolve_signal(Some(0)) {
        Some(RestartResolution::Traced {
            instruction,
            replay_number,
        }) => {
            assert_eq!(instruction, shown.syscall_pc);
            assert_eq!(
                replay_number, None,
                "a handler without SA_RESTART restores EINTR"
            );
        }
        _ => panic!("rewound context must not become a full register replay"),
    }
    assert!(state.resolve_signal(None).is_none());
}

#[test]
fn trace_restart_block_selects_native_restart_without_overwriting_debugger_arguments() {
    use syscall_restart::RestartResolution;
    let mut state = SyscallRestartState::default();
    state.set_block(SyscallRestartBlock::Sleep {
        deadline: 42,
        remaining: 0,
    });
    state.arm(record(SyscallRestartPolicy::RestartBlock));
    assert!(state.prepare_trace().is_some());
    assert!(matches!(
        state.resolve_signal(None),
        Some(RestartResolution::Traced {
            replay_number: Some(128),
            ..
        })
    ));
    state.clear();
    assert!(state.prepare_trace().is_none());
    assert!(state.take_block().is_none());
}
