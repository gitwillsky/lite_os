use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
};

use syn::{
    Expr, ImplItemFn, ItemEnum, ItemFn, ItemMod, ItemStatic, ItemStruct, ItemType, ItemUnion,
    ItemUse, Macro, Path as SynPath, UseTree, spanned::Spanned, visit::Visit,
};

use super::{SourceFile, normalized};

#[cfg(test)]
mod fallible_collections_contract_tests;

#[derive(Default)]
struct PathNameVisitor {
    names: BTreeMap<String, BTreeSet<usize>>,
}

impl<'ast> Visit<'ast> for PathNameVisitor {
    fn visit_path(&mut self, path: &'ast SynPath) {
        for segment in &path.segments {
            let name = segment.ident.to_string();
            self.names
                .entry(name)
                .or_default()
                .insert(segment.ident.span().start().line);
        }
        syn::visit::visit_path(self, path);
    }

    fn visit_use_tree(&mut self, tree: &'ast UseTree) {
        let identifier = match tree {
            UseTree::Path(path) => Some(&path.ident),
            UseTree::Name(name) => Some(&name.ident),
            UseTree::Rename(rename) => Some(&rename.ident),
            UseTree::Glob(_) | UseTree::Group(_) => None,
        };
        if let Some(identifier) = identifier {
            self.names
                .entry(identifier.to_string())
                .or_default()
                .insert(identifier.span().start().line);
        }
        syn::visit::visit_use_tree(self, tree);
    }
}

fn type_contains_path(ty: &syn::Type, name: &str) -> bool {
    let mut visitor = PathNameVisitor::default();
    visitor.visit_type(ty);
    visitor.names.contains_key(name)
}

struct PersistentFallibleMapVisitor<'a> {
    relative: &'a str,
    owners: Vec<String>,
    entries: BTreeMap<String, String>,
    aliases: Vec<usize>,
}

impl PersistentFallibleMapVisitor<'_> {
    fn owner(&self) -> String {
        self.owners.join("::")
    }

    fn record(&mut self, identity: String, ty: &syn::Type) {
        if !type_contains_path(ty, "FallibleMap") {
            return;
        }
        let location = format!("{} :: {identity}", self.relative);
        let previous = self.entries.insert(location.clone(), normalized(ty));
        assert!(
            previous.is_none(),
            "duplicate persistent map identity: {location}"
        );
    }

    fn record_fields(&mut self, fields: &syn::Fields, prefix: &str) {
        for (index, field) in fields.iter().enumerate() {
            let identity = field.ident.as_ref().map_or_else(
                || format!("{prefix}[{index}]"),
                |name| format!("{prefix}.{name}"),
            );
            self.record(identity, &field.ty);
        }
    }
}

