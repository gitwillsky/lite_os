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
    check_ready_projection_consumers(&[allowed_ready_consumers()], &mut errors);
    assert!(errors.is_empty(), "{errors:#?}");

    let reviewed_import = source(
        PROCESSOR_PATH,
        "use ready_membership::{commit_ready_retirement, commit_ready_transition};",
    );
    check_ready_projection_consumers(&[allowed_ready_consumers(), reviewed_import], &mut errors);
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
    check_ready_projection_consumers(&[allowed_ready_consumers(), forbidden], &mut errors);
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
    check_ready_projection_consumers(&[allowed_ready_consumers(), impostors], &mut errors);
    assert_eq!(errors.len(), 6, "{errors:#?}");

    errors.clear();
    check_ready_projection_consumers(
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
    check_ready_transition_shapes(&[allowed], &mut errors);
    assert!(errors.is_empty(), "{errors:#?}");

    check_ready_projection_consumers(
        &[allowed_ready_consumers(), bypass_ready_consumers()],
        &mut errors,
    );
    assert_eq!(errors.len(), 1, "{errors:#?}");

    errors.clear();
    check_ready_transition_shapes(&[bypass_ready_consumers()], &mut errors);
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
    check_ready_projection_consumers(&[deferred], &mut errors);
    assert_eq!(errors.len(), 1, "{errors:#?}");
}

#[test]
fn documentation_links_are_extracted_and_normalized_lexically() {
    assert_eq!(
        markdown_targets("[local](../README.md#entry) [web](https://example.com)"),
        ["../README.md#entry", "https://example.com"]
    );
    assert_eq!(
        normalize_local_link("docs/architecture/domain.md", "../README.md#entry"),
        Ok(Some("docs/README.md".to_owned()))
    );
    assert_eq!(
        normalize_local_link("docs/README.md", "https://example.com"),
        Ok(None)
    );
    assert!(normalize_local_link("README.md", "../outside.md").is_err());
}

#[test]
fn documentation_size_fence_rejects_oversized_manual_lines() {
    let documents = BTreeMap::from([(
        "docs/architecture/domain.md".to_owned(),
        "x".repeat(DOCUMENT_MAX_LINE_BYTES + 1),
    )]);
    let mut errors = Vec::new();
    check_document_sizes(&documents, &mut errors);
    assert_eq!(errors.len(), 1, "{errors:#?}");
}

#[test]
fn test_only_scoped_methods_do_not_expand_the_production_interface() {
    let source = source(
        "kernel/src/domain.rs",
        r#"
            struct Owner;
            impl Owner {
                pub(crate) fn production(&self) {}
            }
            #[cfg(test)]
            impl Owner {
                pub(crate) fn probe(&self) {}
            }
        "#,
    );

    let surface = interface_surface(&[source]);
    assert!(
        surface.iter().any(|entry| entry.contains("production")),
        "{surface:#?}"
    );
    assert!(
        surface.iter().all(|entry| !entry.contains("probe")),
        "{surface:#?}"
    );
}

#[test]
fn documentation_ownership_rejects_retired_terms_and_test_prohibitions() {
    let documents = BTreeMap::from([
        (
            "docs/syscall-support.md".to_owned(),
            "1 个 Linux/riscv64 syscall".to_owned(),
        ),
        (
            "docs/architecture.md".to_owned(),
            "HartTopology；禁止测试".to_owned(),
        ),
    ]);
    let mut errors = Vec::new();
    check_document_ownership(&documents, 1, &mut errors);
    assert_eq!(errors.len(), 3, "{errors:#?}");
}

#[test]
fn every_syscall_requires_exactly_one_domain_row() {
    let paths = [
        "docs/syscall-support/filesystem-io.md",
        "docs/syscall-support/ipc.md",
        "docs/syscall-support/memory.md",
        "docs/syscall-support/process-identity.md",
        "docs/syscall-support/signal-time.md",
        "docs/syscall-support/socket.md",
        "docs/syscall-support/synchronization-scheduling.md",
        "docs/syscall-support/system.md",
    ];
    let mut documents = paths
        .into_iter()
        .map(|path| (path.to_owned(), String::new()))
        .collect::<BTreeMap<_, _>>();
    let tick = char::from(96);
    documents.insert(
        "docs/syscall-support/system.md".to_owned(),
        format!("| 1 | {tick}one{tick} | Complete | scope |"),
    );
    let expected = BTreeMap::from([("one".to_owned(), 1)]);
    let mut errors = Vec::new();
    check_syscall_documentation(&documents, &expected, &mut errors);
    assert!(errors.is_empty(), "{errors:#?}");

    documents.insert(
        "docs/syscall-support/ipc.md".to_owned(),
        format!("| 1 | {tick}one{tick} | Complete | duplicate |"),
    );
    check_syscall_documentation(&documents, &expected, &mut errors);
    assert_eq!(errors.len(), 1, "{errors:#?}");
}
