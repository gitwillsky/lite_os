use std::{fs, path::Path};

use syn::{
    Expr, ExprLit, ExprRange, ItemFn, ItemStatic, Lit, Macro, spanned::Spanned, visit::Visit,
};

use super::{SourceFile, repository_source::source_roots};

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

/// @description 检查跨源码退化模式、单一路径和领域目录命名约束。
/// @param root 用于目录检查；sources 是统一源码快照；errors 接收违规。
/// @return 无；全部违规一次收集。
/// @errors 源码或目录违规均追加到 errors。
pub(super) fn check(root: &Path, sources: &[SourceFile], errors: &mut Vec<String>) {
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
        || !inet.contains("stack.lock()?.device.take_error()")
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
    for source_root in source_roots() {
        collect_garbage_directories(&root.join(source_root), root, &garbage, errors);
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
