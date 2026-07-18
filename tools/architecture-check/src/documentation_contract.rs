use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Component, Path, PathBuf},
};

use syn::parse_file;

use super::{SourceFile, syscall_entries};

#[cfg(test)]
mod documentation_contract_tests;
mod interface;

const ENTRY_DOCUMENTS: &[&str] = &[
    "README.md",
    "AGENTS.md",
    "docs/README.md",
    "docs/architecture.md",
    "docs/architecture-contract.md",
    "docs/syscall-support.md",
];
const ENTRY_DOCUMENT_MAX_LINES: usize = 200;
const ENTRY_DOCUMENT_MAX_BYTES: usize = 32 * 1024;
const DOMAIN_DOCUMENT_MAX_LINES: usize = 500;
const DOMAIN_DOCUMENT_MAX_BYTES: usize = 96 * 1024;
const DOCUMENT_MAX_LINE_BYTES: usize = 320;

fn markdown_files(directory: &Path, files: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries = fs::read_dir(directory)
        .map_err(|error| format!("failed to read {}: {error}", directory.display()))?;
    for entry in entries {
        let path = entry
            .map_err(|error| format!("failed to read directory entry: {error}"))?
            .path();
        if path.is_dir() {
            markdown_files(&path, files)?;
        } else if path.extension().is_some_and(|extension| extension == "md") {
            files.push(path);
        }
    }
    Ok(())
}

fn load_manual_documents(root: &Path) -> Result<BTreeMap<String, String>, String> {
    let mut paths = vec![root.join("README.md"), root.join("AGENTS.md")];
    markdown_files(&root.join("docs"), &mut paths)?;
    paths.sort();
    paths
        .into_iter()
        .map(|path| {
            let relative = path
                .strip_prefix(root)
                .map_err(|error| format!("invalid document path {}: {error}", path.display()))?
                .to_string_lossy()
                .replace(std::path::MAIN_SEPARATOR, "/");
            let text = fs::read_to_string(&path)
                .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
            Ok((relative, text))
        })
        .collect()
}

fn markdown_targets(text: &str) -> Vec<&str> {
    let mut targets = Vec::new();
    let mut remainder = text;
    while let Some(start) = remainder.find("](") {
        remainder = &remainder[start + 2..];
        let Some(end) = remainder.find(')') else {
            break;
        };
        let raw = remainder[..end].trim();
        let raw = raw.strip_prefix('<').unwrap_or(raw);
        let raw = raw.strip_suffix('>').unwrap_or(raw);
        if let Some(target) = raw.split_whitespace().next()
            && !target.is_empty()
        {
            targets.push(target);
        }
        remainder = &remainder[end + 1..];
    }
    targets
}

fn normalize_local_link(source: &str, target: &str) -> Result<Option<String>, String> {
    if target.starts_with('#')
        || target.starts_with("http://")
        || target.starts_with("https://")
        || target.starts_with("mailto:")
    {
        return Ok(None);
    }
    let without_fragment = target.split_once('#').map_or(target, |(path, _)| path);
    let target = without_fragment
        .split_once('?')
        .map_or(without_fragment, |(path, _)| path);
    if target.is_empty() {
        return Ok(None);
    }
    let base = Path::new(source).parent().unwrap_or_else(|| Path::new(""));
    let mut normalized = PathBuf::new();
    for component in base.join(target).components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => normalized.push(part),
            Component::ParentDir => {
                if !normalized.pop() {
                    return Err(format!("link escapes repository root: {target}"));
                }
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(format!("absolute local link is forbidden: {target}"));
            }
        }
    }
    Ok(Some(
        normalized
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/"),
    ))
}

fn check_document_sizes(documents: &BTreeMap<String, String>, errors: &mut Vec<String>) {
    for (path, text) in documents {
        let entry = ENTRY_DOCUMENTS.contains(&path.as_str());
        let line_limit = if entry {
            ENTRY_DOCUMENT_MAX_LINES
        } else {
            DOMAIN_DOCUMENT_MAX_LINES
        };
        let byte_limit = if entry {
            ENTRY_DOCUMENT_MAX_BYTES
        } else {
            DOMAIN_DOCUMENT_MAX_BYTES
        };
        let lines = text.lines().count();
        if lines > line_limit || text.len() > byte_limit {
            errors.push(format!(
                "{path}: document size {lines} lines/{} bytes exceeds {line_limit} lines/{byte_limit} bytes",
                text.len()
            ));
        }
        for (index, line) in text.lines().enumerate() {
            if line.len() > DOCUMENT_MAX_LINE_BYTES {
                errors.push(format!(
                    "{path}:{}: line is {} bytes; manual Markdown limit is {DOCUMENT_MAX_LINE_BYTES}",
                    index + 1,
                    line.len()
                ));
            }
        }
    }
}

