use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
};

use syn::{ItemUse, Path as SynPath, UseTree, visit::Visit};

use super::SourceFile;

const KERNEL_MODULES: &[&str] = &[
    "arch",
    "config",
    "cpu",
    "drivers",
    "drm",
    "entry",
    "fallible_tree",
    "fs",
    "id",
    "input",
    "ipc",
    "lang_item",
    "log",
    "main",
    "memory",
    "platform",
    "random",
    "socket",
    "sync",
    "syscall",
    "system",
    "task",
    "timer",
    "trap",
];

#[derive(Default)]
struct PathCollector {
    paths: Vec<Vec<String>>,
    crate_root_alias: bool,
}

impl<'ast> Visit<'ast> for PathCollector {
    fn visit_path(&mut self, path: &'ast SynPath) {
        self.paths.push(
            path.segments
                .iter()
                .map(|segment| segment.ident.to_string())
                .collect(),
        );
        syn::visit::visit_path(self, path);
    }

    fn visit_item_use(&mut self, item: &'ast ItemUse) {
        self.crate_root_alias |= aliases_crate_root(&item.tree);
        expand_use_tree(Vec::new(), &item.tree, &mut self.paths);
    }
}

fn aliases_crate_root(tree: &UseTree) -> bool {
    match tree {
        UseTree::Rename(rename) => rename.ident == "crate",
        UseTree::Name(name) => name.ident == "crate",
        UseTree::Path(path) if path.ident == "crate" => matches!(*path.tree, UseTree::Glob(_)),
        _ => false,
    }
}

fn expand_use_tree(prefix: Vec<String>, tree: &UseTree, paths: &mut Vec<Vec<String>>) {
    match tree {
        UseTree::Path(path) => {
            let mut next = prefix;
            next.push(path.ident.to_string());
            expand_use_tree(next, &path.tree, paths);
        }
        UseTree::Name(name) => {
            let mut path = prefix;
            path.push(name.ident.to_string());
            paths.push(path);
        }
        UseTree::Rename(rename) => {
            let mut path = prefix;
            path.push(rename.ident.to_string());
            paths.push(path);
        }
        UseTree::Glob(_) => paths.push(prefix),
        UseTree::Group(group) => {
            for item in &group.items {
                expand_use_tree(prefix.clone(), item, paths);
            }
        }
    }
}

fn allowed_dependencies(root: &Path) -> Result<BTreeMap<String, BTreeSet<String>>, String> {
    let path = root.join("docs/architecture-contract.md");
    let text = fs::read_to_string(&path)
        .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
    let mut rules = BTreeMap::new();
    for line in text.lines() {
        let columns: Vec<&str> = line.split('|').map(str::trim).collect();
        if columns.len() < 5 || !columns[1].starts_with('`') {
            continue;
        }
        let owner = columns[1].trim_matches('`');
        if !KERNEL_MODULES.contains(&owner) {
            continue;
        }
        let mut allowed = BTreeSet::new();
        let mut remainder = columns[2];
        while let Some(start) = remainder.find('`') {
            remainder = &remainder[start + 1..];
            let Some(end) = remainder.find('`') else {
                return Err(format!("{owner}: malformed dependency matrix row"));
            };
            let dependency = &remainder[..end];
            if !KERNEL_MODULES.contains(&dependency) {
                return Err(format!(
                    "{owner}: dependency matrix names unknown module `{dependency}`"
                ));
            }
            allowed.insert(dependency.to_owned());
            remainder = &remainder[end + 1..];
        }
        if rules.insert(owner.to_owned(), allowed).is_some() {
            return Err(format!(
                "docs/architecture-contract.md contains duplicate dependency rows for `{owner}`"
            ));
        }
    }
    for module in KERNEL_MODULES {
        if !rules.contains_key(*module) {
            return Err(format!(
                "docs/architecture-contract.md lacks a dependency row for `{module}`"
            ));
        }
    }
    Ok(rules)
}