impl<'ast> Visit<'ast> for PersistentFallibleMapVisitor<'_> {
    fn visit_item_mod(&mut self, item: &'ast ItemMod) {
        if item.content.is_some() {
            self.owners.push(item.ident.to_string());
            syn::visit::visit_item_mod(self, item);
            self.owners.pop();
        }
    }

    fn visit_item_struct(&mut self, item: &'ast ItemStruct) {
        self.owners.push(item.ident.to_string());
        self.record_fields(&item.fields, &self.owner());
        self.owners.pop();
    }

    fn visit_item_union(&mut self, item: &'ast ItemUnion) {
        self.owners.push(item.ident.to_string());
        self.record_fields(&syn::Fields::Named(item.fields.clone()), &self.owner());
        self.owners.pop();
    }

    fn visit_item_enum(&mut self, item: &'ast ItemEnum) {
        self.owners.push(item.ident.to_string());
        let owner = self.owner();
        for variant in &item.variants {
            self.record_fields(&variant.fields, &format!("{owner}::{}", variant.ident));
        }
        self.owners.pop();
    }

    fn visit_item_static(&mut self, item: &'ast ItemStatic) {
        self.record(format!("static {}", item.ident), &item.ty);
    }

    fn visit_item_type(&mut self, item: &'ast ItemType) {
        if type_contains_path(&item.ty, "FallibleMap") {
            self.aliases.push(item.ident.span().start().line);
        }
    }

    fn visit_item_use(&mut self, item: &'ast ItemUse) {
        fn hides_fallible_map(tree: &UseTree, in_fallible_tree: bool) -> bool {
            match tree {
                UseTree::Path(path) => hides_fallible_map(
                    &path.tree,
                    in_fallible_tree || path.ident == "fallible_tree",
                ),
                UseTree::Rename(rename) => rename.ident == "FallibleMap",
                UseTree::Glob(_) => in_fallible_tree,
                UseTree::Group(group) => group
                    .items
                    .iter()
                    .any(|tree| hides_fallible_map(tree, in_fallible_tree)),
                UseTree::Name(_) => false,
            }
        }

        if hides_fallible_map(&item.tree, false) {
            self.aliases.push(item.use_token.span.start().line);
        }
    }

    // Function-local scratch maps are transactions, not persistent owner fields.
    fn visit_item_fn(&mut self, _item: &'ast ItemFn) {}

    fn visit_impl_item_fn(&mut self, _item: &'ast ImplItemFn) {}
}

const FALLIBLE_MAP_REGISTRY_HEADING: &str = "### Persistent FallibleMap registry";

fn registered_fallible_maps(root: &Path) -> Result<BTreeMap<String, String>, String> {
    let path = root.join("docs/architecture-contract.md");
    let text = fs::read_to_string(&path)
        .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
    let section = text
        .split_once(FALLIBLE_MAP_REGISTRY_HEADING)
        .map(|(_, section)| section)
        .ok_or_else(|| {
            format!("docs/architecture-contract.md lacks `{FALLIBLE_MAP_REGISTRY_HEADING}`")
        })?;
    let mut entries = BTreeMap::new();
    for line in section.lines().skip(1) {
        if line.starts_with("## ") || line.starts_with("### ") {
            break;
        }
        if !line.starts_with('|') || line.contains("---") || line.contains("Location") {
            continue;
        }
        let columns: Vec<&str> = line.split('|').map(str::trim).collect();
        if columns.len() != 4
            || !columns[1].starts_with('`')
            || !columns[1].ends_with('`')
            || !columns[2].starts_with('`')
            || !columns[2].ends_with('`')
        {
            return Err(format!(
                "malformed persistent FallibleMap registry row: {line}"
            ));
        }
        let location = columns[1].trim_matches('`').to_owned();
        let ty = columns[2].trim_matches('`').to_owned();
        if !location.starts_with("kernel/src/") || !ty.contains("FallibleMap") {
            return Err(format!(
                "invalid persistent FallibleMap registry row: {line}"
            ));
        }
        if entries.insert(location.clone(), ty).is_some() {
            return Err(format!(
                "duplicate persistent FallibleMap registry entry: {location}"
            ));
        }
    }
    Ok(entries)
}

#[derive(Default)]
struct FallibleTreeAllocationVisitor {
    current_function: Option<String>,
    node_allocations: Vec<(String, usize)>,
    forbidden: Vec<(usize, String)>,
    calls: BTreeMap<String, BTreeSet<String>>,
}

impl FallibleTreeAllocationVisitor {
    fn visit_function(&mut self, name: String, body: &syn::Block) {
        let previous = self.current_function.replace(name);
        self.visit_block(body);
        self.current_function = previous;
    }

    fn record_forbidden(&mut self, line: usize, detail: impl Into<String>) {
        self.forbidden.push((line, detail.into()));
    }

    fn record_call(&mut self, callee: String) {
        if let Some(caller) = &self.current_function {
            self.calls.entry(caller.clone()).or_default().insert(callee);
        }
    }

    fn record_heap_type(&mut self, identifier: &syn::Ident) {
        const FORBIDDEN_HEAP_TYPES: &[&str] = &[
            "Arc",
            "BTreeMap",
            "BTreeSet",
            "BinaryHeap",
            "HashMap",
            "HashSet",
            "LinkedList",
            "Rc",
            "String",
            "Vec",
            "VecDeque",
        ];
        let name = identifier.to_string();
        if FORBIDDEN_HEAP_TYPES.contains(&name.as_str()) {
            self.record_forbidden(
                identifier.span().start().line,
                format!("heap-backed type `{name}`"),
            );
        }
    }
}

impl<'ast> Visit<'ast> for FallibleTreeAllocationVisitor {
    fn visit_item_fn(&mut self, item: &'ast ItemFn) {
        self.visit_function(item.sig.ident.to_string(), &item.block);
    }

    fn visit_impl_item_fn(&mut self, item: &'ast ImplItemFn) {
        self.visit_function(item.sig.ident.to_string(), &item.block);
    }

    fn visit_path(&mut self, path: &'ast SynPath) {
        for segment in &path.segments {
            self.record_heap_type(&segment.ident);
        }
        syn::visit::visit_path(self, path);
    }

    fn visit_use_tree(&mut self, tree: &'ast UseTree) {
        match tree {
            UseTree::Path(path) => self.record_heap_type(&path.ident),
            UseTree::Name(name) => self.record_heap_type(&name.ident),
            UseTree::Rename(rename) => {
                self.record_heap_type(&rename.ident);
                let original = rename.ident.to_string();
                if original == "Box"
                    || matches!(original.as_str(), "alloc" | "alloc_zeroed" | "realloc")
                {
                    self.record_forbidden(
                        rename.ident.span().start().line,
                        format!("aliased allocator `{original}`"),
                    );
                }
            }
            UseTree::Glob(glob) => self.record_forbidden(
                glob.star_token.span.start().line,
                "glob import with opaque allocation surface",
            ),
            UseTree::Group(_) => {}
        }
        syn::visit::visit_use_tree(self, tree);
    }

    fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
        if let Expr::Path(function) = call.func.as_ref() {
            let segments: Vec<_> = function.path.segments.iter().collect();
            let first = segments.first().map(|segment| segment.ident.to_string());
            let last = segments.last().map(|segment| segment.ident.to_string());
            if let Some(last) = &last {
                self.record_call(last.clone());
            }
            if first.as_deref() == Some("Box") {
                match last.as_deref() {
                    Some("try_new_uninit") if normalized(&function.path).contains("Node") => {
                        self.node_allocations.push((
                            self.current_function
                                .clone()
                                .unwrap_or_else(|| "<unknown>".to_owned()),
                            function.path.span().start().line,
                        ));
                    }
                    Some(
                        method @ ("new" | "new_uninit" | "new_zeroed" | "pin" | "try_new"
                        | "try_new_zeroed" | "try_pin"),
                    ) => self.record_forbidden(
                        function.path.span().start().line,
                        format!("Box::{method}"),
                    ),
                    _ => {}
                }
            }
            if matches!(last.as_deref(), Some("alloc" | "alloc_zeroed" | "realloc")) {
                self.record_forbidden(
                    function.path.span().start().line,
                    normalized(&function.path),
                );
            }
        }
        syn::visit::visit_expr_call(self, call);
    }

    fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
        let method = call.method.to_string();
        self.record_call(method.clone());
        if matches!(
            method.as_str(),
            "collect"
                | "extend"
                | "extend_from_slice"
                | "reserve"
                | "reserve_exact"
                | "resize"
                | "shrink_to"
                | "shrink_to_fit"
                | "to_string"
                | "to_vec"
                | "try_reserve"
                | "try_reserve_exact"
        ) {
            self.record_forbidden(call.method.span().start().line, format!(".{method}()"));
        }
        syn::visit::visit_expr_method_call(self, call);
    }

    fn visit_macro(&mut self, node: &'ast Macro) {
        let name = node
            .path
            .segments
            .last()
            .map(|segment| segment.ident.to_string())
            .unwrap_or_default();
        if !matches!(name.as_str(), "assert" | "debug_assert") {
            self.record_forbidden(
                node.path.span().start().line,
                format!("opaque `{name}!` macro"),
            );
        }
        syn::visit::visit_macro(self, node);
    }
}