fn check_document_inventory(
    root: &Path,
    documents: &BTreeMap<String, String>,
    errors: &mut Vec<String>,
) {
    let Some(index) = documents.get("docs/README.md") else {
        errors.push("missing canonical documentation index: docs/README.md".to_owned());
        return;
    };
    let mut indexed = BTreeSet::new();
    for target in markdown_targets(index) {
        match normalize_local_link("docs/README.md", target) {
            Ok(Some(path)) if path.ends_with(".md") => {
                indexed.insert(path);
            }
            Ok(_) => {}
            Err(error) => errors.push(format!("docs/README.md: {error}")),
        }
    }
    let expected = documents
        .keys()
        .filter(|path| path.starts_with("docs/") && path.as_str() != "docs/README.md")
        .cloned()
        .collect::<BTreeSet<_>>();
    for path in expected.difference(&indexed) {
        errors.push(format!("docs/README.md does not index {path}"));
    }
    for path in indexed.difference(&expected) {
        errors.push(format!(
            "docs/README.md indexes non-Markdown or missing document {path}"
        ));
    }

    for (source, text) in documents {
        for target in markdown_targets(text) {
            match normalize_local_link(source, target) {
                Ok(Some(path)) if !root.join(&path).is_file() => {
                    errors.push(format!(
                        "{source}: local link target does not exist: {path}"
                    ));
                }
                Ok(_) => {}
                Err(error) => errors.push(format!("{source}: {error}")),
            }
        }
    }
}

fn check_document_ownership(
    documents: &BTreeMap<String, String>,
    syscall_count: usize,
    errors: &mut Vec<String>,
) {
    let count_claim = format!("{syscall_count} 个 Linux/riscv64 syscall");
    if !documents
        .get("docs/syscall-support.md")
        .is_some_and(|text| text.contains(&count_claim))
    {
        errors.push(format!(
            "docs/syscall-support.md must own the exact syscall count claim: {count_claim}"
        ));
    }
    let prohibited = [
        "HartTopology",
        "HartState",
        "arch::hart",
        "drivers::platform",
        "task::TrapContext",
        "task::TaskContext",
        "kernel/src/arch/riscv64/hart.rs",
        "docs/architecture-interface.txt",
        "architecture/display-terminal.md",
        "architecture-contract/display-terminal.md",
        "syscall/socket.md",
        "禁止维护/修正/执行测试",
        "不维护、修正或执行测试",
        "禁止测试",
        "不得执行测试",
        "测试应理解为心智",
    ];
    let hart_allowed = BTreeSet::from([
        "docs/architecture/boot-platform.md",
        "docs/architecture-contract/boot-platform.md",
        "docs/standards-baseline.md",
    ]);
    let make_allowed = BTreeSet::from([
        "README.md",
        "AGENTS.md",
        "docs/development/build-and-verify.md",
    ]);
    for (path, text) in documents {
        if text.contains("个 Linux/riscv64 syscall") && path.as_str() != "docs/syscall-support.md"
        {
            errors.push(format!(
                "{path}: syscall count belongs only to docs/syscall-support.md"
            ));
        }
        if text.contains("nightly-") && path.as_str() != "docs/standards-baseline.md" {
            errors.push(format!(
                "{path}: exact toolchain revision belongs only to docs/standards-baseline.md"
            ));
        }
        if text.to_ascii_lowercase().contains("hart") && !hart_allowed.contains(path.as_str()) {
            errors.push(format!(
                "{path}: generic documentation must use CPU/CpuId/CpuSet instead of hart"
            ));
        }
        if text
            .lines()
            .any(|line| line.trim_start().starts_with("make "))
            && !make_allowed.contains(path.as_str())
        {
            errors.push(format!(
                "{path}: build/test commands belong only to README, AGENTS or build-and-verify"
            ));
        }
        for phrase in prohibited {
            if text.contains(phrase) {
                errors.push(format!(
                    "{path}: retired documentation term/path remains: {phrase}"
                ));
            }
        }
        if path.starts_with("docs/phase-") || path.starts_with("docs/superpowers/") {
            errors.push(format!(
                "{path}: historical phase/spec documents are forbidden"
            ));
        }
        if path.starts_with("docs/plans/")
            && (!text.contains("Status: Active")
                || ["Status: Complete", "Status: Done", "已完成"]
                    .iter()
                    .any(|marker| text.contains(marker)))
        {
            errors.push(format!(
                "{path}: plan documents are allowed only while explicitly Active"
            ));
        }
    }
}

