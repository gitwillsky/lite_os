use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    path::{Path, PathBuf},
    process::ExitCode,
};

use quote::ToTokens;
use syn::{
    Expr, ExprLit, ExprRange, File, ForeignItemFn, ImplItemFn, ItemFn, ItemForeignMod, ItemImpl,
    ItemStatic, ItemUse, Lit, Macro, Path as SynPath, Signature, UseTree, spanned::Spanned,
    visit::Visit,
};

mod abi_contract;
mod address_space_lock_contract;
mod deferred_context_contract;
mod documentation_contract;
mod epoll_cost;
#[cfg(test)]
mod epoll_cost_tests;
mod ext2_mapping_cost;
mod fallible_collections_contract;
mod fallible_map_cost;
mod filesystem_blocking_lock_contract;
mod fp_context_contract;
mod getdents_cost;
mod huge_page_cost;
mod id_cost;
mod io_copy_cost;
mod log_cost;
mod memory_copy_cost;
mod network_stack_cost;
mod packet_cost;
mod page_table_cost;
mod port_namespace_cost;
mod process_graph_cost;
mod ready_contract;
mod receive_staging_cost;
mod rng_io_cost;
mod scheduler_cost;
mod send_staging_cost;
mod source_size;
#[cfg(test)]
mod source_size_tests;
mod terminal_contract;
mod timer_transaction_cost;
mod translation_fence_contract;
mod unix_connect_contract;
mod user_context_cost;
mod userspace_contract;
mod vfs_opened_cost;
#[cfg(test)]
mod vfs_opened_cost_tests;
mod virtio_blk_completion_contract;
mod virtio_blk_cost;
mod virtio_dma_cost;
mod virtio_gpu_sequence_cost;
mod virtio_net_contract;
mod vma_hot_path;

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

#[derive(Clone, Copy)]
struct SourceDomain {
    root: &'static str,
    binary_crate: bool,
}

const SOURCE_DOMAINS: &[SourceDomain] = &[
    SourceDomain {
        root: "kernel/src",
        binary_crate: true,
    },
    SourceDomain {
        root: "bootloader/src",
        binary_crate: true,
    },
];

struct SourceFile {
    relative: String,
    owner: String,
    text: String,
    lines: Vec<String>,
    syntax: File,
    binary_crate: bool,
}

impl SourceFile {
    fn at(&self, line: usize) -> String {
        format!("{}:{line}", self.relative)
    }

    fn preceding_contains(&self, line: usize, distance: usize, marker: &str) -> bool {
        let start = line.saturating_sub(distance + 1);
        self.lines[start..line.saturating_sub(1).min(self.lines.len())]
            .iter()
            .any(|candidate| candidate.contains(marker))
    }
}

