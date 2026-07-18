use super::*;

fn source(relative: &str, text: &str) -> SourceFile {
    SourceFile {
        relative: relative.to_owned(),
        owner: String::new(),
        text: text.to_owned(),
        lines: text.lines().map(str::to_owned).collect(),
        syntax: syn::parse_file(text).expect("test Rust source must parse"),
        binary_crate: true,
    }
}

fn allowed_ready_consumers() -> SourceFile {
    source(
        READY_MEMBERSHIP_PATH,
        r#"
            fn commit_ready_transition(transition: Token) {
                let (previous, target, generation) =
                    transition.consume_ready_projection_parts();
            }
            fn commit_ready_retirement(retirement: Token) {
                retire(retirement.consume_ready_projection_cpu());
            }
        "#,
    )
}

fn bypass_ready_consumers() -> SourceFile {
    source(
        "kernel/src/task/task_manager.rs",
        r#"
            use SchedulingState::transition_to_ready as alias;
            use core::mem::forget as commit_ready_transition;
            use ready_membership::*;
            fn bypass(mut scheduling: SchedulingState) {
                core::mem::forget(scheduling.transition_to_ready(0));
                let token = ManuallyDrop::new(scheduling.transition_ready_to_running(0, 1));
                commit_ready_transition(scheduling.transition_to_ready(0));
                commit_ready_retirement(token);
                passthrough!(transition_ready_to_stopped);
            }
        "#,
    )
}

#[test]
fn ready_projection_tokens_have_only_the_two_commit_callers() {
    let mut errors = Vec::new();
    check_projection_consumers(&[allowed_ready_consumers()], &mut errors);
    assert!(errors.is_empty(), "{errors:#?}");

    let reviewed_import = source(
        PROCESSOR_PATH,
        "use ready_membership::{commit_ready_retirement, commit_ready_transition};",
    );
    check_projection_consumers(&[allowed_ready_consumers(), reviewed_import], &mut errors);
    assert!(errors.is_empty(), "{errors:#?}");

    let forbidden = source(
        "kernel/src/task/task_manager.rs",
        r#"
            use model::ReadyTransition::consume_ready_projection_parts as bypass;
            use core::mem::forget as commit_ready_transition;
            use core::mem as ready_membership;
            use ready_membership::forget as commit_ready_retirement;
            fn skip_projection(token: Token) {
                token.consume_ready_projection_cpu();
                ReadyTransition::consume_ready_projection_parts(token);
                passthrough!(consume_ready_projection_cpu);
            }
        "#,
    );
    check_projection_consumers(&[allowed_ready_consumers(), forbidden], &mut errors);
    assert_eq!(errors.len(), 6, "{errors:#?}");
}

#[test]
fn ready_projection_fence_rejects_nested_impl_and_duplicate_committers() {
    let impostors = source(
        READY_MEMBERSHIP_PATH,
        r#"
            fn outer() {
                fn commit_ready_transition(token: Token) {
                    token.consume_ready_projection_parts();
                }
            }
            impl Impostor {
                fn commit_ready_retirement(token: Token) {
                    token.consume_ready_projection_cpu();
                }
            }
            mod nested {
                fn commit_ready_transition(token: Token) {
                    token.consume_ready_projection_parts();
                }
            }
        "#,
    );
    let mut errors = Vec::new();
    check_projection_consumers(&[allowed_ready_consumers(), impostors], &mut errors);
    assert_eq!(errors.len(), 6, "{errors:#?}");

    errors.clear();
    check_projection_consumers(
        &[allowed_ready_consumers(), allowed_ready_consumers()],
        &mut errors,
    );
    assert_eq!(errors.len(), 4, "{errors:#?}");
}

#[test]
fn ready_transitions_must_be_direct_commit_arguments() {
    let allowed = source(
        "kernel/src/task/processor/placement.rs",
        r#"
            use super::*;
            fn publish(mut scheduling: SchedulingState) {
                commit_ready_transition(scheduling.transition_to_ready(0));
                commit_ready_retirement(scheduling.transition_ready_to_running(0, 1));
                commit_ready_retirement(scheduling.transition_ready_to_stopped());
            }
        "#,
    );
    let mut errors = Vec::new();
    check_transition_shapes(&[allowed], &mut errors);
    assert!(errors.is_empty(), "{errors:#?}");

    check_projection_consumers(
        &[allowed_ready_consumers(), bypass_ready_consumers()],
        &mut errors,
    );
    assert_eq!(errors.len(), 1, "{errors:#?}");

    errors.clear();
    check_transition_shapes(&[bypass_ready_consumers()], &mut errors);
    assert_eq!(errors.len(), 6, "{errors:#?}");
}

#[test]
fn ready_projection_consumer_must_execute_eagerly() {
    let deferred = source(
        READY_MEMBERSHIP_PATH,
        r#"
            fn commit_ready_transition(transition: Token) {
                core::mem::forget(|| transition.consume_ready_projection_parts());
            }
            fn commit_ready_retirement(retirement: Token) {
                retire(retirement.consume_ready_projection_cpu());
            }
        "#,
    );
    let mut errors = Vec::new();
    check_projection_consumers(&[deferred], &mut errors);
    assert_eq!(errors.len(), 1, "{errors:#?}");
}