/// 检查 persistent `FallibleMap` registry 及 fallible tree 的唯一分配调用图。
///
/// `root` 定位 registry，`sources` 是已解析的 production source；owner、别名、类型与
/// 分配路径的全部失败都追加到 `errors`。
pub(super) fn check(root: &Path, sources: &[SourceFile], errors: &mut Vec<String>) {
    let mut actual = BTreeMap::new();
    for source in sources
        .iter()
        .filter(|source| source.relative.starts_with("kernel/src/"))
    {
        let mut paths = PathNameVisitor::default();
        paths.visit_file(&source.syntax);
        for name in ["BTreeMap", "BTreeSet"] {
            if let Some(lines) = paths.names.get(name) {
                for line in lines {
                    errors.push(format!(
                        "{}: kernel must use the fallible ordered-map seam, not `{name}`",
                        source.at(*line)
                    ));
                }
            }
        }

        let mut visitor = PersistentFallibleMapVisitor {
            relative: &source.relative,
            owners: Vec::new(),
            entries: BTreeMap::new(),
            aliases: Vec::new(),
        };
        visitor.visit_file(&source.syntax);
        for line in visitor.aliases {
            errors.push(format!(
                "{}: persistent FallibleMap aliases bypass the exact owner registry",
                source.at(line)
            ));
        }
        for (location, ty) in visitor.entries {
            if actual.insert(location.clone(), ty).is_some() {
                errors.push(format!(
                    "duplicate persistent FallibleMap owner: {location}"
                ));
            }
        }
    }

    match registered_fallible_maps(root) {
        Ok(registered) => {
            for (location, expected_type) in &registered {
                match actual.get(location) {
                    None => errors.push(format!(
                        "registered persistent FallibleMap was removed or replaced: {location}"
                    )),
                    Some(actual_type) if actual_type != expected_type => errors.push(format!(
                        "persistent FallibleMap type changed at {location}: expected `{expected_type}`, found `{actual_type}`"
                    )),
                    Some(_) => {}
                }
            }
            for (location, ty) in &actual {
                if !registered.contains_key(location) {
                    errors.push(format!(
                        "persistent FallibleMap lacks architecture contract entry: {location} :: `{ty}`"
                    ));
                }
            }
        }
        Err(error) => errors.push(error),
    }

    let mut allocation_sites = Vec::new();
    let mut calls: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for source in sources.iter().filter(|source| {
        source.relative == "kernel/src/fallible_tree.rs"
            || source.relative.starts_with("kernel/src/fallible_tree/")
    }) {
        let mut visitor = FallibleTreeAllocationVisitor::default();
        visitor.visit_file(&source.syntax);
        for (line, detail) in visitor.forbidden {
            errors.push(format!(
                "{}: fallible_tree topology/iteration must not allocate through {detail}",
                source.at(line)
            ));
        }
        allocation_sites.extend(
            visitor
                .node_allocations
                .into_iter()
                .map(|(function, line)| (source.relative.clone(), function, line)),
        );
        for (caller, callees) in visitor.calls {
            calls.entry(caller).or_default().extend(callees);
        }
    }
    if allocation_sites.len() != 1
        || allocation_sites
            .first()
            .is_none_or(|(source, function, _)| {
                source != "kernel/src/fallible_tree.rs" || function != "try_reserve_node"
            })
    {
        let sites = allocation_sites
            .iter()
            .map(|(source, function, line)| format!("{source}:{line}::{function}"))
            .collect::<Vec<_>>()
            .join(", ");
        errors.push(format!(
            "fallible_tree must have exactly one Box::try_new_uninit node allocation in try_reserve_node; found [{sites}]"
        ));
    }

    // 1. 从唯一 allocation primitive 反向闭包全部内部 caller。
    // 2. 只允许四个显式 fallible prepare/insert API 可达该 primitive。
    // 3. rotation/remove/retain/iteration 若直接或间接接入分配，都会成为额外 caller 并失败。
    let mut allocating = BTreeSet::from(["try_reserve_node".to_owned()]);
    loop {
        let previous = allocating.len();
        for (caller, callees) in &calls {
            if callees.iter().any(|callee| allocating.contains(callee)) {
                allocating.insert(caller.clone());
            }
        }
        if allocating.len() == previous {
            break;
        }
    }
    let expected_allocating = BTreeSet::from([
        "try_insert".to_owned(),
        "try_prepare".to_owned(),
        "try_prepare_vacant".to_owned(),
        "try_reserve_node".to_owned(),
    ]);
    if allocating != expected_allocating {
        errors.push(format!(
            "fallible_tree allocation call graph changed: expected {expected_allocating:?}, found {allocating:?}"
        ));
    }
}
