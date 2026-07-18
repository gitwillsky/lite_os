use std::fs;

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

fn fallible_tree_source() -> SourceFile {
    source(
        "kernel/src/fallible_tree.rs",
        r#"
            fn try_reserve_node() { Box::<Node>::try_new_uninit(); }
            fn try_prepare_vacant() { try_reserve_node(); }
            fn try_prepare() { try_reserve_node(); }
            fn try_insert() { try_prepare(); }
        "#,
    )
}

#[test]
fn check_owns_registry_and_allocation_graph_validation() {
    let root = std::env::temp_dir().join(format!(
        "lite_os_fallible_contract_test_{}",
        std::process::id()
    ));
    fs::create_dir_all(root.join("docs")).expect("test docs directory must be created");
    fs::write(
        root.join("docs/architecture-contract.md"),
        r#"
### Persistent FallibleMap registry

| Location | Type |
| --- | --- |
| `kernel/src/owner.rs :: Owner.entries` | `crate :: fallible_tree :: FallibleMap < u64 , u64 >` |
"#,
    )
    .expect("test registry must be written");
    let owner = source(
        "kernel/src/owner.rs",
        "struct Owner { entries: crate::fallible_tree::FallibleMap<u64, u64> }",
    );

    let mut errors = Vec::new();
    check(&root, &[owner, fallible_tree_source()], &mut errors);
    assert!(errors.is_empty(), "{errors:#?}");

    let alias = source(
        "kernel/src/alias.rs",
        "type Hidden = crate::fallible_tree::FallibleMap<u64, u64>;",
    );
    check(&root, &[alias, fallible_tree_source()], &mut errors);
    assert!(
        errors
            .iter()
            .any(|error| error.contains("aliases bypass the exact owner registry")),
        "{errors:#?}"
    );

    fs::remove_dir_all(&root).expect("test directory must be removed");
}