fn main() -> ExitCode {
    let write_interface = env::args().skip(1).any(|arg| arg == "--write-interface");
    let root = repository_root();
    let mut errors = Vec::new();
    let mut review_notices = Vec::new();
    let sources = match load_sources(&root) {
        Ok(sources) => sources,
        Err(error) => {
            eprintln!("architecture fence failed:\n- {error}");
            return ExitCode::FAILURE;
        }
    };

    check_dependencies(&root, &sources, &mut errors);
    source_size::check(&root, &sources, &mut errors, &mut review_notices);
    check_source_patterns(&root, &sources, &mut errors);
    check_architecture_boundaries(&root, &sources, &mut errors);
    address_space_lock_contract::check(&sources, &mut errors);
    deferred_context_contract::check(&sources, &mut errors);
    epoll_cost::check(&root, &mut errors);
    ext2_mapping_cost::check(&root, &mut errors);
    fallible_map_cost::check(&sources, &mut errors);
    fp_context_contract::check(&root, &mut errors);
    filesystem_blocking_lock_contract::check(&sources, &mut errors);
    terminal_contract::check_terminal_contract(&sources, &mut errors);
    timer_transaction_cost::check(&root, &mut errors);
    translation_fence_contract::check(&sources, &mut errors);
    unix_connect_contract::check(&sources, &mut errors);
    user_context_cost::check(&root, &sources, &mut errors);
    virtio_blk_cost::check(&sources, &mut errors);
    virtio_blk_completion_contract::check(&sources, &mut errors);
    virtio_dma_cost::check(&sources, &mut errors);
    virtio_gpu_sequence_cost::check(&sources, &mut errors);
    getdents_cost::check(&root, &sources, &mut errors);
    huge_page_cost::check(&root, &mut errors);
    io_copy_cost::check(&root, &mut errors);
    id_cost::check(&root, &mut errors);
    log_cost::check(&root, &mut errors);
    memory_copy_cost::check(&root, &mut errors);
    network_stack_cost::check(&root, &mut errors);
    packet_cost::check(&root, &mut errors);
    page_table_cost::check(&root, &mut errors);
    port_namespace_cost::check(&root, &mut errors);
    process_graph_cost::check(&sources, &mut errors);
    receive_staging_cost::check(&root, &mut errors);
    rng_io_cost::check(&root, &mut errors);
    scheduler_cost::check(&root, &mut errors);
    send_staging_cost::check(&root, &mut errors);
    vma_hot_path::check(&sources, &mut errors);
    virtio_net_contract::check(&sources, &mut errors);
    vfs_opened_cost::check(&root, &mut errors);
    ready_contract::check(&sources, &mut errors);
    fallible_collections_contract::check(&root, &sources, &mut errors);
    check_global_owners(&sources, &mut errors);
    check_unsafe_proofs(&sources, &mut errors);
    abi_contract::check(&root, &mut errors);
    userspace_contract::check(&root, &mut errors);
    documentation_contract::check(&root, &sources, write_interface, &mut errors);

    if !review_notices.is_empty() {
        eprintln!("architecture review required:");
        for notice in review_notices {
            eprintln!("- {notice}");
        }
    }

    if errors.is_empty() {
        println!("architecture fence passed");
        ExitCode::SUCCESS
    } else {
        eprintln!("architecture fence failed:");
        for error in errors {
            eprintln!("- {error}");
        }
        ExitCode::FAILURE
    }
}

fn repository_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("architecture-check must live under tools/")
        .to_path_buf()
}

fn rust_files(directory: &Path, files: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries = fs::read_dir(directory)
        .map_err(|error| format!("failed to read {}: {error}", directory.display()))?;
    for entry in entries {
        let path = entry
            .map_err(|error| format!("failed to read directory entry: {error}"))?
            .path();
        if path.is_dir() {
            rust_files(&path, files)?;
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            files.push(path);
        }
    }
    Ok(())
}

fn load_sources(root: &Path) -> Result<Vec<SourceFile>, String> {
    let mut sources = Vec::new();
    for domain in SOURCE_DOMAINS {
        let source_root = root.join(domain.root);
        let mut paths = Vec::new();
        rust_files(&source_root, &mut paths)?;
        paths.sort();
        for path in paths {
            let text = fs::read_to_string(&path)
                .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
            let syntax = syn::parse_file(&text)
                .map_err(|error| format!("failed to parse {}: {error}", path.display()))?;
            let relative_path = path
                .strip_prefix(root)
                .map_err(|error| format!("invalid source path {}: {error}", path.display()))?;
            let owner = module_owner(relative_path, domain.root);
            sources.push(SourceFile {
                relative: relative_path.to_string_lossy().replace('\\', "/"),
                owner,
                lines: text.lines().map(str::to_owned).collect(),
                text,
                syntax,
                binary_crate: domain.binary_crate,
            });
        }
    }
    Ok(sources)
}

fn module_owner(relative: &Path, source_root: &str) -> String {
    let inside = relative
        .strip_prefix(source_root)
        .expect("source path must be below its source root");
    let first = inside
        .components()
        .next()
        .expect("Rust source path is not empty");
    let name = first.as_os_str().to_string_lossy();
    name.strip_suffix(".rs").unwrap_or(&name).to_owned()
}

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