fn check_syscall_documentation(
    documents: &BTreeMap<String, String>,
    expected: &BTreeMap<String, usize>,
    errors: &mut Vec<String>,
) {
    let matrix_paths = BTreeSet::from([
        "docs/syscall-support/filesystem-io.md",
        "docs/syscall-support/ipc.md",
        "docs/syscall-support/memory.md",
        "docs/syscall-support/process-identity.md",
        "docs/syscall-support/signal-time.md",
        "docs/syscall-support/socket.md",
        "docs/syscall-support/synchronization-scheduling.md",
        "docs/syscall-support/system.md",
    ]);
    let actual = documents
        .keys()
        .filter(|path| path.starts_with("docs/syscall-support/"))
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    if actual != matrix_paths {
        errors.push(format!(
            "syscall domain matrices must be the fixed eight-way split; found {actual:?}"
        ));
    }

    let mut seen: BTreeMap<(usize, String), Vec<&str>> = BTreeMap::new();
    for path in matrix_paths {
        let Some(text) = documents.get(path) else {
            continue;
        };
        for line in text.lines().filter(|line| line.starts_with('|')) {
            let columns = line.split('|').map(str::trim).collect::<Vec<_>>();
            if columns.len() < 5 {
                continue;
            }
            let Ok(number) = columns[1].parse::<usize>() else {
                continue;
            };
            let name = columns[2].trim_matches(char::from(96)).to_owned();
            let status = columns[3];
            if !matches!(status, "Complete" | "Partial") {
                errors.push(format!(
                    "{path}: syscall {number}/{name} has invalid status {status}"
                ));
            }
            if expected.get(&name) != Some(&number) {
                errors.push(format!(
                    "{path}: syscall row {number}/{name} does not match syscall-abi"
                ));
            }
            seen.entry((number, name)).or_default().push(path);
        }
    }
    for (name, number) in expected {
        let key = (*number, name.clone());
        match seen.get(&key).map(Vec::as_slice) {
            Some([_]) => {}
            Some(paths) => errors.push(format!(
                "syscall {number}/{name} is documented {} times: {paths:?}",
                paths.len()
            )),
            None => errors.push(format!(
                "syscall {number}/{name} is absent from domain matrices"
            )),
        }
    }
    for ((number, name), paths) in seen {
        if expected.get(&name) != Some(&number) {
            errors.push(format!(
                "unexpected documented syscall {number}/{name} in {paths:?}"
            ));
        }
    }
}

/// 检查手册 owner、索引、syscall 矩阵及生成的 scoped interface 基线。
///
/// `root` 是仓库根目录，`sources` 是已解析的 production source；`write_interface`
/// 仅在此前没有错误时允许重写基线，所有读取、契约和写入失败都追加到 `errors`。
pub(super) fn check(
    root: &Path,
    sources: &[SourceFile],
    write_interface: bool,
    errors: &mut Vec<String>,
) {
    let documents = match load_manual_documents(root) {
        Ok(documents) => documents,
        Err(error) => {
            errors.push(error);
            return;
        }
    };
    let abi_text = fs::read_to_string(root.join("syscall-abi/src/lib.rs"))
        .expect("syscall ABI source must exist");
    let abi = parse_file(&abi_text).expect("syscall ABI must parse");
    let syscalls = syscall_entries(&abi);
    check_document_sizes(&documents, errors);
    check_document_inventory(root, &documents, errors);
    check_document_ownership(&documents, syscalls.len(), errors);
    check_syscall_documentation(&documents, &syscalls, errors);
    interface::check(root, sources, write_interface, errors);
}
