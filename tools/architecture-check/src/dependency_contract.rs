use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
};

use proc_macro2::{Delimiter, TokenStream, TokenTree};
use quote::quote;
use syn::{ItemUse, Macro, Path as SynPath, UseTree, visit::Visit};

use super::SourceFile;

use super::module_layout;

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

    fn visit_macro(&mut self, node: &'ast Macro) {
        self.collect_macro_paths(node.tokens.clone());
        syn::visit::visit_macro(self, node);
    }
}

impl PathCollector {
    // syn 不解析 macro token body；逐层扫描路径，包含 $crate 与 grouped use，忽略 literal。
    fn collect_macro_paths(&mut self, stream: TokenStream) {
        let tokens: Vec<_> = stream.into_iter().collect();
        for (index, token) in tokens.iter().enumerate() {
            if let TokenTree::Group(group) = token {
                self.collect_macro_paths(group.stream());
            }
            if !matches!(token, TokenTree::Ident(ident) if ident == "crate") {
                continue;
            }
            if matches!(tokens.get(index + 1), Some(TokenTree::Ident(ident)) if ident == "as") {
                self.crate_root_alias = true;
            }
            let mut path = vec!["crate".to_owned()];
            let mut cursor = index + 1;
            while matches!(tokens.get(cursor), Some(TokenTree::Punct(p)) if p.as_char() == ':')
                && matches!(tokens.get(cursor + 1), Some(TokenTree::Punct(p)) if p.as_char() == ':')
            {
                cursor += 2;
                match tokens.get(cursor) {
                    Some(TokenTree::Ident(ident)) => path.push(ident.to_string()),
                    Some(TokenTree::Group(group)) if group.delimiter() == Delimiter::Brace => {
                        // 复用 import tree 展开，使 crate::{task as t, memory::{...}} 同样受检。
                        if let Ok(tree) = syn::parse2::<UseTree>(quote!(#group)) {
                            expand_use_tree(path.clone(), &tree, &mut self.paths);
                            if path.len() == 1 {
                                self.crate_root_alias |= aliases_root_members(&tree);
                            }
                        }
                        break;
                    }
                    Some(TokenTree::Punct(p)) if p.as_char() == '*' && path.len() == 1 => {
                        self.crate_root_alias = true;
                        break;
                    }
                    _ => break,
                }
                cursor += 1;
            }
            self.paths.push(path);
        }
    }
}

fn aliases_root_members(tree: &UseTree) -> bool {
    match tree {
        UseTree::Rename(rename) => rename.ident == "self",
        UseTree::Name(name) => name.ident == "self",
        UseTree::Glob(_) => true,
        UseTree::Group(group) => group.items.iter().any(aliases_root_members),
        _ => false,
    }
}

fn aliases_crate_root(tree: &UseTree) -> bool {
    match tree {
        UseTree::Rename(rename) => rename.ident == "crate",
        UseTree::Name(name) => name.ident == "crate",
        UseTree::Path(path) if path.ident == "crate" => aliases_root_members(&path.tree),
        UseTree::Group(group) => group.items.iter().any(aliases_crate_root),
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
        if !module_layout::known(owner) {
            continue;
        }
        let mut allowed = BTreeSet::new();
        // composition root 已有的“可装配全部领域”契约；集合仍来自封闭的 owner 清单。
        if owner == "main" && columns[2] == "所有已登记领域（仅装配）" {
            allowed.extend(
                module_layout::names()
                    .filter(|module| *module != "main")
                    .map(str::to_owned),
            );
        }
        let mut remainder = columns[2];
        while let Some(start) = remainder.find('`') {
            remainder = &remainder[start + 1..];
            let Some(end) = remainder.find('`') else {
                return Err(format!("{owner}: malformed dependency matrix row"));
            };
            let dependency = &remainder[..end];
            if !module_layout::known(dependency) {
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
    for module in module_layout::names() {
        if !rules.contains_key(module) {
            return Err(format!(
                "docs/architecture-contract.md lacks a dependency row for `{module}`"
            ));
        }
    }
    Ok(rules)
}

// 正向矩阵本身必须是 DAG；否则逐边允许检查会把环依赖合法化，包括不同 target 的边。
fn dependency_cycle(graph: &BTreeMap<String, BTreeSet<String>>) -> Option<Vec<String>> {
    fn visit(
        owner: &str,
        graph: &BTreeMap<String, BTreeSet<String>>,
        active: &mut Vec<String>,
        finished: &mut BTreeSet<String>,
    ) -> Option<Vec<String>> {
        if let Some(start) = active.iter().position(|node| node == owner) {
            let mut cycle = active[start..].to_vec();
            cycle.push(owner.to_owned());
            return Some(cycle);
        }
        if finished.contains(owner) {
            return None;
        }
        active.push(owner.to_owned());
        if let Some(dependencies) = graph.get(owner) {
            for dependency in dependencies {
                if let Some(cycle) = visit(dependency, graph, active, finished) {
                    return Some(cycle);
                }
            }
        }
        active.pop();
        finished.insert(owner.to_owned());
        None
    }

    let mut finished = BTreeSet::new();
    for owner in graph.keys() {
        if let Some(cycle) = visit(owner, graph, &mut Vec::new(), &mut finished) {
            return Some(cycle);
        }
    }
    None
}

/// 对已加载源码执行正向 module dependency 与 façade containment 契约。
///
/// # Parameters
///
/// - `root`: 定位权威 dependency matrix；sources 是统一源码快照；errors 接收违规。
///
/// # Returns
///
/// 无；全部违规一次收集。
///
/// # Errors
///
/// matrix 读取/格式错误与源码违规均追加到 errors。
pub(super) fn check(root: &Path, sources: &[SourceFile], errors: &mut Vec<String>) {
    let allowlist = match allowed_dependencies(root) {
        Ok(allowlist) => allowlist,
        Err(error) => {
            errors.push(error);
            return;
        }
    };
    if let Some(cycle) = dependency_cycle(&allowlist) {
        errors.push(format!(
            "docs/architecture-contract.md: kernel dependency cycle: {}",
            cycle.join(" -> ")
        ));
    }
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
            if path.first().is_some_and(|segment| segment == "crate") {
                if path
                    .get(1)
                    .is_some_and(|segment| module_layout::grouping(std::slice::from_ref(segment)))
                    && (path.len() == 2 || path.get(2).is_some_and(|segment| segment == "*"))
                {
                    errors.push(format!(
                        "{}: import a named domain, not a grouping",
                        source.relative
                    ));
                }
                if path
                    .get(1)
                    .is_some_and(|name| module_layout::obsolete_root(name))
                {
                    errors.push(format!(
                        "{}: obsolete root domain path {}; use its registered namespace",
                        source.relative,
                        path.join("::")
                    ));
                }
                // 命名空间可分组，但依赖边仍落在最具体的稳定领域 owner 上。
                if let Some(module) = module_layout::namespace_owner(&path[1..])
                    && module != source.owner
                {
                    dependencies.insert(module);
                }
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
    let backend = path.get(2).map(String::as_str);
    let concrete_arch = source.owner != "arch"
        && path.get(1).is_some_and(|module| module == "arch")
        && matches!(backend, Some("aarch64" | "riscv64"));
    let concrete_platform = source.owner != "platform"
        && path.get(1).is_some_and(|module| module == "platform")
        && backend == Some("qemu_virt");
    if concrete_arch || concrete_platform {
        errors.push(format!(
            "{}: concrete backend {} must remain behind its domain facade",
            source.relative,
            path.join("::")
        ));
    }
    if source.owner == "syscall" {
        if matches!(path.get(1).map(String::as_str), Some("fs" | "memory")) && path.len() == 2 {
            errors.push(format!(
                "{}: syscall must import a named domain facade item, not alias its module root",
                source.relative
            ));
        }
        if path
            .iter()
            .map(String::as_str)
            .eq(["crate", "execution", "task"])
        {
            errors.push(format!(
                "{}: syscall must import a named domain facade item, not alias its module root",
                source.relative
            ));
        }
        let forbidden = [
            &["crate", "fs", "ext4"][..],
            &["crate", "fs", "file"][..],
            &["crate", "fs", "inode"][..],
            &["crate", "fs", "Ext4FileSystem"][..],
            &["crate", "memory", "page_table"][..],
            &["crate", "execution", "task", "scheduler"][..],
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
            .is_some_and(|segment| segment == "Ext4FileSystem")
    {
        errors.push(format!(
            "{}: task may consume filesystem interfaces but not the ext4 adapter",
            source.relative
        ));
    }
}

#[cfg(test)]
#[path = "dependency_contract_tests.rs"]
mod tests;
