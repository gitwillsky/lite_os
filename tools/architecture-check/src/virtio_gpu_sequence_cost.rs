use std::collections::BTreeSet;

use syn::{Expr, ImplItem, Item, ItemImpl, visit::Visit};

use super::SourceFile;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct GpuSequenceCost {
    pub(super) direct_assemblies: usize,
    pub(super) wire_shapes: BTreeSet<String>,
    pub(super) direct_publications: usize,
    pub(super) sequence_submissions: usize,
}

impl GpuSequenceCost {
    fn uses_single_sequence_seam(&self) -> bool {
        self.direct_assemblies == 0
            && self.wire_shapes.is_empty()
            && self.direct_publications == 0
            && self.sequence_submissions <= 1
    }
}

pub(super) fn check(sources: &[SourceFile], errors: &mut Vec<String>) {
    match measure(sources) {
        Ok(cost)
            if cost.uses_single_sequence_seam()
                && runtime_fallback_removed(sources)
                && bounded_multi_command_owner(sources) => {}
        Ok(cost) => errors.push(format!(
            "VirtIO GPU completion must use one sequence seam plus bounded head-mapped command slots; measured {cost:?}"
        )),
        Err(error) => errors.push(error),
    }
}

fn bounded_multi_command_owner(sources: &[SourceFile]) -> bool {
    let adapter = sources
        .iter()
        .find(|source| source.relative == "kernel/src/drivers/virtio_gpu.rs")
        .map(|source| source.text.as_str())
        .unwrap_or_default();
    let timeline = sources
        .iter()
        .find(|source| source.relative == "kernel/src/drm/fence_timeline.rs")
        .map(|source| source.text.as_str())
        .unwrap_or_default();
    adapter.contains("const CONTROL_COMMAND_CAPACITY: usize = QUEUE_SIZE as usize / 2;")
        && adapter.contains("struct CommandSlot")
        && adapter.contains("commands: CommandSlots")
        && adapter.contains("pending: Option<PendingCommand>")
        && adapter.contains("pending.head == head")
        && adapter.contains("fn has_non_render_pending")
        && !adapter.contains("control.pending")
        && timeline.contains("struct FenceTimeline")
        && timeline.contains("while self.count != 0 && self.entries[self.head].complete")
}

fn measure(sources: &[SourceFile]) -> Result<GpuSequenceCost, String> {
    let source = sources
        .iter()
        .find(|source| source.relative == "kernel/src/drivers/virtio_gpu.rs")
        .ok_or_else(|| "kernel/src/drivers/virtio_gpu.rs: missing GPU adapter".to_owned())?;
    let method = source
        .syntax
        .items
        .iter()
        .filter_map(|item| match item {
            Item::Impl(ItemImpl { items, .. }) => Some(items),
            _ => None,
        })
        .flatten()
        .find_map(|item| match item {
            ImplItem::Fn(method) if method.sig.ident == "poll_update" => Some(method),
            _ => None,
        })
        .ok_or_else(|| "kernel/src/drivers/virtio_gpu.rs: missing poll_update".to_owned())?;
    let mut visitor = SequenceVisitor::default();
    visitor.visit_impl_item_fn(method);
    Ok(GpuSequenceCost {
        direct_assemblies: visitor.direct_assemblies,
        wire_shapes: visitor.wire_shapes,
        direct_publications: visitor.direct_publications,
        sequence_submissions: visitor.sequence_submissions,
    })
}

fn runtime_fallback_removed(sources: &[SourceFile]) -> bool {
    let runtime_sources = sources
        .iter()
        .filter(|source| {
            source.relative == "kernel/src/drivers/virtio_gpu.rs"
                || source.relative == "kernel/src/drivers/virtio_gpu/resource.rs"
        })
        .collect::<Vec<_>>();
    let runtime = runtime_sources
        .iter()
        .map(|source| source.text.as_str())
        .collect::<String>();
    let mut visitor = RuntimeAssemblyVisitor::default();
    for source in runtime_sources {
        visitor.visit_file(&source.syntax);
    }
    let command_owner = sources
        .iter()
        .find(|source| source.relative == "kernel/src/drivers/virtio_gpu/command.rs")
        .is_some_and(|source| {
            source.text.contains("enum GpuCommand")
                && source.text.contains("fn prepare(")
                && source.text.contains("PreparedCommand")
        });
    runtime.contains("GpuCommand")
        && runtime.contains("submit_command")
        && !runtime.contains("publish_runtime")
        && visitor.cursor_assemblies == 1
        && visitor.forbidden_direct_assemblies == 0
        && command_owner
}

#[derive(Default)]
struct RuntimeAssemblyVisitor {
    cursor_assemblies: usize,
    forbidden_direct_assemblies: usize,
}

impl<'ast> Visit<'ast> for RuntimeAssemblyVisitor {
    fn visit_expr_call(&mut self, expression: &'ast syn::ExprCall) {
        if let Expr::Path(path) = expression.func.as_ref()
            && let Some(name) = path
                .path
                .segments
                .last()
                .map(|segment| segment.ident.to_string())
            && name.starts_with("prepare_")
        {
            if name == "prepare_cursor" {
                self.cursor_assemblies += 1;
            } else {
                self.forbidden_direct_assemblies += 1;
            }
        }
        syn::visit::visit_expr_call(self, expression);
    }
}

#[derive(Default)]
struct SequenceVisitor {
    direct_assemblies: usize,
    wire_shapes: BTreeSet<String>,
    direct_publications: usize,
    sequence_submissions: usize,
}

impl<'ast> Visit<'ast> for SequenceVisitor {
    fn visit_expr_call(&mut self, expression: &'ast syn::ExprCall) {
        if let Expr::Path(path) = expression.func.as_ref()
            && let Some(name) = path
                .path
                .segments
                .last()
                .map(|segment| segment.ident.to_string())
            && name.starts_with("prepare_")
        {
            self.direct_assemblies += 1;
            self.wire_shapes.insert(name);
        }
        syn::visit::visit_expr_call(self, expression);
    }

    fn visit_expr_method_call(&mut self, expression: &'ast syn::ExprMethodCall) {
        match expression.method.to_string().as_str() {
            "publish_runtime" => self.direct_publications += 1,
            "submit_command" => self.sequence_submissions += 1,
            _ => {}
        }
        syn::visit::visit_expr_method_call(self, expression);
    }
}

#[cfg(test)]
mod tests {
    use super::{bounded_multi_command_owner, measure, runtime_fallback_removed};

    #[test]
    fn poll_update_selects_one_command_and_uses_one_submission_seam() {
        let root = super::super::repository_root();
        let sources = super::super::load_sources(&root).expect("repository sources");
        let cost = measure(&sources).expect("GPU sequence cost");
        assert!(cost.uses_single_sequence_seam(), "measured {cost:?}");
    }

    #[test]
    fn runtime_owns_bounded_head_mapped_commands_and_ordered_public_fences() {
        let root = super::super::repository_root();
        let sources = super::super::load_sources(&root).expect("repository sources");
        assert!(bounded_multi_command_owner(&sources));
    }

    #[test]
    fn runtime_uses_command_owner_except_for_the_independent_cursor_queue() {
        let root = super::super::repository_root();
        let sources = super::super::load_sources(&root).expect("repository sources");
        assert!(runtime_fallback_removed(&sources));
    }
}
