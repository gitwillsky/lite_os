use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    path::{Path, PathBuf},
    process::ExitCode,
};

use quote::ToTokens;
use syn::{
    Arm, Expr, ExprLit, ExprMatch, ExprRange, File, ForeignItemFn, ImplItemFn, ItemFn,
    ItemForeignMod, ItemImpl, ItemStatic, ItemUse, Lit, Macro, Pat, PatIdent, Path as SynPath,
    Signature, UseTree, spanned::Spanned, visit::Visit,
};

mod documentation_contract;
mod fallible_collections_contract;
mod ready_contract;
mod source_size;
#[cfg(test)]
mod source_size_tests;
mod terminal_contract;
mod unix_connect_contract;

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
    terminal_contract::check_terminal_contract(&sources, &mut errors);
    unix_connect_contract::check(&sources, &mut errors);
    ready_contract::check(&sources, &mut errors);
    fallible_collections_contract::check(&root, &sources, &mut errors);
    check_global_owners(&sources, &mut errors);
    check_unsafe_proofs(&sources, &mut errors);
    check_abi(&root, &mut errors);
    check_userspace_single_track(&root, &mut errors);
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

fn check_userspace_single_track(root: &Path, errors: &mut Vec<String>) {
    let allowed = BTreeSet::from(["README.md", "base", "console-session", "diagnostics"]);
    let actual = match fs::read_dir(root.join("user")) {
        Ok(entries) => entries
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect::<BTreeSet<_>>(),
        Err(error) => {
            errors.push(format!("failed to inspect user/: {error}"));
            return;
        }
    };
    let expected = allowed
        .into_iter()
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    if actual != expected {
        errors.push(format!(
            "user/: expected the single TUI track {expected:?}, found {actual:?}"
        ));
    }

    for (directory, names) in [
        (
            "base",
            &[
                "busybox.config",
                "group",
                "inittab",
                "liteos.terminfo",
                "network-service",
                "passwd",
                "shutdown",
                "udhcpc.script",
            ][..],
        ),
        ("diagnostics", &["liteos-stress.c"][..]),
    ] {
        let expected = names
            .iter()
            .map(|name| (*name).to_owned())
            .collect::<BTreeSet<_>>();
        let actual = fs::read_dir(root.join("user").join(directory))
            .map(|entries| {
                entries
                    .flatten()
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .collect::<BTreeSet<_>>()
            })
            .unwrap_or_default();
        if actual != expected {
            errors.push(format!(
                "user/{directory}: expected exactly {expected:?}, found {actual:?}"
            ));
        }
    }

    let console = root.join("user/console-session");
    let expected_crate = BTreeSet::from([
        "Cargo.lock".to_owned(),
        "Cargo.toml".to_owned(),
        "src".to_owned(),
    ]);
    let actual_crate = fs::read_dir(&console)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect::<BTreeSet<_>>()
        })
        .unwrap_or_default();
    if actual_crate != expected_crate {
        errors.push(format!(
            "user/console-session: expected exactly {expected_crate:?}, found {actual_crate:?}"
        ));
    }

    let expected_sources = BTreeSet::from([
        "atlas.rs",
        "display.rs",
        "ffi.rs",
        "lib.rs",
        "model.rs",
        "model/parser.rs",
        "model/reflow.rs",
        "model/screen.rs",
        "model/style.rs",
        "reactor.rs",
        "reactor/evdev.rs",
        "reactor/input.rs",
        "reactor/pointer.rs",
        "reactor/session.rs",
    ]);
    let mut source_paths = Vec::new();
    if let Err(error) = rust_files(&console.join("src"), &mut source_paths) {
        errors.push(error);
    }
    let actual_sources = source_paths
        .iter()
        .filter_map(|path| path.strip_prefix(console.join("src")).ok())
        .map(|path| path.to_string_lossy().replace('\\', "/"))
        .collect::<BTreeSet<_>>();
    let expected_sources = expected_sources
        .into_iter()
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    if actual_sources != expected_sources {
        errors.push(format!(
            "user/console-session/src: expected exactly {expected_sources:?}, found {actual_sources:?}"
        ));
    }

    let manifest = fs::read_to_string(console.join("Cargo.toml")).unwrap_or_default();
    for required in [
        "name = \"console-session\"",
        "crate-type = [\"staticlib\"]",
        "panic = \"abort\"",
    ] {
        if !manifest.contains(required) {
            errors.push(format!(
                "user/console-session/Cargo.toml: missing `{required}`"
            ));
        }
    }
    if manifest.contains("[dependencies]") || manifest.contains(" path = ") {
        errors.push(
            "user/console-session: the unique console Module must remain dependency-free"
                .to_owned(),
        );
    }

    let workspace = fs::read_to_string(root.join("Cargo.toml")).unwrap_or_default();
    if !workspace.contains("exclude = [\"bootloader\", \"user/console-session\"]") {
        errors.push(
            "Cargo.toml: bootloader and console-session must be the only excluded Rust crates"
                .to_owned(),
        );
    }

    let inittab = fs::read_to_string(root.join("user/base/inittab")).unwrap_or_default();
    let expected_inittab = "::respawn:/bin/console-session\n::respawn:/etc/init.d/network-service\n::respawn:-/bin/sh\n";
    if inittab != expected_inittab {
        errors.push(
            "user/base/inittab: must supervise console, network and UART recovery exactly once"
                .to_owned(),
        );
    }

    let builder = fs::read_to_string(root.join("scripts/verify_busybox.py")).unwrap_or_default();
    if !builder.contains("def build_console_session(")
        || !builder.contains("/bin/console-session")
        || !builder.contains("user/console-session/src")
        || !builder.contains("/etc/terminfo/l/liteos")
        || [
            "liteui",
            "quickjs",
            "display-session",
            "terminal-service",
            "libseat",
            "libdrm",
        ]
        .iter()
        .any(|marker| builder.contains(marker))
    {
        errors.push(
            "scripts/verify_busybox.py: rootfs must contain only the registered console session track"
                .to_owned(),
        );
    }

    let atlas = fs::read(root.join("assets/fonts/liteos-terminal.a8")).unwrap_or_default();
    if atlas.get(..8) != Some(b"LTA8\0\0\0\x02") || atlas.len() != 481_136 {
        errors.push(
            "assets/fonts/liteos-terminal.a8: expected the checked v2 terminal atlas".to_owned(),
        );
    }
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

