use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
};

use super::{SourceFile, rust_files};

const REVIEW_LINE_THRESHOLD: usize = 600;
const DEFAULT_REJECTION_LIMIT: usize = 1_200;
const UNIT_SOURCE_ROOTS: [&str; 2] = ["tools/kernel-unit/src", "tools/scheduler-unit/src"];
const UNIT_TEST_MODULE_ROOTS: [&str; 1] = ["tools/architecture-check/src"];

struct SourceSizeReview {
    limit: usize,
    owner: String,
    reason: String,
    exit: String,
}

/// 检查 production、unit-test 与 userspace source 的完整 size contract。
///
/// # Parameters
///
/// - `root`: repository root，也是 review registry 与递归 source roots 的共同基准。
/// - `sources`: caller 已解析的全部 production Rust source。
/// - `errors`: 追加 registry、读取或 hard-limit failure。
/// - `review_notices`: 追加超过 review threshold、但尚未达到 hard rejection 的诊断。
pub(super) fn check(
    root: &Path,
    sources: &[SourceFile],
    errors: &mut Vec<String>,
    review_notices: &mut Vec<String>,
) {
    check_production(root, sources, errors, review_notices);
    check_unit_sources(root, errors, review_notices);
    check_user_sources(root, errors);
}

fn reviews(root: &Path) -> Result<BTreeMap<String, SourceSizeReview>, String> {
    let path = root.join("docs/architecture-contract.md");
    let text = fs::read_to_string(&path)
        .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
    let mut reviews = BTreeMap::new();
    for line in text.lines() {
        let columns: Vec<&str> = line.split('|').map(str::trim).collect();
        if columns.len() < 7 || !columns[1].starts_with('`') {
            continue;
        }
        let relative = columns[1].trim_matches('`');
        if !relative.starts_with("kernel/src/") && !relative.starts_with("bootloader/src/") {
            continue;
        }
        let Ok(limit) = columns[2].parse::<usize>() else {
            continue;
        };
        let review = SourceSizeReview {
            limit,
            owner: columns[3].trim_matches('`').to_owned(),
            reason: columns[4].to_owned(),
            exit: columns[5].to_owned(),
        };
        if review.limit <= REVIEW_LINE_THRESHOLD {
            return Err(format!(
                "{relative}: source-size review must exceed the {REVIEW_LINE_THRESHOLD}-line review threshold"
            ));
        }
        if review.owner.is_empty() || review.reason.is_empty() || review.exit.is_empty() {
            return Err(format!(
                "{relative}: source-size review requires owner, reason and exit criterion"
            ));
        }
        if reviews.insert(relative.to_owned(), review).is_some() {
            return Err(format!("duplicate source-size review for {relative}"));
        }
    }
    Ok(reviews)
}

fn check_production(
    root: &Path,
    sources: &[SourceFile],
    errors: &mut Vec<String>,
    review_notices: &mut Vec<String>,
) {
    let reviews = match reviews(root) {
        Ok(reviews) => reviews,
        Err(error) => {
            errors.push(error);
            return;
        }
    };
    let known = sources
        .iter()
        .map(|source| source.relative.as_str())
        .collect::<BTreeSet<_>>();
    for relative in reviews.keys() {
        if !known.contains(relative.as_str()) {
            errors.push(format!(
                "{relative}: source-size review does not name a production Rust source"
            ));
        }
    }
    for source in sources {
        check_production_source(
            source,
            reviews.get(&source.relative),
            errors,
            review_notices,
        );
    }
}

