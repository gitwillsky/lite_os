use std::{fs, path::PathBuf};

use super::{SourceFile, source_size::check};

struct TestRepository {
    root: PathBuf,
}

impl TestRepository {
    fn new() -> Self {
        let base = std::env::temp_dir();
        let root = (0..1_000)
            .map(|suffix| {
                base.join(format!(
                    "liteos-architecture-source-size-{}-{suffix}",
                    std::process::id()
                ))
            })
            .find(|path| fs::create_dir(path).is_ok())
            .expect("source-size test must reserve a temporary repository");
        for relative in [
            "docs",
            "user",
            "tools/kernel-unit/src",
            "tools/scheduler-unit/src",
            "tools/architecture-check/src",
        ] {
            fs::create_dir_all(root.join(relative)).unwrap();
        }
        fs::write(root.join("docs/architecture-contract.md"), "").unwrap();
        Self { root }
    }

    fn write_lines(&self, relative: &str, lines: usize) {
        let path = self.root.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, "x\n".repeat(lines)).unwrap();
    }

    fn review(&self, relative: &str, limit: usize, owner: &str) {
        fs::write(
            self.root.join("docs/architecture-contract.md"),
            format!("| `{relative}` | {limit} | `{owner}` | reason | exit | |\n"),
        )
        .unwrap();
    }
}

impl Drop for TestRepository {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).unwrap();
    }
}

fn production(relative: &str, owner: &str, lines: usize) -> SourceFile {
    SourceFile {
        relative: relative.to_owned(),
        owner: owner.to_owned(),
        text: String::new(),
        lines: vec![String::new(); lines],
        syntax: syn::parse_file("").unwrap(),
        binary_crate: true,
    }
}

#[test]
fn one_entry_enforces_production_unit_and_user_thresholds() {
    let repository = TestRepository::new();
    repository.write_lines("tools/kernel-unit/src/accepted.rs", 600);
    repository.write_lines("tools/kernel-unit/src/review.rs", 601);
    repository.write_lines("tools/kernel-unit/src/review_limit.rs", 1_200);
    repository.write_lines("tools/kernel-unit/src/reject.rs", 1_201);
    repository.write_lines("tools/scheduler-unit/src/reject.rs", 1_201);
    repository.write_lines("tools/architecture-check/src/probe_tests.rs", 1_201);
    repository.write_lines("user/accepted.c", 600);
    repository.write_lines("user/reject.c", 601);
    repository.write_lines("user/quickjs-runtime/vendor/quickjs/quickjs.c", 60_000);
    let sources = [
        production("kernel/src/fs/accepted.rs", "fs", 600),
        production("kernel/src/fs/review.rs", "fs", 601),
        production("kernel/src/fs/review_limit.rs", "fs", 1_200),
        production("kernel/src/fs/reject.rs", "fs", 1_201),
    ];
    let mut errors = Vec::new();
    let mut notices = Vec::new();

    check(&repository.root, &sources, &mut errors, &mut notices);

    assert_eq!(errors.len(), 5, "{errors:#?}");
    assert!(errors.iter().any(|error| error.contains("fs/reject.rs")));
    assert!(
        errors
            .iter()
            .any(|error| error.contains("unit/src/reject.rs"))
    );
    assert!(
        errors
            .iter()
            .any(|error| error.contains("scheduler-unit/src/reject.rs"))
    );
    assert!(errors.iter().any(|error| error.contains("probe_tests.rs")));
    assert!(errors.iter().any(|error| error.contains("user/reject.c")));
    assert_eq!(notices.len(), 4, "{notices:#?}");
    assert!(notices.iter().any(|notice| notice.contains("fs/review.rs")));
    assert!(
        notices
            .iter()
            .any(|notice| notice.contains("unit/src/review.rs"))
    );
    assert!(
        notices
            .iter()
            .any(|notice| notice.contains("fs/review_limit.rs"))
    );
    assert!(
        notices
            .iter()
            .any(|notice| notice.contains("unit/src/review_limit.rs"))
    );
}

#[test]
fn production_review_registry_preserves_its_exact_limit_ratchet() {
    let repository = TestRepository::new();
    repository.review("kernel/src/fs/large.rs", 700, "fs");
    let mut errors = Vec::new();
    let mut notices = Vec::new();

    check(
        &repository.root,
        &[production("kernel/src/fs/large.rs", "fs", 700)],
        &mut errors,
        &mut notices,
    );
    assert!(errors.is_empty(), "{errors:#?}");
    assert!(notices.is_empty(), "{notices:#?}");

    check(
        &repository.root,
        &[production("kernel/src/fs/large.rs", "fs", 699)],
        &mut errors,
        &mut notices,
    );
    assert_eq!(errors.len(), 1, "{errors:#?}");
    assert!(errors[0].contains("preserve the ratchet"));
}