fn syscall_constants(file: &File) -> BTreeSet<String> {
    file.items
        .iter()
        .filter_map(|item| match item {
            syn::Item::Const(item) if item.ident.to_string().starts_with("SYSCALL_") => {
                Some(item.ident.to_string())
            }
            _ => None,
        })
        .collect()
}

fn syscall_entries(file: &File) -> BTreeMap<String, usize> {
    file.items
        .iter()
        .filter_map(|item| match item {
            syn::Item::Const(item) if item.ident.to_string().starts_with("SYSCALL_") => {
                let Expr::Lit(ExprLit {
                    lit: Lit::Int(number),
                    ..
                }) = item.expr.as_ref()
                else {
                    return None;
                };
                Some((
                    item.ident
                        .to_string()
                        .trim_start_matches("SYSCALL_")
                        .to_ascii_lowercase(),
                    number
                        .base10_parse()
                        .expect("syscall number must be an integer"),
                ))
            }
            _ => None,
        })
        .collect()
}

#[derive(Default)]
struct DispatchVisitor {
    constants: BTreeSet<String>,
    numeric_arms: Vec<usize>,
}

impl<'ast> Visit<'ast> for DispatchVisitor {
    fn visit_path(&mut self, path: &'ast SynPath) {
        for segment in &path.segments {
            let name = segment.ident.to_string();
            if name.starts_with("SYSCALL_") {
                self.constants.insert(name);
            }
        }
        syn::visit::visit_path(self, path);
    }

    fn visit_arm(&mut self, arm: &'ast Arm) {
        if matches!(
            &arm.pat,
            Pat::Lit(ExprLit {
                lit: Lit::Int(_),
                ..
            })
        ) {
            self.numeric_arms
                .push(arm.fat_arrow_token.spans[0].start().line);
        }
        syn::visit::visit_arm(self, arm);
    }

    fn visit_pat_ident(&mut self, pattern: &'ast PatIdent) {
        let name = pattern.ident.to_string();
        if name.starts_with("SYSCALL_") {
            self.constants.insert(name);
        }
        syn::visit::visit_pat_ident(self, pattern);
    }

    fn visit_expr_match(&mut self, node: &'ast ExprMatch) {
        syn::visit::visit_expr_match(self, node);
    }
}

fn check_abi(root: &Path, errors: &mut Vec<String>) {
    let abi_path = root.join("syscall-abi/src/lib.rs");
    let dispatch_path = root.join("kernel/src/syscall/mod.rs");
    let abi_text = fs::read_to_string(&abi_path).expect("syscall ABI source must exist");
    let dispatch_text = fs::read_to_string(&dispatch_path).expect("syscall dispatch must exist");
    let abi = syn::parse_file(&abi_text).expect("syscall ABI must parse");
    let dispatch = syn::parse_file(&dispatch_text).expect("syscall dispatch must parse");
    let constants = syscall_constants(&abi);
    let mut visitor = DispatchVisitor::default();
    visitor.visit_file(&dispatch);
    for name in constants.difference(&visitor.constants) {
        errors.push(format!("syscall ABI constant is not dispatched: {name}"));
    }
    for name in visitor.constants.difference(&constants) {
        errors.push(format!(
            "dispatcher uses a syscall absent from syscall-abi: {name}"
        ));
    }
    for line in visitor.numeric_arms {
        errors.push(format!(
            "kernel/src/syscall/mod.rs:{line}: raw numeric syscall dispatch is forbidden"
        ));
    }
}