fn check_dependencies(root: &Path, sources: &[SourceFile], errors: &mut Vec<String>) {
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

fn normalized(tokens: impl ToTokens) -> String {
    tokens.into_token_stream().to_string()
}

#[derive(Default)]
struct PatternVisitor {
    static_mut: Vec<usize>,
    dense_eight_ranges: Vec<usize>,
    unfinished: Vec<usize>,
}

#[derive(Default)]
struct DisplayCompletionLogVisitor {
    inside_completion: bool,
    violations: Vec<(usize, String)>,
}

impl<'ast> Visit<'ast> for DisplayCompletionLogVisitor {
    fn visit_item_fn(&mut self, item: &'ast ItemFn) {
        let previous = self.inside_completion;
        self.inside_completion = item.sig.ident == "dispatch_display_work";
        syn::visit::visit_item_fn(self, item);
        self.inside_completion = previous;
    }

    fn visit_macro(&mut self, node: &'ast Macro) {
        if self.inside_completion
            && let Some(name) = node
                .path
                .segments
                .last()
                .map(|segment| segment.ident.to_string())
            && matches!(
                name.as_str(),
                "trace" | "debug" | "info" | "warn" | "error" | "print" | "println"
            )
        {
            self.violations.push((node.path.span().start().line, name));
        }
        syn::visit::visit_macro(self, node);
    }
}

impl<'ast> Visit<'ast> for PatternVisitor {
    fn visit_item_static(&mut self, item: &'ast ItemStatic) {
        if matches!(item.mutability, syn::StaticMutability::Mut(_)) {
            self.static_mut.push(item.static_token.span.start().line);
        }
        syn::visit::visit_item_static(self, item);
    }

    fn visit_expr_range(&mut self, range: &'ast ExprRange) {
        let integer = |expression: &Option<Box<Expr>>, expected: u64| {
            expression.as_deref().is_some_and(|expression| {
                matches!(expression, Expr::Lit(ExprLit { lit: Lit::Int(value), .. }) if value.base10_parse::<u64>().is_ok_and(|value| value == expected))
            })
        };
        if integer(&range.start, 0) && integer(&range.end, 8) {
            self.dense_eight_ranges
                .push(range.limits.span().start().line);
        }
        syn::visit::visit_expr_range(self, range);
    }

    fn visit_macro(&mut self, node: &'ast Macro) {
        if node.path.segments.last().is_some_and(|segment| {
            matches!(segment.ident.to_string().as_str(), "todo" | "unimplemented")
        }) {
            self.unfinished.push(node.path.span().start().line);
        }
        syn::visit::visit_macro(self, node);
    }
}

fn check_source_patterns(root: &Path, sources: &[SourceFile], errors: &mut Vec<String>) {
    let banned_text = [
        ("MAX_CORES", "fixed CPU capacity"),
        ("start_all_cores", "firmware-driven secondary startup"),
        ("STDOUT_FILENO", "stdout syscall side path"),
    ];
    for source in sources {
        for (needle, label) in banned_text {
            if source.text.contains(needle) {
                errors.push(format!(
                    "{}: banned pattern reintroduces {label}",
                    source.relative
                ));
            }
        }
        let lowercase = source.text.to_ascii_lowercase();
        if lowercase.contains("read-only ext2")
            || lowercase.contains("read_only ext2")
            || source.text.contains("只读 ext2")
        {
            errors.push(format!(
                "{}: banned pattern reintroduces read-only filesystem dual track",
                source.relative
            ));
        }
        let mut visitor = PatternVisitor::default();
        visitor.visit_file(&source.syntax);
        for line in visitor.static_mut {
            errors.push(format!(
                "{}: static mut global state is forbidden",
                source.at(line)
            ));
        }
        for line in visitor.dense_eight_ranges {
            errors.push(format!(
                "{}: dense eight-hart iteration is forbidden",
                source.at(line)
            ));
        }
        for line in visitor.unfinished {
            errors.push(format!(
                "{}: unfinished executable path is forbidden",
                source.at(line)
            ));
        }
        if source.relative == "kernel/src/drm/device.rs" {
            let mut hot_path = DisplayCompletionLogVisitor::default();
            hot_path.visit_file(&source.syntax);
            for (line, name) in hot_path.violations {
                errors.push(format!(
                    "{}: display completion hot path must not invoke synchronous `{name}!` logging",
                    source.at(line)
                ));
            }
        }
    }

    let gpu_boot = sources
        .iter()
        .find(|source| source.relative == "kernel/src/drivers/virtio_gpu/boot.rs")
        .map(|source| source.text.as_str())
        .unwrap_or_default();
    let drm_mode = sources
        .iter()
        .find(|source| source.relative == "kernel/src/drm/mode.rs")
        .map(|source| source.text.as_str())
        .unwrap_or_default();
    if !gpu_boot.contains("let width = host_width - host_width % 8;")
        || !drm_mode.contains("width.is_multiple_of(H_GRANULARITY)")
    {
        errors.push(
            "display mode must be canonicalized once at the VirtIO boundary and consumed unchanged by DRM"
                .to_owned(),
        );
    }

    let gpu_runtime = sources
        .iter()
        .find(|source| source.relative == "kernel/src/drivers/virtio_gpu.rs")
        .map(|source| source.text.as_str())
        .unwrap_or_default();
    let gpu_damage = sources
        .iter()
        .find(|source| source.relative == "kernel/src/drivers/virtio_gpu/damage.rs")
        .map(|source| source.text.as_str())
        .unwrap_or_default();
    let gpu_resources = sources
        .iter()
        .find(|source| source.relative == "kernel/src/drivers/virtio_gpu/resource.rs")
        .map(|source| source.text.as_str())
        .unwrap_or_default();
    if !gpu_resources.contains("target.synchronized(&control.resources)")
        || gpu_runtime.contains("UnrefReplaced")
        || !gpu_damage.contains("fn publish_next(")
        || !gpu_damage.contains("usize::from(free_descriptors / 4)")
        || !gpu_damage.contains("queue.add_to_avail(command.head)")
        || !gpu_resources.contains("slots: [Option<ResidentResource>; 2]")
        || !gpu_resources.contains("fn release_resident(")
    {
        errors.push(
            "VirtIO-GPU runtime must retain the two-slot framebuffer residency cache, batch independent DIRTYFB transfers with capacity proof, and explicitly release RMFB resources"
                .to_owned(),
        );
    }

    let virtqueue = sources
        .iter()
        .find(|source| source.relative == "kernel/src/drivers/virtio_queue.rs")
        .map(|source| source.text.as_str())
        .unwrap_or_default();
    let ethernet_device = sources
        .iter()
        .find(|source| source.relative == "kernel/src/socket/device.rs")
        .map(|source| source.text.as_str())
        .unwrap_or_default();
    let socket_syscall = sources
        .iter()
        .find(|source| source.relative == "kernel/src/syscall/socket.rs")
        .map(|source| source.text.as_str())
        .unwrap_or_default();
    let inet = sources
        .iter()
        .find(|source| source.relative == "kernel/src/socket/inet.rs")
        .map(|source| source.text.as_str())
        .unwrap_or_default();
    if !virtqueue.contains("Result<u16, VirtQueueError>")
        || virtqueue.contains("failed to translate buffer address")
        || !ethernet_device.contains("pending_error: Cell<Option<NetworkError>>")
        || ethernet_device.contains("Ethernet adapter failed")
        || ethernet_device.contains("unhandled error")
        || !inet.contains("stack.lock().device.take_error()")
        || !socket_syscall.contains("SocketError::Device => errno::EIO")
    {
        errors.push(
            "recoverable VirtIO translation and Ethernet adapter failures must follow typed errors through the socket EIO seam"
                .to_owned(),
        );
    }

    let garbage = [
        "common", "utils", "helpers", "misc", "manager", "base", "shared", "core",
    ];
    for domain in SOURCE_DOMAINS {
        collect_garbage_directories(&root.join(domain.root), root, &garbage, errors);
    }
}

fn check_architecture_boundaries(root: &Path, sources: &[SourceFile], errors: &mut Vec<String>) {
    for source in sources
        .iter()
        .filter(|source| source.relative.starts_with("kernel/src/"))
    {
        if source.owner != "arch"
            && (source.text.contains("riscv::") || source.text.contains("use riscv::{"))
        {
            errors.push(format!(
                "{}: direct RISC-V mechanism is restricted to the arch backend",
                source.relative
            ));
        }
        if !matches!(source.owner.as_str(), "arch" | "platform")
            && source.text.contains("target_arch")
        {
            errors.push(format!(
                "{}: target selection is restricted to static arch/platform facades",
                source.relative
            ));
        }
        if source.owner != "arch" && source.text.contains("crate::arch::riscv64") {
            errors.push(format!(
                "{}: concrete architecture paths may not cross the arch facade",
                source.relative
            ));
        }
        if !matches!(source.owner.as_str(), "arch" | "platform")
            && (source.text.contains("core::arch::asm") || source.text.contains("asm!("))
        {
            errors.push(format!(
                "{}: inline assembly is restricted to the arch backend",
                source.relative
            ));
        }
        if source.owner != "arch"
            && (source.text.contains("RiscvPteFlags") || source.text.contains("PageTableFlags"))
        {
            errors.push(format!(
                "{}: encoded page-table flags may not cross the semantic MMU facade",
                source.relative
            ));
        }
        if source.owner != "platform" && source.text.contains("crate::platform::qemu_virt") {
            errors.push(format!(
                "{}: concrete machine paths may not cross the platform facade",
                source.relative
            ));
        }
        if source.owner != "platform" && source.text.contains("PlatformInfo") {
            errors.push(format!(
                "{}: concrete platform discovery records may not cross the platform facade",
                source.relative
            ));
        }
        if !matches!(
            source.owner.as_str(),
            "arch" | "cpu" | "entry" | "main" | "platform"
        ) && source.text.contains("HardwareCpuId")
        {
            errors.push(format!(
                "{}: hardware CPU identity may not enter generic kernel domains",
                source.relative
            ));
        }
        if source.relative == "kernel/src/main.rs"
            && (source.text.contains("no_mangle") || source.text.contains("extern \"C\""))
        {
            errors.push(
                "kernel/src/main.rs: raw boot/trap ABI must remain behind typed architecture seams"
                    .to_owned(),
            );
        }
        if !matches!(source.owner.as_str(), "arch" | "entry") && source.text.contains("no_mangle") {
            errors.push(format!(
                "{}: raw exported symbols are restricted to architecture/entry codecs",
                source.relative
            ));
        }
        if source.text.contains("dyn Architecture") || source.text.contains("trait Architecture") {
            errors.push(format!(
                "{}: runtime architecture dispatch is forbidden; use the static arch facade",
                source.relative
            ));
        }
    }

    for retired in [
        "kernel/src/arch/riscv64/hart.rs",
        "kernel/src/task/context.rs",
        "kernel/src/task/trap_context.rs",
        "kernel/src/drivers/platform.rs",
    ] {
        if root.join(retired).exists() {
            errors.push(format!(
                "{retired}: retired architecture path must not be restored"
            ));
        }
    }

    let manifest = fs::read_to_string(root.join("kernel/Cargo.toml")).unwrap_or_default();
    let Some(target_dependencies) =
        manifest.find("[target.'cfg(target_arch = \"riscv64\")'.dependencies]")
    else {
        errors.push(
            "kernel/Cargo.toml: RISC-V dependencies require a target-specific table".to_owned(),
        );
        return;
    };
    if manifest[..target_dependencies]
        .lines()
        .any(|line| line.trim_start().starts_with("riscv ="))
    {
        errors.push(
            "kernel/Cargo.toml: riscv crate must not be an unconditional dependency".to_owned(),
        );
    }
}

fn collect_garbage_directories(
    directory: &Path,
    root: &Path,
    garbage: &[&str],
    errors: &mut Vec<String>,
) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| garbage.contains(&name))
        {
            errors.push(format!(
                "{}: directory name has no domain meaning",
                path.strip_prefix(root).unwrap_or(&path).display()
            ));
        }
        collect_garbage_directories(&path, root, garbage, errors);
    }
}

