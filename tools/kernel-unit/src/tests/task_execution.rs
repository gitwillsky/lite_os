use alloc::vec::Vec;

use crate::{
    clone_errno::{clone_resource_errno, process_clone_memory_errno, thread_clone_memory_errno},
    clone_tid_store::store_clone_tid_values,
    console_batch::{CONSOLE_WAKE_BATCH, ConsoleWakeBatch},
    pty_input_notification::{PtyInputActions, pty_input_actions},
    snapshot_staging::{SnapshotCapacity, snapshot_capacity},
    terminal_input_batch::{
        CHARACTER_WRITE_CHUNK_BYTES, TERMINAL_INPUT_BATCH_BYTES, TerminalInputBatch,
        character_write_chunk, terminal_input_chunk,
    },
    thread_activation::{
        ActivationDecision, ActivationTransition, PreActivationState, new_thread_activation,
    },
    timer_preparation_policy::{
        PosixCreateAction, posix_create_action, posix_deadline_needed, real_replacement_needs,
    },
    wait_publication::{ChildWaitPublication, child_wait_publication},
};

#[test]
fn clone_errno_distinguishes_snapshot_oom_from_resource_exhaustion() {
    assert_eq!(process_clone_memory_errno(true), -crate::errno::ENOMEM);
    assert_eq!(thread_clone_memory_errno(true), -crate::errno::ENOMEM);
    assert_eq!(process_clone_memory_errno(false), -crate::errno::EAGAIN);
    assert_eq!(thread_clone_memory_errno(false), -crate::errno::EINVAL);
    assert_eq!(clone_resource_errno(), -crate::errno::EAGAIN);
}

#[test]
fn terminal_input_budget_has_exact_boundary() {
    assert_eq!(TERMINAL_INPUT_BATCH_BYTES, 256);
    assert_eq!(terminal_input_chunk(0, 128), 128);
    assert_eq!(terminal_input_chunk(255, 128), 1);
    assert_eq!(terminal_input_chunk(256, 128), 0);
    assert_eq!(terminal_input_chunk(usize::MAX, 128), 0);
    assert_eq!(
        TerminalInputBatch {
            signals: 1 << 1,
            backlog: true,
        },
        TerminalInputBatch {
            signals: 2,
            backlog: true,
        }
    );
}

#[test]
fn pty_newline_after_input_budget_requeues_raw_backlog() {
    let newline_offset = TERMINAL_INPUT_BATCH_BYTES;
    let first_batch = terminal_input_chunk(0, newline_offset + 1);
    assert_eq!(first_batch, TERMINAL_INPUT_BATCH_BYTES);
    assert!(newline_offset >= first_batch);
    assert!(pty_input_actions(false, true, 0).notify_slave);
}

#[test]
fn pty_second_chunk_newline_is_cooked_before_write_returns() {
    let newline_offset = TERMINAL_INPUT_BATCH_BYTES;
    let total = newline_offset + 1;
    let mut written = 0;
    let mut chunks = 0;
    let mut cooked_ready = false;
    while written < total {
        let chunk = character_write_chunk(total - written, true);
        // PtyMaster::write 同步 drain 每个已 copy-in chunk；chunk 不超过同一 256-byte budget。
        assert_eq!(terminal_input_chunk(0, chunk), chunk);
        cooked_ready |= (written..written + chunk).contains(&newline_offset);
        written += chunk;
        chunks += 1;
    }
    assert_eq!(chunks, 2);
    assert!(cooked_ready);
    assert_eq!(
        character_write_chunk(CHARACTER_WRITE_CHUNK_BYTES + 1, false),
        CHARACTER_WRITE_CHUNK_BYTES
    );
}

#[test]
fn pty_input_batch_preserves_isig_for_task_routing() {
    let signals = (1 << (2 - 1)) | (1 << (20 - 1));
    assert_eq!(
        pty_input_actions(false, false, signals),
        PtyInputActions {
            notify_slave: false,
            signals,
        }
    );
}

