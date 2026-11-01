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
    fn signature(&mut self, signature: &Signature, attributes: &[syn::Attribute]) {
        // An unsafe API states caller obligations in RustDoc; its body still needs local proofs.
        let documented = attributes.iter().any(|attribute| {
            let syn::Meta::NameValue(meta) = &attribute.meta else { return false };
            let syn::Expr::Lit(literal) = &meta.value else { return false };
            matches!(&literal.lit, syn::Lit::Str(text) if meta.path.is_ident("doc") && text.value().trim() == "# Safety")
        });
        if documented { return; }
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
        self.signature(&item.sig, &item.attrs);
        syn::visit::visit_item_fn(self, item);
    }

    fn visit_impl_item_fn(&mut self, item: &'ast ImplItemFn) {
        self.signature(&item.sig, &item.attrs);
        syn::visit::visit_impl_item_fn(self, item);
    }

    fn visit_foreign_item_fn(&mut self, item: &'ast ForeignItemFn) {
        self.signature(&item.sig, &item.attrs);
        syn::visit::visit_foreign_item_fn(self, item);
    }

    fn visit_item_foreign_mod(&mut self, item: &'ast ItemForeignMod) {
        self.lines.push(item.abi.extern_token.span.start().line);
        syn::visit::visit_item_foreign_mod(self, item);
    }
}

/// 检查二进制全局 owner 声明与每个 unsafe/extern 的局部安全证明。
///
/// # Parameters
///
/// - `sources`: 统一源码快照；errors 接收缺失 owner 或 SAFETY proof 的位置。
///
/// # Returns
///
/// 无；全部违规一次收集。
///
/// # Errors
///
/// 源码证明缺失均追加到 errors。
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rustdoc_proves_the_api_but_not_an_unsafe_body() {
        let syntax = syn::parse_file("/// # Safety\n/// The caller owns the pointer.\nunsafe fn access(p: *const u8) -> u8 { unsafe { *p } }").unwrap();
        let mut visitor = UnsafeVisitor::default();
        visitor.visit_file(&syntax);
        assert_eq!(visitor.lines.len(), 1, "the body still requires a local proof");
        let syntax = syn::parse_file("unsafe fn undocumented() {}").unwrap();
        let mut visitor = UnsafeVisitor::default();
        visitor.visit_file(&syntax);
        assert_eq!(visitor.lines.len(), 1, "undocumented unsafe API must be rejected");
    }
}
