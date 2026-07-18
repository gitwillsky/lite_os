use std::{fs, path::Path};

use super::{DEFAULT_SOURCE_REJECTION_LIMIT, SOURCE_REVIEW_LINE_THRESHOLD, rust_files};

/// architecture-check 必须递归扫描的 host unit-test source roots。
pub(super) const UNIT_SOURCE_ROOTS: [&str; 2] =
    ["tools/kernel-unit/src", "tools/scheduler-unit/src"];
/// 生产工具 crate 中按命名约定隔离的 unit-test module roots。
pub(super) const UNIT_TEST_MODULE_ROOTS: [&str; 1] = ["tools/architecture-check/src"];

/// Unit-test source line count 对应的 architecture gate 结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum UnitSourceSizeDisposition {
    /// 文件未越过 review threshold。
    Accepted,
    /// 文件需要 owner/seam review，但尚未越过 rejection limit。
    Review,
    /// 文件越过 rejection limit，必须拆分。
    Reject,
}

/// 按 production Rust source 的同一阈值分类 unit-test source。
///
/// # Parameters
///
/// - `lines`: source file 的逻辑行数。
///
/// # Returns
///
/// `<=600` 接受，`601..=1200` 触发 review，`>1200` 拒绝。
pub(super) const fn classify(lines: usize) -> UnitSourceSizeDisposition {
    if lines > DEFAULT_SOURCE_REJECTION_LIMIT {
        UnitSourceSizeDisposition::Reject
    } else if lines > SOURCE_REVIEW_LINE_THRESHOLD {
        UnitSourceSizeDisposition::Review
    } else {
        UnitSourceSizeDisposition::Accepted
    }
}

/// 对 host test crates 与 production tool 中独立命名的 test modules 执行递归 size gate。
///
/// # Parameters
///
/// - `root`: repository root。
/// - `errors`: 追加读取失败或超过 rejection limit 的错误。
/// - `review_notices`: 追加超过 review threshold 的 architecture notice。
///
/// # Returns
///
/// 结果通过两个 caller-owned diagnostics collections 返回。
pub(super) fn check_unit_source_sizes(
    root: &Path,
    errors: &mut Vec<String>,
    review_notices: &mut Vec<String>,
) {
    for relative_root in UNIT_SOURCE_ROOTS {
        check_source_root(root, relative_root, false, errors, review_notices);
    }
    for relative_root in UNIT_TEST_MODULE_ROOTS {
        check_source_root(root, relative_root, true, errors, review_notices);
    }
}

fn check_source_root(
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
        match classify(lines) {
                UnitSourceSizeDisposition::Accepted => {}
                UnitSourceSizeDisposition::Review => review_notices.push(format!(
                    "{relative}: {lines} lines exceeds the {SOURCE_REVIEW_LINE_THRESHOLD}-line review threshold; split tests at their domain seam before adding more cases"
                )),
                UnitSourceSizeDisposition::Reject => errors.push(format!(
                    "{relative}: {lines} lines exceeds the {DEFAULT_SOURCE_REJECTION_LIMIT}-line rejection limit; split tests at their domain seam"
                )),
            }
    }
}