#[test]
fn console_waiter_batch_stops_at_constant_limit() {
    assert_eq!(CONSOLE_WAKE_BATCH, 32);
    let mut batch = ConsoleWakeBatch::new();
    for group in 0..CONSOLE_WAKE_BATCH - 1 {
        assert!(!batch.is_full());
        batch.record(Some(group));
    }
    assert!(!batch.is_full());
    batch.record(None);
    assert!(batch.is_full());
    assert_eq!(batch.selected(), CONSOLE_WAKE_BATCH);
}

#[test]
#[should_panic(expected = "console wake group selected twice")]
fn console_waiter_batch_rejects_duplicate_wake_group() {
    let mut batch = ConsoleWakeBatch::new();
    batch.record(Some(7));
    batch.record(Some(7));
}

#[test]
fn clone_tid_store_fault_is_best_effort_and_does_not_skip_later_store() {
    let mut attempts = Vec::new();
    store_clone_tid_values([Some(0), Some(0x4000)], |address| {
        attempts.push(address);
        if address == 0 { Err(()) } else { Ok(()) }
    });
    assert_eq!(attempts, [0, 0x4000]);
}

#[test]
fn clone_activation_gives_group_exit_precedence_over_job_control() {
    assert_eq!(
        new_thread_activation(PreActivationState::New, true, true),
        ActivationDecision {
            inherit_group_exit: true,
            transition: ActivationTransition::ReadyNew,
        }
    );
    assert_eq!(
        new_thread_activation(PreActivationState::StoppedNew, true, false),
        ActivationDecision {
            inherit_group_exit: true,
            transition: ActivationTransition::ResumeStoppedNew,
        }
    );
    assert_eq!(
        new_thread_activation(PreActivationState::Activated, true, true),
        ActivationDecision {
            inherit_group_exit: true,
            transition: ActivationTransition::None,
        }
    );
}

#[test]
fn clone_stop_continue_before_activate_restores_new_before_ready() {
    assert_eq!(
        new_thread_activation(PreActivationState::StoppedNew, false, false),
        ActivationDecision {
            inherit_group_exit: false,
            transition: ActivationTransition::None,
        }
    );
    // SIGCONT 把 StoppedNew 恢复为 New；最终 activate 才能发布唯一 Ready membership。
    assert_eq!(
        new_thread_activation(PreActivationState::New, false, true),
        ActivationDecision {
            inherit_group_exit: false,
            transition: ActivationTransition::ReadyNew,
        }
    );
}

#[test]
fn child_wait_final_recheck_precedes_staging_oom() {
    assert_eq!(
        child_wait_publication(true, false, false, false),
        ChildWaitPublication::ConsumeEvent
    );
    assert_eq!(
        child_wait_publication(false, false, true, false),
        ChildWaitPublication::Interrupted
    );
    assert_eq!(
        child_wait_publication(false, false, false, false),
        ChildWaitPublication::OutOfMemory
    );
    assert_eq!(
        child_wait_publication(false, false, false, true),
        ChildWaitPublication::Publish
    );
}

#[test]
fn graph_snapshot_retries_when_final_owner_observes_growth() {
    assert_eq!(
        snapshot_capacity(7, 8),
        SnapshotCapacity::Retry { minimum: 8 }
    );
    assert_eq!(snapshot_capacity(8, 8), SnapshotCapacity::Capture);
    assert_eq!(snapshot_capacity(9, 8), SnapshotCapacity::Capture);
}

#[test]
fn armed_timer_reset_reuses_existing_nodes_without_spurious_oom() {
    assert_eq!(
        real_replacement_needs(true, true, true, true),
        crate::timer_preparation_policy::TimerReplacementNeeds {
            record: false,
            deadline: false,
        }
    );
    assert!(!posix_deadline_needed(true, true));
    // deadline 在 staging 窗口到期/被摘除时，final owner 必须要求锁外补 node 后重试。
    assert!(real_replacement_needs(true, false, true, true).deadline);
    assert!(posix_deadline_needed(false, true));
}

#[test]
fn posix_timer_id_collision_retargets_the_prepared_node() {
    assert_eq!(
        posix_create_action(true),
        PosixCreateAction::RetargetPreparedNode
    );
    assert_eq!(posix_create_action(false), PosixCreateAction::Commit);
}