#[derive(Default)]
struct GlobalVisitor {
    lines: Vec<usize>,
}

impl<'ast> Visit<'ast> for GlobalVisitor {
    fn visit_item_static(&mut self, item: &'ast ItemStatic) {
        self.lines.push(item.static_token.span.start().line);
    }
}

fn check_global_owners(sources: &[SourceFile], errors: &mut Vec<String>) {
    for source in sources.iter().filter(|source| source.binary_crate) {
        let mut visitor = GlobalVisitor::default();
        visitor.visit_file(&source.syntax);
        for (index, line) in source.lines.iter().enumerate() {
            if line.contains("static ref ") {
                visitor.lines.push(index + 1);
            }
        }
        visitor.lines.sort_unstable();
        visitor.lines.dedup();
        for line in visitor.lines {
            if !source.preceding_contains(line, 4, "OWNER:") {
                errors.push(format!(
                    "{}: global state lacks an OWNER declaration",
                    source.at(line)
                ));
            }
        }
    }
}

#[derive(Default)]
struct UnsafeVisitor {
    lines: Vec<usize>,
}

impl UnsafeVisitor {
    fn signature(&mut self, signature: &Signature) {
        if let Some(unsafety) = signature.unsafety {
            self.lines.push(unsafety.span.start().line);
        }
    }
}

