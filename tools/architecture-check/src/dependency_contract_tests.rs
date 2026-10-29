use super::*;

fn violations(text: &str) -> Vec<String> {
    let source = SourceFile {
        relative: "kernel/src/devices/model/probe.rs".to_owned(),
        owner: "device".to_owned(),
        lines: text.lines().map(str::to_owned).collect(),
        syntax: syn::parse_file(text).unwrap(),
        text: text.to_owned(),
        binary_crate: true,
    };
    let mut errors = Vec::new();
    check(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."),
        &[source],
        &mut errors,
    );
    errors
}

#[test]
fn macro_paths_cannot_bypass_dependency_or_concrete_facade_rules() {
    for text in [
        "fn probe() { crate::execution::task::current_task(); }",
        "macro_rules! probe { () => { crate::execution::task::current_task() }; }",
        "macro_rules! probe { () => { $crate::execution::task::current_task() }; }",
        "macro_rules! probe { () => { use crate::{execution::task as t}; t::current_task() }; }",
        "macro_rules! probe { () => { use crate::{execution::task::{current_task as t}}; t() }; }",
    ] {
        let errors = violations(text);
        assert!(
            errors
                .iter()
                .any(|error| error.contains("crate::task is absent")),
            "{text}: {errors:?}"
        );
    }
    let errors =
        violations("macro_rules! probe { () => { crate::platform::qemu_virt::probe() }; }");
    assert!(
        errors
            .iter()
            .any(|error| error.contains("concrete backend")),
        "{errors:?}"
    );
}

#[test]
fn driver_grouping_preserves_child_owner_boundaries() {
    for text in [
        "fn probe() { crate::drivers::virtio::probe(); }",
        "macro_rules! probe { () => { crate::drivers::pci::probe() }; }",
    ] {
        assert!(
            violations(text)
                .iter()
                .any(|error| error.contains("is absent")),
            "{text}"
        );
    }
    assert!(
        violations("use crate::drivers as implementations;")
            .iter()
            .any(|error| error.contains("not a grouping"))
    );
}

#[test]
fn old_root_paths_cannot_hide_conditional_dependencies() {
    for text in [
        "#[cfg(target_arch = \"other\")] fn probe() { crate::task::current_task(); }",
        "macro_rules! probe { () => { $crate::audio::probe() }; }",
    ] {
        assert!(
            violations(text)
                .iter()
                .any(|error| error.contains("obsolete root domain")),
            "{text}"
        );
    }
}

#[test]
fn macro_and_grouped_imports_cannot_alias_the_crate_root() {
    for text in [
        "use crate as root;",
        "use crate::{self as root};",
        "use crate::{*};",
        "macro_rules! probe { () => { use crate as root; }; }",
        "macro_rules! probe { () => { use crate::*; }; }",
        "macro_rules! probe { () => { use crate::{self as root}; }; }",
    ] {
        let errors = violations(text);
        assert!(
            errors
                .iter()
                .any(|error| error.contains("aliasing or glob-importing")),
            "{text}: {errors:?}"
        );
    }
}

#[test]
fn literals_comments_and_allowed_macro_paths_do_not_add_dependencies() {
    let errors = violations(
        r#"
        // crate::execution::task::current_task()
        macro_rules! probe { () => { println!("crate::execution::task::current_task()"); crate::arch::probe() }; }
    "#,
    );
    assert!(errors.is_empty(), "{errors:?}");
}

fn graph(edges: &[(&str, &[&str])]) -> BTreeMap<String, BTreeSet<String>> {
    edges
        .iter()
        .map(|(owner, dependencies)| {
            (
                (*owner).to_owned(),
                dependencies
                    .iter()
                    .map(|dependency| (*dependency).to_owned())
                    .collect(),
            )
        })
        .collect()
}

#[test]
fn permitted_edges_must_not_form_a_cycle() {
    let graph = graph(&[
        ("memory", &["random"]),
        ("random", &["drivers"]),
        ("drivers", &["memory"]),
    ]);
    assert_eq!(
        dependency_cycle(&graph),
        Some(vec![
            "drivers".into(),
            "memory".into(),
            "random".into(),
            "drivers".into()
        ])
    );
}

#[test]
fn diamond_dependencies_are_valid_but_self_dependencies_are_not() {
    assert_eq!(
        dependency_cycle(&graph(&[
            ("task", &["memory", "random"]),
            ("random", &["drivers"]),
            ("drivers", &["memory"]),
            ("memory", &[]),
        ])),
        None
    );
    assert_eq!(
        dependency_cycle(&graph(&[("memory", &["memory"])])),
        Some(vec!["memory".into(), "memory".into()])
    );
}
