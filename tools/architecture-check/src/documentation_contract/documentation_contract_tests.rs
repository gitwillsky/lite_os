use std::collections::BTreeMap;

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

    let surface = interface::production_surface(&[source]);
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
