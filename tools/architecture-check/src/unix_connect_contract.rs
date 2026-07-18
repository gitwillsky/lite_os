use syn::{Expr, ExprCall, ImplItem, Item, spanned::Spanned, visit::Visit};

use super::SourceFile;

const LIFECYCLE_PATH: &str = "kernel/src/socket/unix/lifecycle.rs";

#[derive(Default)]
struct ConnectOrder {
    begin: Vec<(usize, usize)>,
    resources: Vec<(usize, usize)>,
}

impl<'ast> Visit<'ast> for ConnectOrder {
    fn visit_expr_call(&mut self, call: &'ast ExprCall) {
        let Expr::Path(function) = call.func.as_ref() else {
            syn::visit::visit_expr_call(self, call);
            return;
        };
        let names = function
            .path
            .segments
            .iter()
            .map(|segment| segment.ident.to_string())
            .collect::<Vec<_>>();
        if names.as_slice() == ["ConnectGuard", "begin"] {
            let start = call.span().start();
            self.begin.push((start.line, start.column));
        }
        if names.as_slice() == ["resources"] {
            let start = call.span().start();
            self.resources.push((start.line, start.column));
        }
        syn::visit::visit_expr_call(self, call);
    }
}

/// 固定 AF_UNIX stream connect 的 resource-factory ordering contract。
pub(super) fn check(sources: &[SourceFile], errors: &mut Vec<String>) {
    let Some(source) = sources
        .iter()
        .find(|source| source.relative == LIFECYCLE_PATH)
    else {
        errors.push(format!(
            "missing AF_UNIX connect contract source: {LIFECYCLE_PATH}"
        ));
        return;
    };
    let mut connect_functions = Vec::new();
    for item in &source.syntax.items {
        let Item::Impl(implementation) = item else {
            continue;
        };
        for item in &implementation.items {
            let ImplItem::Fn(function) = item else {
                continue;
            };
            if function.sig.ident == "connect_stream" {
                connect_functions.push(function);
            }
        }
    }
    let [connect] = connect_functions.as_slice() else {
        errors.push(format!(
            "{LIFECYCLE_PATH}: expected exactly one connect_stream implementation"
        ));
        return;
    };
    let mut order = ConnectOrder::default();
    order.visit_block(&connect.block);
    let ([begin], [resources]) = (order.begin.as_slice(), order.resources.as_slice()) else {
        errors.push(format!(
            "{LIFECYCLE_PATH}: connect_stream must call ConnectGuard::begin and resources exactly once"
        ));
        return;
    };
    if begin >= resources {
        errors.push(format!(
            "{LIFECYCLE_PATH}:{}: AF_UNIX resources factory must run after backlog reservation at line {}",
            resources.0, begin.0
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(body: &str) -> SourceFile {
        let text = format!(
            "struct UnixSocket; struct ConnectGuard; impl ConnectGuard {{ fn begin() {{}} }} \
             impl UnixSocket {{ fn connect_stream() {{ {body} }} }}"
        );
        SourceFile {
            relative: LIFECYCLE_PATH.to_owned(),
            owner: "socket".to_owned(),
            lines: text.lines().map(str::to_owned).collect(),
            syntax: syn::parse_file(&text).unwrap(),
            text,
            binary_crate: true,
        }
    }

    #[test]
    fn reservation_must_precede_the_unique_resource_factory_call() {
        let mut errors = Vec::new();
        check(
            &[source("ConnectGuard::begin(); resources();")],
            &mut errors,
        );
        assert!(errors.is_empty(), "{errors:#?}");

        check(
            &[source("resources(); ConnectGuard::begin();")],
            &mut errors,
        );
        assert_eq!(errors.len(), 1, "{errors:#?}");
    }

    #[test]
    fn missing_or_duplicate_calls_fail_closed() {
        let mut errors = Vec::new();
        check(&[source("ConnectGuard::begin();")], &mut errors);
        assert_eq!(errors.len(), 1, "{errors:#?}");

        errors.clear();
        check(
            &[source("ConnectGuard::begin(); resources(); resources();")],
            &mut errors,
        );
        assert_eq!(errors.len(), 1, "{errors:#?}");
    }
}
