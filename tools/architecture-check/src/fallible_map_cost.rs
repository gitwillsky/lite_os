use quote::ToTokens;
use syn::{Expr, visit::Visit};

use super::SourceFile;

const COST_SAMPLE_ENTRIES: usize = 4_096;
const RV64_POINTER_BYTES: usize = 8;
const OLD_ITERATOR_SLOTS: usize = 128;

pub(super) fn check(sources: &[SourceFile], errors: &mut Vec<String>) {
    let mut one_shot = OneShotIterators::default();
    for source in sources {
        one_shot.visit_file(&source.syntax);
    }
    if one_shot.count != 0 {
        let zeroed = COST_SAMPLE_ENTRIES * OLD_ITERATOR_SLOTS * RV64_POINTER_BYTES;
        let comparisons = COST_SAMPLE_ENTRIES * COST_SAMPLE_ENTRIES.ilog2() as usize;
        errors.push(format!(
            "FallibleMap: found {} iter_from/iter_after(...).next() production lookups; the legacy {COST_SAMPLE_ENTRIES}-entry loop cleared at least {zeroed} bytes of RV64 iterator stack and still performs about {comparisons} balanced-tree comparisons; use one persistent traversal or the explicit neighbor/split owner API",
            one_shot.count,
        ));
    }

    let Some(iterator) = sources
        .iter()
        .find(|source| source.relative == "kernel/src/fallible_tree/iter.rs")
    else {
        errors.push("FallibleMap iterator owner disappeared".to_owned());
        return;
    };
    if iterator.text.contains("MAX_HEIGHT")
        || !iterator.text.contains("current: Option<&'a Node<K, V>>")
        || !iterator.text.contains("node.next.map")
    {
        errors.push(format!(
            "{}: iterator must be the single-pointer successor cursor; the old constructor cleared {OLD_ITERATOR_SLOTS} pointer slots ({} bytes on RV64)",
            iterator.relative,
            OLD_ITERATOR_SLOTS * RV64_POINTER_BYTES
        ));
    }

    if let Some(tree) = sources
        .iter()
        .find(|source| source.relative == "kernel/src/fallible_tree.rs")
        && method_tokens(tree, "retain").is_none_or(|body| {
            body.contains("join_with_root")
                || body.contains("join_ordered")
                || !body.contains("retain_linear")
        })
    {
        errors.push(
            "kernel/src/fallible_tree.rs: retain rebuilds at every node through AVL joins; require one linear ownership pass plus one linear balanced rebuild"
                .to_owned(),
        );
    }

    if let Some(graph) = sources
        .iter()
        .find(|source| source.relative == "kernel/src/socket/unix/rights_graph.rs")
        && (method_tokens(graph, "detach").is_none_or(|body| {
            body.contains("self . nodes . retain") || !body.contains("remove_unreferenced")
        }) || !graph.text.contains("references: usize")
            || !graph.text.contains("state.references == 0")
            || !graph.text.contains(".checked_add(state.edges.len())"))
    {
        errors.push(format!(
            "kernel/src/socket/unix/rights_graph.rs: detach scans every graph node; one edge revocation visits all {COST_SAMPLE_ENTRIES} nodes in the cost sample instead of only affected endpoints"
        ));
    }
}

#[derive(Default)]
struct OneShotIterators {
    count: usize,
}

impl<'ast> Visit<'ast> for OneShotIterators {
    fn visit_expr(&mut self, expression: &'ast Expr) {
        if let Expr::MethodCall(next) = expression
            && next.method == "next"
            && is_bounded_iterator_chain(&next.receiver)
        {
            self.count += 1;
        }
        syn::visit::visit_expr(self, expression);
    }
}

fn is_bounded_iterator_chain(expression: &Expr) -> bool {
    let Expr::MethodCall(call) = expression else {
        return false;
    };
    if call.method == "iter_from" || call.method == "iter_after" {
        return true;
    }
    matches!(
        call.method.to_string().as_str(),
        "map" | "filter" | "filter_map" | "copied" | "cloned" | "take" | "take_while"
    ) && is_bounded_iterator_chain(&call.receiver)
}

fn method_tokens(source: &SourceFile, name: &str) -> Option<String> {
    source.syntax.items.iter().find_map(|item| {
        let syn::Item::Impl(implementation) = item else {
            return None;
        };
        implementation.items.iter().find_map(|item| {
            let syn::ImplItem::Fn(method) = item else {
                return None;
            };
            (method.sig.ident == name).then(|| method.block.to_token_stream().to_string())
        })
    })
}