impl<'ast> Visit<'ast> for UnsafeVisitor {
    fn visit_expr_unsafe(&mut self, expression: &'ast syn::ExprUnsafe) {
        self.lines.push(expression.unsafe_token.span.start().line);
        syn::visit::visit_expr_unsafe(self, expression);
    }

    fn visit_item_impl(&mut self, item: &'ast ItemImpl) {
        if let Some(unsafety) = item.unsafety {
            self.lines.push(unsafety.span.start().line);
        }
        syn::visit::visit_item_impl(self, item);
    }

    fn visit_item_fn(&mut self, item: &'ast ItemFn) {
        self.signature(&item.sig);
        syn::visit::visit_item_fn(self, item);
    }

    fn visit_impl_item_fn(&mut self, item: &'ast ImplItemFn) {
        self.signature(&item.sig);
        syn::visit::visit_impl_item_fn(self, item);
    }

    fn visit_foreign_item_fn(&mut self, item: &'ast ForeignItemFn) {
        self.signature(&item.sig);
        syn::visit::visit_foreign_item_fn(self, item);
    }

    fn visit_item_foreign_mod(&mut self, item: &'ast ItemForeignMod) {
        self.lines.push(item.abi.extern_token.span.start().line);
        syn::visit::visit_item_foreign_mod(self, item);
    }
}

fn check_unsafe_proofs(sources: &[SourceFile], errors: &mut Vec<String>) {
    for source in sources {
        let mut visitor = UnsafeVisitor::default();
        visitor.visit_file(&source.syntax);
        visitor.lines.sort_unstable();
        visitor.lines.dedup();
        for line in visitor.lines {
            if !source.preceding_contains(line, 6, "SAFETY:") {
                errors.push(format!(
                    "{}: unsafe operation lacks a local SAFETY proof",
                    source.at(line)
                ));
            }
        }
    }
}
