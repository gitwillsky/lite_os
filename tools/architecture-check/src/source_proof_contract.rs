use syn::{
    ForeignItemFn, ImplItemFn, ItemFn, ItemForeignMod, ItemImpl, ItemStatic, Signature,
    visit::Visit,
};

use super::SourceFile;

#[derive(Default)]
struct GlobalVisitor {
    lines: Vec<usize>,
}

impl<'ast> Visit<'ast> for GlobalVisitor {
    fn visit_item_static(&mut self, item: &'ast ItemStatic) {
        self.lines.push(item.static_token.span.start().line);
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

/// @description 检查二进制全局 owner 声明与每个 unsafe/extern 的局部安全证明。
/// @param sources 统一源码快照；errors 接收缺失 owner 或 SAFETY proof 的位置。
/// @return 无；全部违规一次收集。
/// @errors 源码证明缺失均追加到 errors。
pub(super) fn check(sources: &[SourceFile], errors: &mut Vec<String>) {
    check_global_owners(sources, errors);
    check_unsafe_proofs(sources, errors);
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
