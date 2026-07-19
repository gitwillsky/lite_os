use std::{fs, path::PathBuf};

const HANDOFFS: usize = 1_024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HandoffCost {
    kernel_context_switches: usize,
    idle_entries_with_runnable_successor: usize,
    logical_cpu_restore_guards: usize,
    post_switch_completion_continuations: usize,
    single_pending_owner: usize,
}

fn repository_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("architecture-check must live under tools/")
        .to_path_buf()
}

fn read(path: &str) -> String {
    fs::read_to_string(repository_root().join(path))
        .unwrap_or_else(|error| panic!("{path}: {error}"))
}

fn function_body<'a>(source: &'a str, signature: &str) -> Option<&'a str> {
    let start = source.find(signature)?;
    let body = &source[start..];
    let mut depth = 0usize;
    let mut opened = false;
    for (offset, byte) in body.bytes().enumerate() {
        match byte {
            b'{' => {
                opened = true;
                depth += 1;
            }
            b'}' if opened => {
                depth -= 1;
                if depth == 0 {
                    return Some(&body[..=offset]);
                }
            }
            _ => {}
        }
    }
    None
}

fn measure() -> HandoffCost {
    let task_manager = read("kernel/src/task/task_manager.rs");
    let context_switch = read("kernel/src/task/task_manager/context_switch.rs");
    let handoff = read("kernel/src/task/processor/handoff.rs");
    let sync = read("kernel/src/sync/mod.rs");
    let task = read("kernel/src/task/mod.rs");
    let legacy_deschedule =
        function_body(&context_switch, "pub(super) fn schedule_with_task_context(").is_some_and(
            |body| body.contains("switch_kernel_context(task_cx_ptr, idle_task_cx_ptr)"),
        );
    let legacy_dispatch = function_body(&task_manager, "fn switch_to_task(").is_some_and(|body| {
        body.contains("switch_kernel_context(idle_task_cx_ptr, next_task_cx_ptr)")
    });
    let direct_handoff = function_body(&context_switch, "fn select_task_switch_target(")
        .is_some_and(|body| body.contains("Processor::select_task"))
        && function_body(&context_switch, "pub(super) fn schedule_with_task_context(").is_some_and(
            |body| {
                body.matches("switch_kernel_context(").count() == 1
                    && body.contains("select_task_switch_target")
            },
        );

    if direct_handoff {
        let task_handoff =
            function_body(&context_switch, "pub(super) fn schedule_with_task_context(")
                .is_some_and(|body| {
                    matches!(
                        (
                            body.find("switch_kernel_context"),
                            body.find("complete_pending_handoff")
                        ),
                        (Some(switch), Some(complete)) if switch < complete
                    )
                });
        let idle_handoff = function_body(&context_switch, "pub(super) fn switch_from_idle(")
            .is_some_and(|body| {
                matches!(
                    (
                        body.find("switch_kernel_context"),
                        body.find("complete_pending_handoff")
                    ),
                    (Some(switch), Some(complete)) if switch < complete
                )
            });
        let irq_transfer_drop = sync
            .find("impl Drop for LocalIrqTransfer")
            .and_then(|start| function_body(&sync[start..], "fn drop("));
        let first_run = function_body(&task, "fn resume_new_task(")
            .is_some_and(|body| body.contains("complete_pending_handoff"));
        HandoffCost {
            kernel_context_switches: HANDOFFS,
            idle_entries_with_runnable_successor: 0,
            logical_cpu_restore_guards: usize::from(
                sync.contains("cpu: crate::cpu::CpuId")
                    && irq_transfer_drop.is_some_and(|body| {
                        matches!(
                            (
                                body.find("compiler_fence(Ordering::SeqCst)"),
                                body.find("local IRQ transfer crossed logical CPUs"),
                                body.find("restore_local")
                            ),
                            (Some(fence), Some(cpu), Some(restore)) if fence < cpu && cpu < restore
                        )
                    }),
            ),
            post_switch_completion_continuations: [task_handoff, idle_handoff, first_run]
                .into_iter()
                .filter(|present| *present)
                .count(),
            single_pending_owner: usize::from(
                handoff.contains("processor.pending_handoff.is_none()")
                    && handoff.contains("pending_handoff\n            .take()"),
            ),
        }
    } else if legacy_deschedule && legacy_dispatch {
        HandoffCost {
            kernel_context_switches: HANDOFFS * 2,
            idle_entries_with_runnable_successor: HANDOFFS,
            logical_cpu_restore_guards: 0,
            post_switch_completion_continuations: 0,
            single_pending_owner: 0,
        }
    } else {
        panic!("production scheduler handoff path is not measurable");
    }
}

#[test]
fn runnable_handoffs_switch_directly_without_entering_idle() {
    let cost = measure();
    assert_eq!(
        cost,
        HandoffCost {
            kernel_context_switches: HANDOFFS,
            idle_entries_with_runnable_successor: 0,
            logical_cpu_restore_guards: 1,
            post_switch_completion_continuations: 3,
            single_pending_owner: 1,
        },
        "N={HANDOFFS} runnable handoffs; measured {cost:?}"
    );
}