/// @description 对已加载源码执行正向 module dependency 与 façade containment 契约。
/// @param root 定位权威 dependency matrix；sources 是统一源码快照；errors 接收违规。
/// @return 无；全部违规一次收集。
/// @errors matrix 读取/格式错误与源码违规均追加到 errors。
pub(super) fn check(root: &Path, sources: &[SourceFile], errors: &mut Vec<String>) {
    let allowlist = match allowed_dependencies(root) {
        Ok(allowlist) => allowlist,
        Err(error) => {
            errors.push(error);
            return;
        }
    };
    let known: BTreeSet<&str> = KERNEL_MODULES.iter().copied().collect();
    for source in sources
        .iter()
        .filter(|source| source.relative.starts_with("kernel/src/"))
    {
        let mut collector = PathCollector::default();
        collector.visit_file(&source.syntax);
        if collector.crate_root_alias {
            errors.push(format!(
                "{}: aliasing or glob-importing the crate root bypasses dependency ownership",
                source.relative
            ));
        }
        let mut dependencies = BTreeSet::new();
        for path in &collector.paths {
            if path.first().is_some_and(|segment| segment == "crate")
                && let Some(module) = path.get(1)
                && known.contains(module.as_str())
                && module != &source.owner
            {
                dependencies.insert(module.as_str());
            }
            check_facade_path(source, path, errors);
        }
        let Some(allowed) = allowlist.get(source.owner.as_str()) else {
            errors.push(format!(
                "{}: kernel module `{}` has no positive dependency contract",
                source.relative, source.owner
            ));
            continue;
        };
        for dependency in dependencies {
            if allowed.contains(dependency) {
                continue;
            }
            errors.push(format!(
                "{}: crate::{} is absent from the positive dependency contract for {}",
                source.relative, dependency, source.owner
            ));
        }
    }

    for source in sources
        .iter()
        .filter(|source| source.relative.starts_with("bootloader/src/"))
    {
        if source.text.contains("kernel::") || source.text.contains("user::") {
            errors.push(format!(
                "{}: bootloader must remain an independent M-mode domain",
                source.relative
            ));
        }
    }
}

fn check_facade_path(source: &SourceFile, path: &[String], errors: &mut Vec<String>) {
    if path.first().is_none_or(|segment| segment != "crate") {
        return;
    }
    if source.owner == "fs"
        && path.get(1).is_some_and(|segment| segment == "drivers")
        && path.get(2).is_none_or(|segment| segment != "block")
    {
        errors.push(format!(
            "{}: filesystem may depend only on the drivers::block seam",
            source.relative
        ));
    }
    if source.owner == "syscall" {
        if matches!(
            path.get(1).map(String::as_str),
            Some("fs" | "memory" | "task")
        ) && path.len() == 2
        {
            errors.push(format!(
                "{}: syscall must import a named domain facade item, not alias its module root",
                source.relative
            ));
        }
        let forbidden = [
            &["crate", "fs", "ext2"][..],
            &["crate", "fs", "file"][..],
            &["crate", "fs", "inode"][..],
            &["crate", "fs", "Ext2FileSystem"][..],
            &["crate", "memory", "page_table"][..],
            &["crate", "task", "scheduler"][..],
        ];
        if forbidden.iter().any(|prefix| {
            path.len() >= prefix.len()
                && path
                    .iter()
                    .map(String::as_str)
                    .zip(prefix.iter().copied())
                    .all(|(actual, expected)| actual == expected)
        }) {
            errors.push(format!(
                "{}: syscall bypasses a domain facade through {}",
                source.relative,
                path.join("::")
            ));
        }
    }
    if source.owner == "task"
        && path.get(1).is_some_and(|segment| segment == "fs")
        && path
            .get(2)
            .is_some_and(|segment| segment == "Ext2FileSystem")
    {
        errors.push(format!(
            "{}: task may consume filesystem interfaces but not the ext2 adapter",
            source.relative
        ));
    }
}