fn check_production_source(
    source: &SourceFile,
    review: Option<&SourceSizeReview>,
    errors: &mut Vec<String>,
    review_notices: &mut Vec<String>,
) {
    let lines = source.lines.len();
    if let Some(review) = review
        && review.owner != source.owner
        && !review.owner.starts_with(&format!("{}::", source.owner))
    {
        errors.push(format!(
            "{}: source-size review owner `{}` does not belong to module `{}`",
            source.relative, review.owner, source.owner
        ));
    }
    let limit = review.map_or(DEFAULT_REJECTION_LIMIT, |review| review.limit);
    if lines > limit {
        let detail = if review.is_some() {
            format!("reviewed source-file limit {limit}")
        } else {
            format!("default rejection limit {DEFAULT_REJECTION_LIMIT}")
        };
        errors.push(format!(
            "{}: {lines} lines exceeds its {detail}; split at a domain seam or record an exact reviewed limit in docs/architecture-contract.md",
            source.relative
        ));
    } else if review.is_some() && lines < limit {
        errors.push(format!(
            "{}: source shrank to {lines} lines; lower its architecture limit from {limit} to preserve the ratchet",
            source.relative
        ));
    } else if lines > REVIEW_LINE_THRESHOLD && review.is_none() {
        review_notices.push(format!(
            "{}: {lines} lines exceeds the {REVIEW_LINE_THRESHOLD}-line review threshold; review its owner/seam and either split it or add owner, reason and exit criterion to docs/architecture-contract.md",
            source.relative
        ));
    }
}

#[derive(Clone, Copy)]
enum UnitDisposition {
    Accepted,
    Review,
    Reject,
}

const fn unit_disposition(lines: usize) -> UnitDisposition {
    if lines > DEFAULT_REJECTION_LIMIT {
        UnitDisposition::Reject
    } else if lines > REVIEW_LINE_THRESHOLD {
        UnitDisposition::Review
    } else {
        UnitDisposition::Accepted
    }
}

fn check_unit_sources(root: &Path, errors: &mut Vec<String>, review_notices: &mut Vec<String>) {
    for relative_root in UNIT_SOURCE_ROOTS {
        check_unit_root(root, relative_root, false, errors, review_notices);
    }
    for relative_root in UNIT_TEST_MODULE_ROOTS {
        check_unit_root(root, relative_root, true, errors, review_notices);
    }
}

fn check_unit_root(
    root: &Path,
    relative_root: &str,
    named_test_modules_only: bool,
    errors: &mut Vec<String>,
    review_notices: &mut Vec<String>,
) {
    let source_root = root.join(relative_root);
    let mut paths = Vec::new();
    if let Err(error) = rust_files(&source_root, &mut paths) {
        errors.push(error);
        return;
    }
    paths.sort();

    for path in paths {
        if named_test_modules_only
            && !path
                .file_stem()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with("_tests"))
        {
            continue;
        }
        let source = match fs::read_to_string(&path) {
            Ok(source) => source,
            Err(error) => {
                errors.push(format!("failed to read {}: {error}", path.display()));
                continue;
            }
        };
        let lines = source.lines().count();
        let relative = path.strip_prefix(root).unwrap_or(&path).display();
        match unit_disposition(lines) {
            UnitDisposition::Accepted => {}
            UnitDisposition::Review => review_notices.push(format!(
                "{relative}: {lines} lines exceeds the {REVIEW_LINE_THRESHOLD}-line review threshold; split tests at their domain seam before adding more cases"
            )),
            UnitDisposition::Reject => errors.push(format!(
                "{relative}: {lines} lines exceeds the {DEFAULT_REJECTION_LIMIT}-line rejection limit; split tests at their domain seam"
            )),
        }
    }
}

fn check_user_sources(root: &Path, errors: &mut Vec<String>) {
    let mut pending = vec![root.join("user")];
    let quickjs_vendor = root.join("user/quickjs-runtime/vendor/quickjs");
    while let Some(directory) = pending.pop() {
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) => {
                errors.push(format!(
                    "failed to inspect {}: {error}",
                    directory.display()
                ));
                continue;
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if path == quickjs_vendor {
                    continue;
                }
                pending.push(path);
                continue;
            }
            let extension = path.extension().and_then(|extension| extension.to_str());
            if !matches!(
                extension,
                Some("rs" | "c" | "h" | "js" | "mjs" | "ts" | "tsx" | "css")
            ) {
                continue;
            }
            let Ok(source) = fs::read_to_string(&path) else {
                errors.push(format!("failed to read user source {}", path.display()));
                continue;
            };
            let lines = source.lines().count();
            if lines > REVIEW_LINE_THRESHOLD {
                let relative = path.strip_prefix(root).unwrap_or(&path).display();
                errors.push(format!(
                    "{relative}: {lines} lines exceeds the hard {REVIEW_LINE_THRESHOLD}-line user-module limit; split at an owner/interface seam"
                ));
            }
        }
    }
}
